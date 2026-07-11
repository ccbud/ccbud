// ccbud Tauri backend.
//
// Registers the IPC surface consumed by the shared renderer and wires the native runtime:
// config persistence, localhost gateway, Claude integrations, history/usage/export, tray,
// popover, updater, and self-check hooks.
#![allow(unused_variables)]

mod claude;
mod claudedesktop;
mod codex;
mod codexconnect;
mod counttokens;
mod exporthtml;
mod gateway;
mod history;
mod plugin;
mod protocol;
mod store;
mod usage;
mod ziputil;

use serde_json::{json, Value};
use tauri::{Emitter, Manager};

// Timestamp (ms since epoch) of the last popover hide — used to debounce the tray click,
// which would otherwise re-show the popover on the very click that blurred it shut.
static LAST_POPOVER_HIDE_MS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
// Timestamp of the last popover show — a fullscreen app steals focus the instant the popover
// appears, so we ignore blur within a grace window after show (else it hides before being seen).
static LAST_POPOVER_SHOW_MS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---- config / providers (real, store.rs) ----
#[tauri::command]
fn config_get() -> Value {
    store::read_config()
}
/// Last gateway start error (e.g. a bad port the user typed). Surfaced via server:status so the
/// renderer can show the failure banner. Mirrors main.js lastStartError.
static LAST_START_ERROR: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
#[tauri::command]
async fn config_save(
    app: tauri::AppHandle,
    gw: tauri::State<'_, std::sync::Arc<gateway::GatewayState>>,
    cfg: Value,
) -> Result<Value, String> {
    let prev = store::read_config();
    let prev_port = prev.get("port").and_then(|v| v.as_u64()).unwrap_or(8788) as u16;
    let next_port = cfg
        .get("port")
        .and_then(|v| v.as_u64())
        .map(|p| p as u16)
        .unwrap_or(prev_port);
    let was_connected = claude::is_connected(prev_port);
    let codex_was_connected = codexconnect::is_connected(prev_port);
    let prev_dirs = prev.get("historyDirs").cloned();

    // If the gateway is running and the port changed, bind the NEW port BEFORE committing so a bad
    // port can never lock the user out — roll back to the old port and report on failure.
    if next_port != prev_port && gw.current_port().await.is_some() {
        gw.stop().await;
        if let Err(e) = gw.start(next_port).await {
            let _ = gw.start(prev_port).await;
            let msg = format!("端口 {} 启动失败：{}", next_port, e);
            *LAST_START_ERROR.lock().unwrap() = Some(msg.clone());
            gw.emit("gateway:status", full_status(&gw).await);
            return Err(msg);
        }
        *LAST_START_ERROR.lock().unwrap() = None;
    }

    let saved = store::write_config(cfg);
    use tauri_plugin_autostart::ManagerExt;
    let want = saved.get("openAtLogin").and_then(|v| v.as_bool()).unwrap_or(false);
    let mgr = app.autolaunch();
    let _ = if want { mgr.enable() } else { mgr.disable() };

    // Keep each connected CLI's config in sync if connected (port/token may have changed).
    if was_connected || codex_was_connected {
        let port = saved.get("port").and_then(|v| v.as_u64()).unwrap_or(8788) as u16;
        let token = claude::current_token(&saved);
        if was_connected {
            claude::connect(port, &token);
        }
        if codex_was_connected {
            codexconnect::connect(port, &token, &codex_model(&saved));
        }
    }

    // History dirs changed → invalidate + re-warm the usage cache and notify the renderer.
    if saved.get("historyDirs").cloned() != prev_dirs {
        usage::invalidate_cache();
        let cfg2 = saved.clone();
        std::thread::spawn(move || usage::warm_cache(&cfg2, "all"));
        let _ = app.emit("history:changed", json!({ "files": [] }));
    }

    update_tray_title(&app);
    gw.emit("gateway:status", full_status(&gw).await);
    Ok(saved)
}
#[tauri::command]
fn provider_upsert(p: Value) -> Value {
    let mut cfg = store::read_config();
    let mut provider = p;
    let pid = provider.get("id").and_then(|v| v.as_str()).map(|s| s.to_string());
    {
        let provs = cfg["providers"].as_array_mut().unwrap();
        match pid {
            Some(id) if !id.is_empty() => {
                if let Some(i) = provs
                    .iter()
                    .position(|x| x.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
                {
                    provs[i] = provider;
                } else {
                    provs.push(provider);
                }
            }
            _ => {
                let id = store::gen_id();
                provider
                    .as_object_mut()
                    .unwrap()
                    .insert("id".into(), json!(id.clone()));
                provs.push(provider);
                if cfg["activeProviderId"].is_null() {
                    cfg["activeProviderId"] = json!(id);
                }
            }
        }
    }
    store::write_config(cfg)
}
#[tauri::command]
fn provider_delete(id: String) -> Value {
    let mut cfg = store::read_config();
    let kept: Vec<Value> = cfg["providers"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|p| p.get("id").and_then(|v| v.as_str()) != Some(id.as_str()))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    cfg["providers"] = json!(kept);
    if cfg["activeProviderId"].as_str() == Some(id.as_str()) {
        cfg["activeProviderId"] = cfg["providers"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(|p| p.get("id").cloned())
            .unwrap_or(Value::Null);
    }
    store::write_config(cfg)
}
#[tauri::command]
fn provider_set_active(id: String) -> Value {
    let mut cfg = store::read_config();
    cfg["activeProviderId"] = json!(id);
    store::write_config(cfg)
}

// ---- plugins (sidecar coding-agent backends, see plugin.rs) ----
type PluginState<'a> = tauri::State<'a, std::sync::Arc<plugin::PluginManager>>;

/// List discovered plugins with running + auth status.
#[tauri::command]
async fn plugin_list(pm: PluginState<'_>) -> Result<Value, String> {
    Ok(pm.list().await)
}
/// Single plugin status snapshot.
#[tauri::command]
async fn plugin_status(pm: PluginState<'_>, id: String) -> Result<Value, String> {
    Ok(pm.status(&id).await)
}
/// Enable (spawn + health-gate + register provider) or disable (stop + unregister) a plugin.
#[tauri::command]
async fn plugin_set_enabled(pm: PluginState<'_>, id: String, enabled: bool) -> Result<Value, String> {
    if enabled {
        pm.start(&id).await?;
    } else {
        pm.stop(&id)?;
    }
    Ok(pm.status(&id).await)
}
/// Start a plugin login (forwarded to the plugin's control plane).
#[tauri::command]
async fn plugin_auth_login(pm: PluginState<'_>, id: String) -> Result<Value, String> {
    pm.auth_login(&id).await
}
/// Clear a plugin's cached login.
#[tauri::command]
async fn plugin_auth_logout(pm: PluginState<'_>, id: String) -> Result<Value, String> {
    pm.auth_logout(&id).await
}
/// Run a plugin-declared UI action: forward form `values` to its control plane.
#[tauri::command]
async fn plugin_action(pm: PluginState<'_>, id: String, action: String, values: Value) -> Result<Value, String> {
    pm.action(&id, &action, values).await
}
/// Prefill a plugin action form with the plugin's current values.
#[tauri::command]
async fn plugin_action_load(pm: PluginState<'_>, id: String, action: String) -> Result<Value, String> {
    pm.action_load(&id, &action).await
}
/// Add a plugin: pick a local folder containing plugin.json and install it.
#[tauri::command]
async fn plugin_install(pm: PluginState<'_>) -> Result<Value, String> {
    let picked = rfd::AsyncFileDialog::new()
        .set_title("选择插件目录（内含 plugin.json）")
        .pick_folder()
        .await;
    let dir = match picked {
        Some(f) => f.path().to_path_buf(),
        None => return Ok(json!({ "canceled": true })),
    };
    let id = pm.install(&dir)?;
    Ok(json!({ "ok": true, "id": id }))
}
/// Remove a plugin after a native confirm: stop it, drop its provider, delete its files.
#[tauri::command]
async fn plugin_uninstall(pm: PluginState<'_>, id: String) -> Result<Value, String> {
    let res = rfd::AsyncMessageDialog::new()
        .set_level(rfd::MessageLevel::Warning)
        .set_title("删除插件")
        .set_description(format!(
            "确定删除插件「{}」？将停用它、移除对应服务，并删除其安装目录。",
            id
        ))
        .set_buttons(rfd::MessageButtons::OkCancel)
        .show()
        .await;
    if !matches!(res, rfd::MessageDialogResult::Ok) {
        return Ok(json!({ "canceled": true }));
    }
    pm.uninstall(&id)?;
    Ok(json!({ "ok": true }))
}
/// Open the plugins folder in the OS file browser.
#[tauri::command]
fn plugin_open_dir() -> bool {
    let dir = plugin::plugins_root();
    let _ = std::fs::create_dir_all(&dir);
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(&dir).spawn().is_ok()
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer").arg(&dir).spawn().is_ok()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        std::process::Command::new("xdg-open").arg(&dir).spawn().is_ok()
    }
}
/// Install a plugin from a git repository (clone + build + install). Runs the
/// blocking git/build work off the async runtime.
#[tauri::command]
async fn plugin_install_git(pm: PluginState<'_>, url: String) -> Result<Value, String> {
    let mgr = pm.inner().clone();
    let id = tokio::task::spawn_blocking(move || mgr.install_from_git(&url))
        .await
        .map_err(|e| e.to_string())??;
    Ok(json!({ "ok": true, "id": id }))
}
/// Check whether a plugin's git source has a newer version.
#[tauri::command]
async fn plugin_check_update(pm: PluginState<'_>, id: String) -> Result<Value, String> {
    Ok(pm.check_update(&id).await)
}
/// Update a plugin from its recorded git source (re-clone + build + replace).
#[tauri::command]
async fn plugin_update(pm: PluginState<'_>, id: String) -> Result<Value, String> {
    let mgr = pm.inner().clone();
    let id = tokio::task::spawn_blocking(move || mgr.update(&id))
        .await
        .map_err(|e| e.to_string())??;
    Ok(json!({ "ok": true, "id": id }))
}
/// Build the upstream `/v1/messages` URL from a provider baseUrl.
/// Live connection test: POST a tiny ping to the provider, shaped for its declared wire protocol
/// (Anthropic /v1/messages, OpenAI /chat/completions, or /responses), and report ok/error/timeout.
/// The renderer localizes the result message.
#[tauri::command]
async fn provider_test(p: Value) -> Value {
    let base = p.get("baseUrl").and_then(|v| v.as_str()).unwrap_or("").trim();
    if base.is_empty() {
        return json!({ "ok": false, "reason": "baseUrlEmpty" });
    }
    if !(base.starts_with("http://") || base.starts_with("https://")) {
        return json!({ "ok": false, "reason": "baseUrlInvalid" });
    }
    // Test against the provider's DECLARED protocol endpoint — an openai-chat provider must be
    // pinged at /chat/completions with a Chat body, not the Anthropic /v1/messages default.
    let wire = crate::protocol::Wire::from_provider(p.get("protocol").and_then(|v| v.as_str()));
    let url = wire.upstream_url(base);
    let model = p
        .get("defaultModel")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            p.get("models")
                .and_then(|m| m.as_array())
                .and_then(|a| a.first())
                .and_then(|m| m.get("upstream"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or("claude-3-5-haiku-20241022")
        .to_string();
    let token = p.get("authToken").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let insecure = store::read_config()
        .get("insecureSkipVerify")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    // Protocol-shaped ping body.
    let body = match wire {
        crate::protocol::Wire::OpenAiResponses => json!({ "model": model, "max_output_tokens": 16, "input": "ping" }),
        crate::protocol::Wire::OpenAiChat => json!({ "model": model, "max_tokens": 16, "messages": [{ "role": "user", "content": "ping" }] }),
        crate::protocol::Wire::Anthropic => json!({ "model": model, "max_tokens": 16, "messages": [{ "role": "user", "content": "ping" }] }),
    };
    let client = match reqwest::Client::builder()
        .danger_accept_invalid_certs(insecure)
        .timeout(std::time::Duration::from_secs(30))
        .build()
    {
        Ok(c) => c,
        Err(e) => return json!({ "ok": false, "message": e.to_string() }),
    };
    // Auth via Authorization: Bearer only. Sending both authorization and x-api-key trips
    // providers that reject having the two auth headers present at once.
    let mut rb = client
        .post(&url)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", token));
    if wire == crate::protocol::Wire::Anthropic {
        rb = rb.header("anthropic-version", "2023-06-01");
    }
    let resp = rb.json(&body).send().await;
    match resp {
        Ok(r) => {
            let status = r.status().as_u16();
            let text = r.text().await.unwrap_or_default();
            let parsed: Option<Value> = serde_json::from_str(&text).ok();
            let http_ok = (200..300).contains(&status);
            // A well-shaped reply for the tested protocol: Anthropic `type:message`, Chat `choices`,
            // Responses `output`/`id`.
            let shape_ok = parsed.as_ref().map(|j| match wire {
                crate::protocol::Wire::Anthropic => j.get("type").and_then(|v| v.as_str()) == Some("message"),
                crate::protocol::Wire::OpenAiChat => j.get("choices").map(|c| c.is_array()).unwrap_or(false),
                crate::protocol::Wire::OpenAiResponses => j.get("output").is_some() || j.get("id").is_some(),
            }).unwrap_or(false);
            if http_ok && shape_ok {
                let m = parsed
                    .as_ref()
                    .and_then(|j| j.get("model"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(&model);
                return json!({ "ok": true, "status": status, "model": m });
            }
            let msg = parsed
                .as_ref()
                .and_then(|j| j.get("error"))
                .and_then(|e| e.get("message"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    if !text.is_empty() {
                        text.chars().take(200).collect()
                    } else {
                        format!("HTTP {}", status)
                    }
                });
            json!({ "ok": false, "status": status, "message": msg })
        }
        Err(e) => {
            if e.is_timeout() {
                json!({ "ok": false, "reason": "timeout" })
            } else {
                json!({ "ok": false, "message": e.to_string() })
            }
        }
    }
}

// ---- claude code / desktop integration ----
/// The literal selected CLIs from config `connectTargets` (subset of {claude, codex}, deduped).
/// Empty is a valid state ("nothing connected") — the hero Connect button substitutes a default.
fn connect_targets(cfg: &Value) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    if let Some(a) = cfg.get("connectTargets").and_then(|v| v.as_array()) {
        for v in a {
            if let Some(s) = v.as_str() {
                if (s == "claude" || s == "codex") && !out.iter().any(|x| x == s) {
                    out.push(s.to_string());
                }
            }
        }
    }
    out
}

/// The model written into Codex's config: the fixed sentinel "gpt-5.5-ccbud". Codex derives its
/// model family from the name — a foreign name (e.g. "z-ai/glm-5.2") makes it warn about an
/// unknown/degraded model on every launch, while a gpt-5.5-prefixed one is accepted silently.
/// The gateway routes any "*-ccbud" sentinel to the active provider's primary model.
fn codex_model(_cfg: &Value) -> String {
    "gpt-5.5-ccbud".to_string()
}

/// Make each CLI's config file match the selected `connectTargets`: write the selected ones to
/// point at the gateway, restore the rest. PURELY a config-file operation — the gateway service
/// itself is an independent switch (`gatewayEnabled`), never started or stopped from here.
fn apply_connections(cfg: &Value) {
    let selected = connect_targets(cfg);
    let claude_on = selected.iter().any(|t| t == "claude");
    let codex_on = selected.iter().any(|t| t == "codex");
    let port = cfg.get("port").and_then(|v| v.as_u64()).unwrap_or(8788) as u16;
    let token = claude::current_token(cfg);
    if claude_on {
        claude::connect(port, &token);
    } else {
        claude::disconnect();
    }
    if codex_on {
        codexconnect::connect(port, &token, &codex_model(cfg));
    } else {
        codexconnect::disconnect();
    }
}

#[tauri::command]
async fn claude_connect(
    app: tauri::AppHandle,
    gw: tauri::State<'_, std::sync::Arc<gateway::GatewayState>>,
) -> Result<Value, String> {
    let mut cfg = store::read_config();
    let n = cfg.get("providers").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
    if n == 0 {
        return Ok(json!({ "ok": false, "reason": "noProvider" }));
    }
    // Hero "一键接入" with nothing selected connects Claude Code by default (and persists it, so the
    // toggle reflects it).
    if connect_targets(&cfg).is_empty() {
        cfg["connectTargets"] = json!(["claude"]);
        cfg = store::write_config(cfg);
    }
    apply_connections(&cfg);
    let status = full_status(&gw).await;
    gw.emit("gateway:status", status);
    refresh_tray_menu(&app);
    Ok(json!({ "ok": true }))
}
#[tauri::command]
async fn claude_disconnect(
    app: tauri::AppHandle,
    gw: tauri::State<'_, std::sync::Arc<gateway::GatewayState>>,
) -> Result<Value, String> {
    // Master off: restore BOTH CLIs' config files (idempotent). The gateway service keeps its own
    // switch — removing the CLI wiring doesn't stop it.
    claude::disconnect();
    codexconnect::disconnect();
    let status = full_status(&gw).await;
    gw.emit("gateway:status", status);
    refresh_tray_menu(&app);
    Ok(json!({ "ok": true }))
}

/// Independent gateway-service switch: persist `gatewayEnabled` and start/stop the localhost
/// server. CLI config files are untouched — connect/disconnect is a separate, config-only action.
#[tauri::command]
async fn gateway_set_enabled(
    app: tauri::AppHandle,
    gw: tauri::State<'_, std::sync::Arc<gateway::GatewayState>>,
    on: bool,
) -> Result<Value, String> {
    let mut cfg = store::read_config();
    cfg["gatewayEnabled"] = json!(on);
    let saved = store::write_config(cfg);
    if on {
        let port = saved.get("port").and_then(|v| v.as_u64()).unwrap_or(8788) as u16;
        if let Err(e) = gw.start(port).await {
            let msg = format!("port {} failed: {}", port, e);
            *LAST_START_ERROR.lock().unwrap() = Some(msg.clone());
            gw.emit("gateway:status", full_status(&gw).await);
            refresh_tray_menu(&app);
            return Ok(json!({ "ok": false, "reason": "portFailed", "message": msg }));
        }
        *LAST_START_ERROR.lock().unwrap() = None;
    } else {
        gw.stop().await;
    }
    let status = full_status(&gw).await;
    gw.emit("gateway:status", status);
    refresh_tray_menu(&app);
    Ok(json!({ "ok": true }))
}

/// Live per-CLI switch: flip one target on/off, persist the selection, and immediately write or
/// restore that CLI's config file. Config-only — the gateway service has its own switch.
#[tauri::command]
async fn set_connect_target(
    app: tauri::AppHandle,
    gw: tauri::State<'_, std::sync::Arc<gateway::GatewayState>>,
    target: String,
    on: bool,
) -> Result<Value, String> {
    let mut cfg = store::read_config();
    if on && cfg.get("providers").and_then(|v| v.as_array()).map(|a| a.is_empty()).unwrap_or(true) {
        return Ok(json!({ "ok": false, "reason": "noProvider" }));
    }
    let mut targets = connect_targets(&cfg);
    targets.retain(|t| t != &target);
    if on && (target == "claude" || target == "codex") {
        targets.push(target.clone());
    }
    cfg["connectTargets"] = json!(targets);
    let saved = store::write_config(cfg);
    apply_connections(&saved);
    let status = full_status(&gw).await;
    gw.emit("gateway:status", status);
    refresh_tray_menu(&app);
    Ok(json!({ "ok": true }))
}
#[tauri::command]
async fn desktop_status(
    gw: tauri::State<'_, std::sync::Arc<gateway::GatewayState>>,
) -> Result<Value, String> {
    let port = gw
        .current_port()
        .await
        .unwrap_or_else(|| store::read_config().get("port").and_then(|v| v.as_u64()).unwrap_or(8788) as u16);
    Ok(claudedesktop::status(port))
}
#[tauri::command]
async fn desktop_connect(
    gw: tauri::State<'_, std::sync::Arc<gateway::GatewayState>>,
) -> Result<Value, String> {
    let cfg = store::read_config();
    let port = cfg.get("port").and_then(|v| v.as_u64()).unwrap_or(8788) as u16;
    if cfg.get("providers").and_then(|v| v.as_array()).map(|a| a.is_empty()).unwrap_or(true) {
        return Ok(json!({ "ok": false, "reason": "noProvider" }));
    }
    let _ = gw.start(port).await;
    Ok(claudedesktop::connect(port, &claude::current_token(&cfg)))
}
#[tauri::command]
fn desktop_disconnect() -> Value {
    claudedesktop::disconnect()
}
fn pct(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{:02X}", b),
        })
        .collect()
}
#[tauri::command]
fn desktop_replay(file: String, prompt: Option<String>) -> Value {
    if file.is_empty() {
        return json!({ "ok": false, "reason": "noFile" });
    }
    if !cfg!(target_os = "macos") {
        return json!({ "ok": false, "reason": "unsupported" });
    }
    // The full review prompt comes from the renderer's i18n (desktop.replayPrompt) so it stays
    // localized; fall back to a minimal default only if the renderer didn't supply one.
    let prompt = prompt
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "请基于这些对话记录在 Claude 桌面版里继续。".to_string());
    // Attach the main session AND every subagent transcript (they live in a separate subagents/ dir),
    // each as its own `file=` — the Cowork deep link honors repeated `file=` — so the analysis covers
    // subagent runs, not just the main thread.
    let mut url = format!("claude://cowork/new?q={}&file={}", pct(&prompt), pct(&file));
    for sub in history::subagent_transcript_paths(&file) {
        url.push_str("&file=");
        url.push_str(&pct(&sub));
    }
    #[cfg(target_os = "macos")]
    {
        let ok = std::process::Command::new("/usr/bin/open").arg(&url).spawn().is_ok();
        json!({ "ok": ok })
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = url;
        json!({ "ok": false, "reason": "unsupported" })
    }
}

// ---- server / usage / monitor / logs ----
async fn full_status(gw: &std::sync::Arc<gateway::GatewayState>) -> Value {
    let mut s = gw.status().await;
    let port = gw
        .current_port()
        .await
        .unwrap_or_else(|| store::read_config().get("port").and_then(|v| v.as_u64()).unwrap_or(8788) as u16);
    if let Some(o) = s.as_object_mut() {
        let claude_on = claude::is_connected(port);
        let codex_on = codexconnect::is_connected(port);
        // `connected` = any CLI wired to the gateway (drives the tray "已接入" indicator).
        o.insert("connected".into(), json!(claude_on || codex_on));
        o.insert("connectedClaude".into(), json!(claude_on));
        o.insert("connectedCodex".into(), json!(codex_on));
        o.insert("codexAvailable".into(), json!(codexconnect::is_available()));
        o.insert(
            "gatewayEnabled".into(),
            json!(store::read_config().get("gatewayEnabled").and_then(|v| v.as_bool()).unwrap_or(true)),
        );
        o.insert(
            "lastStartError".into(),
            LAST_START_ERROR
                .lock()
                .ok()
                .and_then(|g| g.clone())
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        o.insert("claudePath".into(), json!(claude::settings_path().to_string_lossy()));
    }
    s
}
#[tauri::command]
async fn server_status(
    gw: tauri::State<'_, std::sync::Arc<gateway::GatewayState>>,
) -> Result<Value, String> {
    Ok(full_status(&gw).await)
}
#[tauri::command]
fn usage_get(range: Option<String>) -> Value {
    let t = std::time::Instant::now();
    let cfg = store::read_config();
    // Usage surfaces (popover heatmap/stats, hero) always aggregate EVERY configured dir — the
    // conversations-page directory switcher must not silently filter the calendar down to one CLI.
    let r = usage::usage_get(&cfg, "all", range.as_deref().unwrap_or("7d"));
    eprintln!(
        "[TIMING] usage_get(range={}) {}ms",
        range.as_deref().unwrap_or("7d"),
        t.elapsed().as_millis()
    );
    r
}

/// Compact token count (mirror of usage.js `formatTokens`): 1234→"1.2K", 4.9e9→"4.9B".
fn format_tokens(n: i64) -> String {
    let n = n.max(0);
    if n < 1000 {
        return n.to_string();
    }
    let strip = |s: String| s.strip_suffix(".0").map(|p| p.to_string()).unwrap_or(s);
    if n < 1_000_000 {
        let v = n as f64 / 1e3;
        let s = if n < 10_000 { format!("{:.1}", v) } else { format!("{:.0}", v) };
        return format!("{}K", strip(s));
    }
    if n < 1_000_000_000 {
        let v = n as f64 / 1e6;
        let s = if n < 10_000_000 { format!("{:.1}", v) } else { format!("{:.0}", v) };
        return format!("{}M", strip(s));
    }
    let v = n as f64 / 1e9;
    format!("{}B", strip(format!("{:.1}", v)))
}

#[cfg(test)]
mod fmt_tests {
    use super::format_tokens;
    #[test]
    fn matches_js_format_tokens() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1000), "1K");
        assert_eq!(format_tokens(1234), "1.2K");
        assert_eq!(format_tokens(9999), "10K");
        assert_eq!(format_tokens(12_345), "12K");
        assert_eq!(format_tokens(1_000_000), "1M");
        assert_eq!(format_tokens(4_900_000), "4.9M");
        assert_eq!(format_tokens(12_000_000), "12M");
        assert_eq!(format_tokens(1_000_000_000), "1B");
        assert_eq!(format_tokens(4_892_112_447), "4.9B");
    }
}

/// Set the macOS menu-bar tray title to the configured usage token count (or clear it when
/// trayUsage is off). Heavy work (config read + usage scan) runs on the caller's thread;
/// only the set_title call hops to the main thread, where macOS requires UI mutation.
fn update_tray_title(app: &tauri::AppHandle) {
    let config = store::read_config();
    let tu = config.get("trayUsage").cloned().unwrap_or_else(|| json!({}));
    let enabled = tu.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
    let title: Option<String> = if enabled {
        let range = tu.get("range").and_then(|v| v.as_str()).unwrap_or("7d").to_string();
        // Same global scope as the popover — the tray count is a whole-machine number.
        let tokens = usage::usage_get(&config, "all", &range)
            .get("tokens")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        Some(format!(" {}", format_tokens(tokens)))
    } else {
        None
    };
    let app2 = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Some(tray) = app2.tray_by_id("main") {
            let _ = tray.set_title(title.as_deref());
        }
    });
}

// ---- system tray: dynamic, localized context menu (parity with main.js buildTrayMenu) ----
struct TrayLabels {
    running_with: &'static str,
    stopped: &'static str,
    open_main: &'static str,
    stop_gw: &'static str,
    start_gw: &'static str,
    quit: &'static str,
    check_updates: &'static str,
}
fn tray_labels(lang: &str) -> TrayLabels {
    match lang {
        // config.language stores "zh" (store.rs normalize) — accept both spellings.
        "zh" | "zh-CN" => TrayLabels { running_with: "● 网关运行中 · {name}", stopped: "○ 网关已停止", open_main: "打开主界面", stop_gw: "停止网关服务", start_gw: "启动网关服务", quit: "退出 ccbud", check_updates: "检查更新…" },
        "zh-TW" => TrayLabels { running_with: "● 閘道執行中 · {name}", stopped: "○ 閘道已停止", open_main: "開啟主視窗", stop_gw: "停止閘道服務", start_gw: "啟動閘道服務", quit: "結束 ccbud", check_updates: "檢查更新…" },
        "ja" => TrayLabels { running_with: "● ゲートウェイ稼働中 · {name}", stopped: "○ ゲートウェイ停止中", open_main: "メインウィンドウを開く", stop_gw: "ゲートウェイを停止", start_gw: "ゲートウェイを起動", quit: "ccbud を終了", check_updates: "更新を確認…" },
        "ko" => TrayLabels { running_with: "● 게이트웨이 실행 중 · {name}", stopped: "○ 게이트웨이 중지됨", open_main: "메인 창 열기", stop_gw: "게이트웨이 중지", start_gw: "게이트웨이 시작", quit: "ccbud 종료", check_updates: "업데이트 확인…" },
        _ => TrayLabels { running_with: "● Gateway running · {name}", stopped: "○ Gateway stopped", open_main: "Open main window", stop_gw: "Stop gateway service", start_gw: "Start gateway service", quit: "Quit ccbud", check_updates: "Check for updates…" },
    }
}
fn config_lang(config: &Value) -> String {
    config.get("language").and_then(|v| v.as_str()).unwrap_or("en").to_string()
}
fn active_provider_name(config: &Value) -> String {
    let id = match config.get("activeProviderId").and_then(|v| v.as_str()) {
        Some(i) => i,
        None => return String::new(),
    };
    config
        .get("providers")
        .and_then(|v| v.as_array())
        .and_then(|arr| {
            arr.iter()
                .find(|p| p.get("id").and_then(|v| v.as_str()) == Some(id))
                .and_then(|p| p.get("name").and_then(|v| v.as_str()))
        })
        .unwrap_or("")
        .to_string()
}
fn build_tray_menu(
    app: &tauri::AppHandle,
    running: bool,
    provider: &str,
    lang: &str,
) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
    let l = tray_labels(lang);
    let status_txt = if running {
        let name = if provider.is_empty() { "ccbud" } else { provider };
        l.running_with.replace("{name}", name)
    } else {
        l.stopped.to_string()
    };
    // Status row is disabled (it's an indicator, like main.js { enabled: false }).
    let status_i = MenuItem::with_id(app, "tray_status", status_txt, false, None::<&str>)?;
    let open_i = MenuItem::with_id(app, "tray_open", l.open_main, true, None::<&str>)?;
    let conn_i = if running {
        MenuItem::with_id(app, "tray_gw_stop", l.stop_gw, true, None::<&str>)?
    } else {
        MenuItem::with_id(app, "tray_gw_start", l.start_gw, true, None::<&str>)?
    };
    let check_i = MenuItem::with_id(app, "tray_check", l.check_updates, true, None::<&str>)?;
    let quit_i = MenuItem::with_id(app, "tray_quit", l.quit, true, None::<&str>)?;
    let sep1 = PredefinedMenuItem::separator(app)?;
    let sep2 = PredefinedMenuItem::separator(app)?;
    Menu::with_items(app, &[&status_i, &sep1, &open_i, &conn_i, &check_i, &sep2, &quit_i])
}
/// Rebuild the tray menu to reflect the gateway service state + locale + active provider.
fn refresh_tray_menu(app: &tauri::AppHandle) {
    let app2 = app.clone();
    let _ = app.run_on_main_thread(move || {
        let config = store::read_config();
        let running = app2
            .try_state::<std::sync::Arc<gateway::GatewayState>>()
            .map(|s| s.port_sync().is_some())
            .unwrap_or(false);
        let provider = active_provider_name(&config);
        let lang = config_lang(&config);
        if let Ok(menu) = build_tray_menu(&app2, running, &provider, &lang) {
            if let Some(tray) = app2.tray_by_id("main") {
                let _ = tray.set_menu(Some(menu));
            }
        }
    });
}
#[tauri::command]
async fn monitor_get(
    gw: tauri::State<'_, std::sync::Arc<gateway::GatewayState>>,
    id: Value,
) -> Result<Value, String> {
    let idn = id.as_i64().or_else(|| id.as_str().and_then(|s| s.parse().ok())).unwrap_or(-1);
    Ok(gw.monitor_get(idn).await)
}
#[tauri::command]
async fn monitor_clear(
    gw: tauri::State<'_, std::sync::Arc<gateway::GatewayState>>,
) -> Result<Value, String> {
    gw.monitor_clear().await;
    Ok(json!(true))
}
#[tauri::command]
fn logs_get(gw: tauri::State<'_, std::sync::Arc<gateway::GatewayState>>) -> Value {
    gw.logs_snapshot()
}
#[tauri::command]
fn logs_clear(gw: tauri::State<'_, std::sync::Arc<gateway::GatewayState>>) -> Value {
    gw.logs_clear();
    Value::Null
}

// ---- window / app lifecycle ----
/// macOS Dock icon follows the main window: Regular (Dock shown) while a window is open,
/// Accessory (menu-bar only) when it's closed. The popover floats over fullscreen apps via its
/// NSPanel regardless of this policy, so showing the Dock icon with the main window is safe.
fn set_dock_visible(app: &tauri::AppHandle, visible: bool) {
    #[cfg(target_os = "macos")]
    {
        let app2 = app.clone();
        let _ = app.run_on_main_thread(move || {
            let policy = if visible {
                tauri::ActivationPolicy::Regular
            } else {
                tauri::ActivationPolicy::Accessory
            };
            let _ = app2.set_activation_policy(policy);
        });
    }
}
#[tauri::command]
fn app_open_main(app: tauri::AppHandle) -> Value {
    if let Some(win) = app.get_webview_window("main") {
        set_dock_visible(&app, true);
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
    Value::Null
}
#[tauri::command]
fn app_quit(app: tauri::AppHandle) -> Value {
    app.exit(0);
    Value::Null
}
#[tauri::command] fn window_settings_mode(on: bool) -> Value { Value::Null }
#[tauri::command]
fn window_view_min_width(app: tauri::AppHandle, w: i64) -> Value {
    if let Some(win) = app.get_webview_window("main") {
        let min_w = std::cmp::max(600, if w > 0 { w } else { 900 }) as f64;
        let _ = win.set_min_size(Some(tauri::Size::Logical(tauri::LogicalSize::new(min_w, 600.0))));
    }
    Value::Null
}

// ---- conversation history ----
#[tauri::command]
fn history_projects() -> Value {
    let cfg = store::read_config();
    let active = cfg.get("historyActive").and_then(|v| v.as_str()).unwrap_or("all").to_string();
    json!(history::list_projects(&cfg, &active))
}
#[tauri::command]
fn history_list() -> Value {
    let cfg = store::read_config();
    let active = cfg.get("historyActive").and_then(|v| v.as_str()).unwrap_or("all").to_string();
    json!(history::list_sessions(&cfg, &active, 400))
}
#[tauri::command]
fn history_get(file: String) -> Value {
    history::get_session(&file)
}
#[tauri::command]
async fn history_search(query: String) -> Result<Value, String> {
    let cfg = store::read_config();
    let active = cfg.get("historyActive").and_then(|v| v.as_str()).unwrap_or("all").to_string();
    // Content scan is read/parse heavy — keep it off the IPC thread so the UI stays responsive.
    tauri::async_runtime::spawn_blocking(move || json!(history::search_sessions(&cfg, &active, &query, 120)))
        .await
        .map_err(|e| e.to_string())
}
#[tauri::command]
fn history_dirs() -> Value {
    let cfg = store::read_config();
    let active = cfg.get("historyActive").and_then(|v| v.as_str()).unwrap_or("all").to_string();
    json!({ "dirs": history::dir_stats(&cfg), "active": active })
}
#[tauri::command]
async fn history_pick_dir() -> Result<Value, String> {
    let folder = rfd::AsyncFileDialog::new().set_title("选择工作目录").pick_folder().await;
    match folder {
        // Return the picked path (home-collapsed to `~/…`) and let the renderer persist it
        // via saveConfig, matching the renderer contract.
        Some(f) => {
            let mut picked = f.path().to_path_buf();
            // If the user drilled into a data subdir (projects/ = Claude, sessions/ = Codex),
            // store its parent (the work dir) so both trees are probed correctly.
            let name = picked.file_name().and_then(|n| n.to_str()).map(|s| s.to_string());
            if matches!(name.as_deref(), Some("projects") | Some("sessions"))
                && !picked.join(name.as_deref().unwrap()).is_dir()
            {
                if let Some(parent) = picked.parent() {
                    picked = parent.to_path_buf();
                }
            }
            let path = store::collapse_home(&picked.to_string_lossy());
            Ok(json!({ "ok": true, "path": path }))
        }
        None => Ok(json!({ "ok": false, "canceled": true })),
    }
}
#[tauri::command]
fn history_set_active(app: tauri::AppHandle, id: String) -> Value {
    let mut cfg = store::read_config();
    cfg["historyActive"] = json!(if id.is_empty() { "all".to_string() } else { id });
    let saved = store::write_config(cfg);
    let _ = app.emit(
        "history:changed",
        json!({ "files": [], "active": saved.get("historyActive").cloned().unwrap_or(json!("all")) }),
    );
    saved
}
#[tauri::command]
async fn history_import(app: tauri::AppHandle) -> Result<Value, String> {
    match rfd::AsyncFileDialog::new().add_filter("对话记录 (.jsonl / .zip)", &["jsonl", "zip"]).set_title("导入对话记录").pick_files().await {
        Some(files) => {
            let paths: Vec<String> = files.iter().map(|f| f.path().to_string_lossy().to_string()).collect();
            let r = history::import_paths(&paths);
            let _ = app.emit("history:changed", json!({ "files": [] }));
            Ok(r)
        }
        None => Ok(json!({ "canceled": true })),
    }
}
#[tauri::command]
fn history_import_paths(app: tauri::AppHandle, paths: Value) -> Value {
    let list: Vec<String> = paths
        .as_array()
        .map(|a| a.iter().filter_map(|p| p.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
    let r = history::import_paths(&list);
    let _ = app.emit("history:changed", json!({ "files": [] }));
    r
}
#[tauri::command]
fn history_remove_import(app: tauri::AppHandle, file: String) -> Value {
    let r = history::remove_import(&file);
    if r.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        let _ = app.emit("history:changed", json!({ "files": [] }));
    }
    r
}
#[tauri::command]
fn history_set_meta(app: tauri::AppHandle, file: String, patch: Value) -> Value {
    let cfg = store::read_config();
    let r = history::set_ccbud(&file, &patch, &cfg);
    if r.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        let _ = app.emit("history:changed", json!({ "files": [file] }));
    }
    r
}
#[tauri::command]
fn history_delete_forever(app: tauri::AppHandle, file: String) -> Value {
    let cfg = store::read_config();
    let r = history::delete_session_file(&file, &cfg);
    if r.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        let _ = app.emit("history:changed", json!({ "files": [] }));
    }
    r
}
#[tauri::command]
async fn history_export_raw(file: String) -> Result<Value, String> {
    let base = exporthtml::export_base_name(&file);
    // A session with subagents exports as a .zip bundle (main .jsonl at the top level + subagents/);
    // a plain session stays a verbatim .jsonl. import_paths accepts either.
    if history::session_has_subagents(&file) {
        let bytes = history::export_bundle(&file).map_err(|e| e.to_string())?;
        match rfd::AsyncFileDialog::new()
            .add_filter("ZIP", &["zip"])
            .set_file_name(format!("{}.zip", base))
            .save_file()
            .await
        {
            Some(d) => {
                let p = d.path().to_path_buf();
                std::fs::write(&p, bytes).map_err(|e| e.to_string())?;
                Ok(json!({ "canceled": false, "path": p.to_string_lossy(), "bundled": true }))
            }
            None => Ok(json!({ "canceled": true })),
        }
    } else {
        let data = std::fs::read_to_string(&file).map_err(|e| e.to_string())?;
        match rfd::AsyncFileDialog::new()
            .add_filter("JSONL", &["jsonl"])
            .set_file_name(format!("{}.jsonl", base))
            .save_file()
            .await
        {
            Some(d) => {
                let p = d.path().to_path_buf();
                std::fs::write(&p, data).map_err(|e| e.to_string())?;
                Ok(json!({ "canceled": false, "path": p.to_string_lossy(), "bundled": false }))
            }
            None => Ok(json!({ "canceled": true })),
        }
    }
}
#[tauri::command]
async fn history_export_html(payload: Value) -> Result<Value, String> {
    let file = payload
        .get("file")
        .and_then(|v| v.as_str())
        .or_else(|| payload.as_str())
        .ok_or("no file")?
        .to_string();
    // Build the export data once, then reuse it for both the HTML body and the filename.
    let data = exporthtml::build_data(&file);
    let html = exporthtml::html_from_data(&data);
    let base = exporthtml::export_base_name_from_data(&data);
    match rfd::AsyncFileDialog::new().set_file_name(format!("{}.html", base)).save_file().await {
        Some(d) => {
            let p = d.path().to_path_buf();
            std::fs::write(&p, html).map_err(|e| e.to_string())?;
            // Open the freshly-exported viewer in the user's default browser (issue #7).
            open_path_native(&p);
            Ok(json!({ "canceled": false, "path": p.to_string_lossy() }))
        }
        None => Ok(json!({ "canceled": true })),
    }
}

// ---- utilities ----
#[tauri::command]
fn util_copy(text: String) -> bool {
    match arboard::Clipboard::new() {
        Ok(mut cb) => cb.set_text(text).is_ok(),
        Err(_) => false,
    }
}
#[tauri::command]
fn util_open_external(url: String) -> bool {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return false;
    }
    let spawned = {
        #[cfg(target_os = "macos")]
        {
            std::process::Command::new("open").arg(&url).spawn()
        }
        #[cfg(target_os = "windows")]
        {
            std::process::Command::new("cmd").args(["/C", "start", "", &url]).spawn()
        }
        #[cfg(target_os = "linux")]
        {
            std::process::Command::new("xdg-open").arg(&url).spawn()
        }
    };
    spawned.is_ok()
}

// Open a local file with the OS default handler. Used to pop the freshly-exported HTML viewer in
// the user's browser so they don't have to hunt for it in the filesystem. Best-effort: a spawn
// failure must not fail the export.
fn open_path_native(path: &std::path::Path) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(path).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd").args(["/C", "start", ""]).arg(path).spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
}

// ---- in-app updates ----
// In-app update state, mapped to the shape the renderer's about/update pane expects
// (runningVersion / latestVersion / mode / pending). Tauri's updater is in-app full → mode "hot".
static UPDATE_LATEST: std::sync::Mutex<Option<(String, Option<String>)>> =
    std::sync::Mutex::new(None);
static UPDATE_CHECKED: std::sync::Mutex<bool> = std::sync::Mutex::new(false);
static UPDATE_STAGED: std::sync::Mutex<bool> = std::sync::Mutex::new(false);
// A download is in flight (manual or auto) — second caller gets "busy" instead of a duplicate.
static UPDATE_DOWNLOADING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
// Daily auto-check bookkeeping: the day (local YYYY-MM-DD) whose auto check already completed
// (in-memory mirror of the on-disk stamp), an in-flight guard, and the last attempt time so a
// failed attempt (offline) is retried on a later visibility change instead of on every focus.
static AUTO_UPDATE_DONE_DAY: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
static AUTO_UPDATE_RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static AUTO_UPDATE_LAST_TRY_MS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
const AUTO_UPDATE_RETRY_MS: i64 = 10 * 60 * 1000;

/// Clears an in-flight flag on drop, so a panic/unwind or an early return can never leave
/// UPDATE_DOWNLOADING / AUTO_UPDATE_RUNNING stuck true for the rest of the process.
struct FlagGuard(&'static std::sync::atomic::AtomicBool);
impl Drop for FlagGuard {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

fn build_update_state(app: &tauri::AppHandle) -> Value {
    let cfg = store::read_config();
    let current = app.package_info().version.to_string();
    let latest = UPDATE_LATEST.lock().ok().and_then(|g| g.clone());
    let checked = UPDATE_CHECKED.lock().map(|g| *g).unwrap_or(false);
    let staged = UPDATE_STAGED.lock().map(|g| *g).unwrap_or(false);
    let (latest_v, notes, mode) = match (&latest, checked) {
        (Some((v, n)), _) => (json!(v), n.clone().map(Value::String).unwrap_or(Value::Null), "hot"),
        (None, true) => (Value::Null, Value::Null, "none"),
        (None, false) => (Value::Null, Value::Null, "unknown"),
    };
    json!({
        "ok": true,
        "runningVersion": current,
        "shellVersion": current,
        "latestVersion": latest_v,
        "mode": mode,
        "notes": notes,
        "pending": if staged {
            json!({ "staged": true, "version": latest.as_ref().map(|(v, _)| v.clone()) })
        } else {
            Value::Null
        },
        "installMethod": "tauri",
        "autoUpdate": cfg.get("autoUpdate").cloned().unwrap_or(json!({ "check": true, "autoDownload": true })),
    })
}
#[tauri::command]
fn update_state(app: tauri::AppHandle) -> Value {
    build_update_state(&app)
}
/// Hit the updater endpoint and sync UPDATE_CHECKED/UPDATE_LATEST + the renderer's
/// update:state. Shared by the manual update_check command and the daily auto check.
async fn run_update_check(app: &tauri::AppHandle) -> Result<Option<tauri_plugin_updater::Update>, String> {
    use tauri_plugin_updater::UpdaterExt;
    *UPDATE_CHECKED.lock().unwrap() = true;
    let result = match app.updater() {
        Ok(updater) => updater.check().await,
        Err(e) => Err(e),
    };
    match result {
        Ok(found) => {
            *UPDATE_LATEST.lock().unwrap() =
                found.as_ref().map(|u| (u.version.clone(), u.body.clone()));
            let _ = app.emit("update:state", build_update_state(app));
            Ok(found)
        }
        Err(e) => Err(e.to_string()),
    }
}
#[tauri::command]
async fn update_check(app: tauri::AppHandle) -> Result<Value, String> {
    match run_update_check(&app).await {
        Ok(_) => Ok(build_update_state(&app)),
        Err(e) => Ok(json!({
            "ok": false,
            "error": e,
            "runningVersion": app.package_info().version.to_string(),
        })),
    }
}
/// Download + stage the available update (restart applies it). Shared by the manual
/// update_download command and the daily auto flow; UPDATE_DOWNLOADING dedupes the two.
async fn run_update_download(app: &tauri::AppHandle) -> Result<Value, String> {
    use tauri_plugin_updater::UpdaterExt;
    if UPDATE_DOWNLOADING.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return Err("busy".to_string());
    }
    let _busy = FlagGuard(&UPDATE_DOWNLOADING);
    let updater = app.updater().map_err(|e| e.to_string())?;
    match updater.check().await.map_err(|e| e.to_string())? {
        Some(u) => {
            u.download_and_install(|_chunk, _total| {}, || {}).await.map_err(|e| e.to_string())?;
            *UPDATE_STAGED.lock().unwrap() = true;
            let st = build_update_state(app);
            let _ = app.emit("update:staged", st.clone());
            let _ = app.emit("update:state", st.clone());
            Ok(st)
        }
        None => Ok(json!({ "ok": true, "mode": "none" })),
    }
}
#[tauri::command]
async fn update_download(app: tauri::AppHandle) -> Result<Value, String> {
    run_update_download(&app).await
}
#[tauri::command]
fn update_apply(app: tauri::AppHandle) -> Value {
    app.restart();
}
#[tauri::command]
fn update_set_auto(patch: Value) -> Value {
    let mut cfg = store::read_config();
    let mut au = cfg.get("autoUpdate").cloned().unwrap_or(json!({ "check": true, "autoDownload": true }));
    if let Some(o) = au.as_object_mut() {
        if let Some(c) = patch.get("check") {
            o.insert("check".into(), c.clone());
        }
        if let Some(d) = patch.get("autoDownload") {
            o.insert("autoDownload".into(), d.clone());
        }
    }
    cfg["autoUpdate"] = au.clone();
    store::write_config(cfg);
    au
}

// ---- daily auto update (first time the app becomes visible each day) ----
// The stamp lives in its own tiny file (NOT config.json) so the daily writer never races the
// renderer's whole-config round-trips through config_save.
fn auto_update_stamp_file() -> std::path::PathBuf {
    store::ccbud_home().join("update-check.json")
}
fn today_local() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}
fn last_auto_update_day() -> String {
    std::fs::read_to_string(auto_update_stamp_file())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.get("lastAutoCheckDay").and_then(|d| d.as_str()).map(|s| s.to_string()))
        .unwrap_or_default()
}
fn mark_auto_update_day(day: &str) {
    if let Ok(mut g) = AUTO_UPDATE_DONE_DAY.lock() {
        *g = Some(day.to_string());
    }
    let _ = std::fs::create_dir_all(store::ccbud_home());
    let _ = std::fs::write(
        auto_update_stamp_file(),
        serde_json::to_vec(&json!({ "lastAutoCheckDay": day })).unwrap_or_default(),
    );
}

// Native restart prompt after an auto-downloaded update (localized like tray_labels — the main
// window may be hidden when the popover triggered the check, so this can't live in the renderer).
struct UpdatePromptLabels {
    title: &'static str,
    body: &'static str, // {v} → new version
    restart: &'static str,
    later: &'static str,
}
fn update_prompt_labels(lang: &str) -> UpdatePromptLabels {
    match lang {
        "zh" | "zh-CN" => UpdatePromptLabels { title: "更新已就绪", body: "新版本 {v} 已自动下载完成。是否立即重启以应用新版本？", restart: "立即重启", later: "稍后" },
        "zh-TW" => UpdatePromptLabels { title: "更新已就緒", body: "新版本 {v} 已自動下載完成。要立即重新啟動以套用新版本嗎？", restart: "立即重啟", later: "稍後" },
        "ja" => UpdatePromptLabels { title: "アップデートの準備ができました", body: "新しいバージョン {v} のダウンロードが完了しました。今すぐ再起動して適用しますか？", restart: "今すぐ再起動", later: "後で" },
        "ko" => UpdatePromptLabels { title: "업데이트 준비 완료", body: "새 버전 {v} 다운로드가 완료되었습니다. 지금 다시 시작하여 적용할까요?", restart: "지금 다시 시작", later: "나중에" },
        _ => UpdatePromptLabels { title: "Update ready", body: "Version {v} has been downloaded. Restart now to switch to the new version?", restart: "Restart now", later: "Later" },
    }
}
async fn prompt_restart_to_apply(app: &tauri::AppHandle) {
    let version = UPDATE_LATEST
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|(v, _)| v.clone()))
        .unwrap_or_default();
    let l = update_prompt_labels(&config_lang(&store::read_config()));
    // On Linux this shells out to zenity; without it rfd logs an error and returns Cancel,
    // degrading to the staged update applying on the next launch (About pane shows "restart").
    let res = rfd::AsyncMessageDialog::new()
        .set_level(rfd::MessageLevel::Info)
        .set_title(l.title)
        .set_description(l.body.replace("{v}", &version))
        .set_buttons(rfd::MessageButtons::OkCancelCustom(l.restart.to_string(), l.later.to_string()))
        .show()
        .await;
    if matches!(&res, rfd::MessageDialogResult::Custom(s) if s == l.restart) {
        app.restart();
    }
}

/// Called from every "app became visible" site (main window focus, popover show, launch).
/// The first such moment each day — with autoUpdate.check on — runs one update check; when an
/// update exists and autoUpdate.autoDownload is on it's downloaded, then the user is asked
/// whether to restart into the new version (declining leaves it staged for the next launch).
/// The day is stamped only after a flow that reached the network succeeds, so an offline
/// launch doesn't burn the day's only attempt — the next visibility (≥10 min later) retries.
fn auto_update_on_visible(app: &tauri::AppHandle) {
    let today = today_local();
    if AUTO_UPDATE_DONE_DAY
        .lock()
        .map(|g| g.as_deref() == Some(today.as_str()))
        .unwrap_or(false)
    {
        return;
    }
    if last_auto_update_day() == today {
        // Stamped by a previous run of this process instance or a crashed one — mirror it.
        if let Ok(mut g) = AUTO_UPDATE_DONE_DAY.lock() {
            *g = Some(today);
        }
        return;
    }
    if now_ms() - AUTO_UPDATE_LAST_TRY_MS.load(std::sync::atomic::Ordering::Relaxed) < AUTO_UPDATE_RETRY_MS {
        return;
    }
    if AUTO_UPDATE_RUNNING.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let running = FlagGuard(&AUTO_UPDATE_RUNNING);
    let au = store::read_config().get("autoUpdate").cloned().unwrap_or_else(|| json!({}));
    if !au.get("check").and_then(|v| v.as_bool()).unwrap_or(true) {
        return; // `running` drops here and clears the flag
    }
    AUTO_UPDATE_LAST_TRY_MS.store(now_ms(), std::sync::atomic::Ordering::Relaxed);
    let auto_dl = au.get("autoDownload").and_then(|v| v.as_bool()).unwrap_or(true);
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let _running = running; // held until the task ends (cleared even on panic/unwind)
        match run_update_check(&app).await {
            Ok(None) => mark_auto_update_day(&today),
            Ok(Some(_)) => {
                let staged = UPDATE_STAGED.lock().map(|g| *g).unwrap_or(false);
                if staged {
                    // Downloaded on an earlier day but never restarted — just re-ask.
                    mark_auto_update_day(&today);
                    prompt_restart_to_apply(&app).await;
                } else if !auto_dl {
                    mark_auto_update_day(&today); // surfaced in the About pane only
                } else {
                    match run_update_download(&app).await {
                        Ok(_) => {
                            mark_auto_update_day(&today);
                            if UPDATE_STAGED.lock().map(|g| *g).unwrap_or(false) {
                                prompt_restart_to_apply(&app).await;
                            }
                        }
                        // A manual download is already in flight — the user took over today's
                        // update (the About pane drives the rest), so the day is done.
                        Err(e) if e == "busy" => mark_auto_update_day(&today),
                        Err(_) => {} // download failed → day left unstamped so a later visibility retries
                    }
                }
            }
            Err(_) => {} // check failed (offline?) → retry on a later visibility
        }
    });
}

// ---- debug self-check (gated by CCBUD_SELFCHECK env; injected via on_page_load) ----
#[tauri::command]
fn selfcheck_report(report: Value) {
    let line = serde_json::to_string(&report).unwrap_or_default();
    eprintln!("[SELFCHECK] {}", line);
    // Also append to a file when CCBUD_SELFCHECK_OUT is set — a GUI-session run
    // (open .app via launchd) has no terminal-attached stderr to read.
    if let Ok(path) = std::env::var("CCBUD_SELFCHECK_OUT") {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(f, "{}", line);
        }
    }
}
#[tauri::command]
fn selfcheck_routing() -> Value {
    gateway::routing_selftest()
}
#[tauri::command]
fn selfcheck_history() -> Value {
    history::history_selftest(&store::ccbud_home())
}
#[tauri::command]
fn selfcheck_desktop() -> Value {
    let p = claudedesktop::build_profile(18799, "tok");
    json!({
        "hasBaseUrl": p.contains("http://localhost:18799"),
        "hasProvider": p.contains("inferenceProvider") && p.contains("gateway"),
        "hasModels": p.contains("claude-sonnet-5") && p.contains("anthropicFamilyTier") && p.contains("isFamilyDefault"),
        "hasBundleId": p.contains("com.anthropic.claudefordesktop"),
        "validXml": p.starts_with("<?xml") && p.contains("</plist>"),
    })
}
#[tauri::command]
fn selfcheck_import() -> Value {
    history::import_selftest(&store::ccbud_home())
}
#[tauri::command]
fn selfcheck_export() -> Value {
    let base = store::ccbud_home();
    let _ = history::history_selftest(&base);
    let file = base.join("test-claude").join("projects").join("-test-cwd").join("sess1.jsonl");
    let html = exporthtml::build_export_html(&file.to_string_lossy());
    json!({
        "len": html.len(),
        "hasConv": html.contains("__CONV__"),
        "hasContent": html.contains("hello world from selfcheck"),
        "hasSkin": html.contains("</style>"),
        "embedded": html.len() > 180000,
        "validHtml": html.starts_with("<!doctype html") && html.contains("</html>"),
    })
}
#[tauri::command]
fn selfcheck_popover(app: tauri::AppHandle) -> Value {
    let pop = match app.get_webview_window("popover") {
        Some(p) => p,
        None => return json!({ "err": "no popover window" }),
    };
    let mon = match pop.current_monitor() {
        Ok(Some(m)) => m,
        _ => return json!({ "err": "no monitor" }),
    };
    let scale = mon.scale_factor();
    let pw = (424.0 * scale) as i32;
    let sx = mon.position().x;
    let sy = mon.position().y;
    let sw = mon.size().width as i32;
    let sh = mon.size().height as i32;
    // Simulate a tray icon at the top-right of the menu bar, run the same placement
    // math as the real tray click, then read back where the window actually lands.
    let tray_cx = sx + sw - (12.0 * scale) as i32;
    let x = (tray_cx - pw / 2).clamp(sx + 4, sx + sw - pw - 4);
    let y = sy + (26.0 * scale) as i32;
    // macOS window ops must run on the main thread — the real tray callback already
    // does; here we hop onto it explicitly and read back inside the same closure so
    // the probe sees the post-move geometry without a cross-thread timing race.
    let (tx, rx) = std::sync::mpsc::channel();
    let pop2 = pop.clone();
    let _ = app.run_on_main_thread(move || {
        let _ = pop2.show();
        let _ = pop2.set_position(tauri::PhysicalPosition::new(x, y));
        let pos = pop2.outer_position().ok().map(|p| (p.x, p.y));
        let size = pop2.outer_size().ok().map(|s| (s.width as i32, s.height as i32));
        let _ = pop2.hide();
        let _ = tx.send((pos, size));
    });
    let (pos, size) = rx
        .recv_timeout(std::time::Duration::from_millis(1500))
        .unwrap_or((None, None));
    let in_screen = match (pos, size) {
        (Some((px, py)), Some((sw2, sh2))) => {
            px >= sx && py >= sy && (px + sw2) <= (sx + sw + 2) && (py + sh2) <= (sy + sh + 2)
        }
        _ => false,
    };
    json!({
        "scale": scale,
        "monitor": [sx, sy, sw, sh],
        "computed": [x, y],
        "popPos": pos.map(|(a, b)| json!([a, b])),
        "popSize": size.map(|(a, b)| json!([a, b])),
        "inScreen": in_screen,
    })
}
#[tauri::command]
async fn selfcheck_gateway(
    gw: tauri::State<'_, std::sync::Arc<gateway::GatewayState>>,
) -> Result<Value, String> {
    // Mutates config (writes a mock provider) — only ever allowed in a throwaway self-check run.
    if std::env::var("CCBUD_SELFCHECK").is_err() {
        return Err("selfcheck disabled".into());
    }
    let port = gw.current_port().await.unwrap_or(0);
    let mut r = gateway::gateway_selftest(port).await;
    let sse_ex = gw.monitor_recent().await; // last recorded by gateway_selftest = the SSE exchange
    // Exercise HEAD / (mock 404 → gateway fallback 200 → recorded) to verify monitor detail + ms.
    let head_status = reqwest::Client::new()
        .head(format!("http://127.0.0.1:{}/", port))
        .send()
        .await
        .map(|x| x.status().as_u16())
        .unwrap_or(0);
    tokio::time::sleep(std::time::Duration::from_millis(60)).await;
    let head_ex = gw.monitor_recent().await; // now the HEAD exchange
    if let Some(o) = r.as_object_mut() {
        let req_ok = sse_ex.get("reqBody").and_then(|b| b.get("text")).and_then(|t| t.as_str()).map(|s| !s.is_empty()).unwrap_or(false);
        let res_ok = sse_ex.get("resBody").and_then(|b| b.get("text")).and_then(|t| t.as_str()).map(|s| !s.is_empty()).unwrap_or(false);
        let redacted = sse_ex.get("reqHeaders").map(|h| h.to_string().contains("已隐藏")).unwrap_or(false);
        o.insert("monitorReqBody".into(), json!(req_ok));
        o.insert("monitorResBody".into(), json!(res_ok));
        o.insert("monitorRedacted".into(), json!(redacted));
        o.insert("recordHasMs".into(), json!(sse_ex.get("ms").map(|v| v.is_number()).unwrap_or(false)));
        o.insert("headStatus".into(), json!(head_status));
        o.insert(
            "headMonitored".into(),
            json!(head_ex.get("method").and_then(|m| m.as_str()) == Some("HEAD")
                && head_ex.get("reqHeaders").map(|h| h.is_object()).unwrap_or(false)
                && head_ex.get("ms").map(|v| v.is_number()).unwrap_or(false)),
        );
    }
    Ok(r)
}
const SELFCHECK_JS: &str = r#"
(function(){
  if (window.__ccbud_sc) return; window.__ccbud_sc = 1;
  window.__ccbud_errors = [];
  window.addEventListener('error', function(e){ try{window.__ccbud_errors.push(String((e&&e.message)||(e&&e.error)||e));}catch(_){} }, true);
  window.addEventListener('unhandledrejection', function(e){ try{window.__ccbud_errors.push('promise:'+String((e.reason&&e.reason.message)||e.reason));}catch(_){} });
  function rep(o){ try{ window.__TAURI__.core.invoke('selfcheck_report',{report:o}); }catch(_){} }
  setTimeout(async function(){
    var o={};
    try{
      o.hasCcbud=!!window.ccbud;
      o.hasTauri=!!(window.__TAURI__&&window.__TAURI__.core);
      o.bodyLen=(document.body&&document.body.innerHTML.length)||0;
      o.navItems=document.querySelectorAll('.nav-item,[data-view],[data-nav]').length;
      o.colorMix=!!(window.CSS&&CSS.supports&&CSS.supports('color','color-mix(in srgb,red,blue)'));
      o.highlight=!!(window.CSS&&CSS.highlights);
      // store round-trip — self-check runs point CCBUD_HOME at a throwaway dir
      try{
        var before=await window.ccbud.getConfig();
        o.provBefore=((before&&before.providers)||[]).length;
        var saved=await window.ccbud.upsertProvider({name:'SelfTest',baseUrl:'https://x.test',authToken:'tok',defaultModel:'m1',smallFastModel:'m1',extra:'shouldDrop'});
        o.provAfter=((saved&&saved.providers)||[]).length;
        o.savedName=saved&&saved.providers&&saved.providers[0]&&saved.providers[0].name;
        o.savedHasId=!!(saved&&saved.providers&&saved.providers[0]&&saved.providers[0].id);
        o.savedActiveMatches=!!(saved&&saved.activeProviderId&&saved.providers[0]&&saved.activeProviderId===saved.providers[0].id);
        o.droppedExtra=!(saved&&saved.providers&&saved.providers[0]&&('extra' in saved.providers[0]));
        var reread=await window.ccbud.getConfig();
        o.rereadProv=((reread&&reread.providers)||[]).length;
      }catch(e){ o.storeErr=String(e); }
      try{ o.routing=await window.__TAURI__.core.invoke('selfcheck_routing'); }catch(e){ o.routingErr=String(e); }
      try{ o.server=await window.ccbud.serverStatus(); }catch(e){ o.serverErr=String(e); }
      try{ o.gateway=await window.__TAURI__.core.invoke('selfcheck_gateway'); }catch(e){ o.gatewayErr=String(e); }
      try{
        o.histDirs=(await window.ccbud.historyDirs()).dirs.length;
        var hl=await window.ccbud.historyList();
        o.histCount=(hl||[]).length;
        o.histSample=hl&&hl[0]?{title:String(hl[0].title||'').slice(0,40),project:hl[0].project,hasCwd:!!hl[0].cwd,hasFile:!!hl[0].file}:null;
        if(hl&&hl[0]){ var ss=await window.ccbud.historyGet(hl[0].file); o.histMsgs=ss&&ss.messages?ss.messages.length:-1; o.histTotals=ss&&ss.meta?ss.meta.totals:null; }
      }catch(e){ o.histErr=String(e); }
      try{ var ug=await window.ccbud.usageGet('all'); o.usage={tokens:ug.tokens,requests:ug.requests,fav:ug.favoriteModel,heatmap:(ug.heatmap||[]).length,byModel:(ug.byModel||[]).length,activeDays:ug.activeDays}; }catch(e){ o.usageErr=String(e); }
      try{ var cc=await window.ccbud.connect(); var s1=await window.ccbud.serverStatus(); var dd=await window.ccbud.disconnect(); var s2=await window.ccbud.serverStatus(); o.claude={connOk:cc&&cc.ok,connected:s1.connected,discOk:dd&&dd.ok,afterDisc:s2.connected}; }catch(e){ o.claudeErr=String(e); }
      try{ o.copyOk=await window.ccbud.copy('selfcheck-clip'); }catch(e){ o.copyErr=String(e); }
      try{ o.histMeta=await window.__TAURI__.core.invoke('selfcheck_history'); }catch(e){ o.histMetaErr=String(e); }
      try{ o.desktop=await window.__TAURI__.core.invoke('selfcheck_desktop'); }catch(e){ o.desktopErr=String(e); }
      try{ o.export=await window.__TAURI__.core.invoke('selfcheck_export'); }catch(e){ o.exportErr=String(e); }
      try{ o.import=await window.__TAURI__.core.invoke('selfcheck_import'); }catch(e){ o.importErr=String(e); }
      try{ var us=await window.ccbud.updateState(); var sa=await window.ccbud.updateSetAuto({check:false}); o.update={current:us.current,status:us.status,setAutoCheck:sa.check}; }catch(e){ o.updateErr=String(e); }
      try{ o.drag={regions:document.querySelectorAll('.drag-region').length,wired:document.querySelectorAll('[data-tauri-drag-region]').length}; }catch(e){ o.dragErr=String(e); }
      try{ var cs=getComputedStyle(document.body); o.userSelect=cs.webkitUserSelect||cs.userSelect; }catch(e){}
      try{ var ep=document.getElementById('endpoint'); var eb=document.getElementById('exportBlock'); o.epSel=ep?getComputedStyle(ep).webkitUserSelect:'-'; o.ebSel=eb?getComputedStyle(eb).webkitUserSelect:'-'; }catch(e){}
      try{ o.popoverPos=await window.__TAURI__.core.invoke('selfcheck_popover'); }catch(e){ o.popoverPosErr=String(e); }
      o.errors=window.__ccbud_errors.slice(0,20);
    }catch(e){o.fatal=String((e&&e.stack)||e);}
    rep(o);
  },2200);
})();
"#;

const POPOVER_SELFCHECK_JS: &str = r#"
(function(){
  setTimeout(async function(){
    var o={win:"popover"};
    try{ o.hasCcbud=!!window.ccbud; var u=await window.ccbud.usageGet("all"); o.usageTokens=u?u.tokens:"null"; o.heatmapLen=u&&u.heatmap?u.heatmap.length:-1; o.heatmapFilled=u&&u.heatmap?u.heatmap.filter(function(c){return c.level>0;}).length:-1; }catch(e){ o.usageErr=String(e); }
    try{ var st=document.getElementById("sTokens"); o.sTokensText=st?st.textContent:"noel"; var hm=document.getElementById("heatmap"); o.heatCells=hm?hm.children.length:-1; }catch(e){}
    try{
      o.innerW=window.innerWidth; o.innerH=window.innerHeight; o.scrollH=document.body.scrollHeight;
      var st2=document.getElementById("sTokens"); if(st2){var r=st2.getBoundingClientRect(); o.sTokTop=Math.round(r.top); o.sTokVisible=(r.top>=0&&r.bottom<=window.innerHeight);}
      var hm2=document.getElementById("heatmap"); if(hm2){var hr=hm2.getBoundingClientRect(); o.hmTop=Math.round(hr.top); o.hmBottom=Math.round(hr.bottom);}
      o.bodyBg=getComputedStyle(document.body).backgroundColor;
      var root=document.querySelector(".pop-body-root"); o.rootBg=root?getComputedStyle(root).backgroundColor:"noel";
    }catch(e){ o.visErr=String(e); }
    try{ window.__TAURI__.core.invoke("selfcheck_report",{report:o}); }catch(_){}
  }, 1500);
})();
"#;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[allow(unused_mut)]
    let mut builder = tauri::Builder::default()
        // single-instance MUST be the first plugin registered.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_updater::Builder::new().build());
    // macOS: NSPanel plugin so the popover can be a non-activating panel that floats over
    // fullscreen apps (a plain window can't reliably appear on another app's fullscreen Space).
    #[cfg(target_os = "macos")]
    {
        builder = builder.plugin(tauri_nspanel::init());
    }
    builder
        .on_page_load(|webview, payload| {
            if matches!(payload.event(), tauri::webview::PageLoadEvent::Finished)
                && std::env::var("CCBUD_SELFCHECK").is_ok()
            {
                match webview.label() {
                    "main" => {
                        let _ = webview.eval(SELFCHECK_JS);
                    }
                    "popover" => {
                        let _ = webview.eval(POPOVER_SELFCHECK_JS);
                    }
                    _ => {}
                }
            }
        })
        .setup(|app| {
            if cfg!(debug_assertions) {
                app.handle().plugin(
                    tauri_plugin_log::Builder::default()
                        .level(log::LevelFilter::Info)
                        .build(),
                )?;
            }
            // One-time migrations: a detected Codex install (~/.codex/sessions) and an XDG
            // Claude tree (~/.config/claude/projects) join historyDirs as regular work dirs.
            // Runs BEFORE the history watcher so their trees get watched.
            store::ensure_codex_dir();
            store::ensure_xdg_claude_dir();

            // Start the localhost gateway on the configured port (proxy.js parity).
            let gw = gateway::GatewayState::new(app.handle().clone());
            app.manage(gw.clone());
            // Sidecar plugin manager (see plugin.rs) — discovers, launches, and health-gates
            // coding-agent plugins, surfacing each running one as a backend:"plugin" provider.
            app.manage(plugin::PluginManager::new());
            let startup_cfg = store::read_config();
            let port = startup_cfg.get("port").and_then(|v| v.as_u64()).unwrap_or(8788) as u16;
            let enabled = startup_cfg.get("gatewayEnabled").and_then(|v| v.as_bool()).unwrap_or(true);
            let app_for_tray = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                if enabled {
                    if let Err(e) = gw.start(port).await {
                        eprintln!("[ccbud] gateway start failed: {}", e);
                    }
                }
                refresh_tray_menu(&app_for_tray);
            });

            // System tray: icon + dynamic i18n menu (status / open / connect-or-disconnect /
            // check-updates / quit, parity with main.js buildTrayMenu) + click-to-open popover.
            {
                use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
                let init_cfg = store::read_config();
                let menu = build_tray_menu(app.handle(), false, "", &config_lang(&init_cfg))?;
                // Menu-bar icon: monochrome template (like other macOS apps), auto black/white.
                let tray_img = tauri::image::Image::from_bytes(include_bytes!("../../build/iconTemplate.png"))
                    .unwrap_or_else(|_| app.default_window_icon().cloned().unwrap());
                let _ = TrayIconBuilder::with_id("main")
                    .icon(tray_img)
                    .icon_as_template(true)
                    .tooltip("ccbud")
                    .menu(&menu)
                    .show_menu_on_left_click(false)
                    .on_menu_event(|app, event| match event.id.as_ref() {
                        "tray_open" => {
                            if let Some(w) = app.get_webview_window("main") {
                                set_dock_visible(app, true);
                                let _ = w.show();
                                let _ = w.unminimize();
                                let _ = w.set_focus();
                            }
                        }
                        // Tray toggles the gateway SERVICE (start/stop), never the CLI configs.
                        "tray_gw_start" | "tray_gw_stop" => {
                            let on = event.id.as_ref() == "tray_gw_start";
                            let app = app.clone();
                            tauri::async_runtime::spawn(async move {
                                let mut cfg = store::read_config();
                                cfg["gatewayEnabled"] = json!(on);
                                let saved = store::write_config(cfg);
                                let port = saved.get("port").and_then(|v| v.as_u64()).unwrap_or(8788) as u16;
                                let gw = app
                                    .try_state::<std::sync::Arc<gateway::GatewayState>>()
                                    .map(|s| s.inner().clone());
                                if let Some(gw) = gw {
                                    if on {
                                        let _ = gw.start(port).await;
                                    } else {
                                        gw.stop().await;
                                    }
                                    let status = full_status(&gw).await;
                                    gw.emit("gateway:status", status);
                                }
                                refresh_tray_menu(&app);
                            });
                        }
                        "tray_check" => {
                            if let Some(w) = app.get_webview_window("main") {
                                set_dock_visible(app, true);
                                let _ = w.show();
                                let _ = w.unminimize();
                                let _ = w.set_focus();
                            }
                            // Open the About/update pane shortly after the window is up (main.js parity).
                            let app2 = app.clone();
                            tauri::async_runtime::spawn(async move {
                                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                                let _ = app2.emit("update:openPane", json!({}));
                            });
                        }
                        "tray_quit" => app.exit(0),
                        _ => {}
                    })
                    .on_tray_icon_event(|tray, event| {
                        if let TrayIconEvent::Click {
                            button: MouseButton::Left,
                            button_state: MouseButtonState::Up,
                            rect,
                            ..
                        } = event
                        {
                            let app = tray.app_handle();
                            if let Some(pop) = app.get_webview_window("popover") {
                                #[cfg(target_os = "macos")]
                                let vis_before = {
                                    use tauri_nspanel::ManagerExt as _;
                                    app.get_webview_panel("popover")
                                        .map(|p| p.is_visible())
                                        .unwrap_or(false)
                                };
                                #[cfg(not(target_os = "macos"))]
                                let vis_before = pop.is_visible().unwrap_or(false);
                                let debounced = now_ms()
                                    - LAST_POPOVER_HIDE_MS
                                        .load(std::sync::atomic::Ordering::Relaxed)
                                    < 250;
                                let action;
                                if vis_before {
                                    #[cfg(target_os = "macos")]
                                    {
                                        use tauri_nspanel::ManagerExt as _;
                                        if let Ok(p) = app.get_webview_panel("popover") {
                                            p.order_out(None);
                                        }
                                    }
                                    #[cfg(not(target_os = "macos"))]
                                    let _ = pop.hide();
                                    LAST_POPOVER_HIDE_MS
                                        .store(now_ms(), std::sync::atomic::Ordering::Relaxed);
                                    action = "hide";
                                } else if debounced {
                                    // Debounce: clicking the tray first blurs (hides) the popover;
                                    // without this the same click would re-show it instantly.
                                    action = "debounce_skip";
                                } else {
                                    // Center under the tray icon, clamped to the monitor (rect +
                                    // scale are physical px, so retina is handled correctly).
                                    //
                                    // Pick the monitor the TRAY icon sits on. pop.current_monitor() is the
                                    // monitor the (hidden) popover window last sat on, which on a
                                    // multi-display setup is often NOT the screen whose menu bar was
                                    // clicked; using it clamps the popover to the wrong monitor's
                                    // bounds. Find the monitor whose physical bounds contain the tray
                                    // rect (each candidate's own scale converts the rect to px).
                                    let mon = pop
                                        .available_monitors()
                                        .ok()
                                        .and_then(|mons| {
                                            mons.into_iter().find(|m| {
                                                let p = rect
                                                    .position
                                                    .to_physical::<f64>(m.scale_factor());
                                                let mp = m.position();
                                                let ms = m.size();
                                                p.x >= mp.x as f64
                                                    && p.x < mp.x as f64 + ms.width as f64
                                                    && p.y >= mp.y as f64
                                                    && p.y < mp.y as f64 + ms.height as f64
                                            })
                                        })
                                        .or_else(|| pop.current_monitor().ok().flatten())
                                        .or_else(|| pop.primary_monitor().ok().flatten());
                                    let geom = mon.map(|mon| {
                                        let scale = mon.scale_factor();
                                        let pw = (424.0 * scale) as i32;
                                        let sx = mon.position().x;
                                        let sw = mon.size().width as i32;
                                        let tray_pos = rect.position.to_physical::<f64>(scale);
                                        let tray_size = rect.size.to_physical::<f64>(scale);
                                        let tray_cx = (tray_pos.x + tray_size.width / 2.0) as i32;
                                        let x = (tray_cx - pw / 2).clamp(sx + 4, sx + sw - pw - 4);
                                        let y = (tray_pos.y + tray_size.height + 2.0) as i32;
                                        tauri::PhysicalPosition::new(x, y)
                                    });
                                    if let Some(p) = geom {
                                        let _ = pop.set_position(p);
                                    }
                                    // Show via the NSPanel: nonactivating, so it appears on the
                                    // CURRENT Space (incl. a fullscreen app's) without activating
                                    // ccbud or switching Spaces.
                                    #[cfg(target_os = "macos")]
                                    {
                                        use tauri_nspanel::ManagerExt as _;
                                        if let Ok(p) = app.get_webview_panel("popover") {
                                            p.show();
                                        }
                                    }
                                    #[cfg(not(target_os = "macos"))]
                                    {
                                        let _ = pop.show();
                                        let _ = pop.set_focus();
                                    }
                                    if let Some(p) = geom {
                                        let _ = pop.set_position(p);
                                    }
                                    let _ = app.emit("popover:show", ());
                                    LAST_POPOVER_SHOW_MS
                                        .store(now_ms(), std::sync::atomic::Ordering::Relaxed);
                                    // The popover appearing counts as "app became visible today".
                                    auto_update_on_visible(app);
                                    action = "show";
                                }
                                if let Ok(path) = std::env::var("CCBUD_SELFCHECK_OUT") {
                                    use std::io::Write;
                                    if let Ok(mut f) = std::fs::OpenOptions::new()
                                        .create(true)
                                        .append(true)
                                        .open(&path)
                                    {
                                        let _ = writeln!(
                                            f,
                                            "{}",
                                            json!({ "trayClick": action, "visBefore": vis_before })
                                        );
                                    }
                                }
                            }
                        }
                    })
                    .build(app)?;
            }

            // Popover behavior: (1) float on the current Space AND over fullscreen apps;
            // (2) auto-hide when it loses focus — clicking anywhere else closes it.
            if let Some(pop) = app.get_webview_window("popover") {
                // macOS: convert the popover into a non-activating NSPanel. Unlike a plain window,
                // a nonactivating panel can float on the CURRENT Space — including another app's
                // fullscreen Space — and shows without activating ccbud or switching Spaces.
                #[cfg(target_os = "macos")]
                {
                    use tauri_nspanel::cocoa::appkit::NSWindowCollectionBehavior as CB;
                    use tauri_nspanel::WebviewWindowExt as _;
                    if let Ok(panel) = pop.to_panel() {
                        panel.set_style_mask((1 << 7) as i32); // NSWindowStyleMaskNonactivatingPanel
                        panel.set_collection_behaviour(
                            CB::NSWindowCollectionBehaviorCanJoinAllSpaces
                                | CB::NSWindowCollectionBehaviorFullScreenAuxiliary
                                | CB::NSWindowCollectionBehaviorStationary,
                        );
                        panel.set_floating_panel(true);
                        panel.set_level(24); // ~NSMainMenuWindowLevel: above fullscreen content
                        panel.set_hides_on_deactivate(false);
                        panel.set_released_when_closed(false);
                    }
                }
                let pop2 = pop.clone();
                pop.on_window_event(move |event| {
                    // Bind + deref: `Focused(false)` as a literal pattern does NOT match against
                    // &WindowEvent here (match ergonomics), so the handler would never fire.
                    if let tauri::WindowEvent::Focused(focused) = event {
                        if !*focused {
                            // Grace period: a fullscreen app steals focus the instant the popover
                            // shows; ignore that blur so it isn't hidden before being seen. A real
                            // click-away blur arrives well after the show.
                            if now_ms()
                                - LAST_POPOVER_SHOW_MS.load(std::sync::atomic::Ordering::Relaxed)
                                >= 400
                            {
                                let _ = pop2.hide();
                                LAST_POPOVER_HIDE_MS
                                    .store(now_ms(), std::sync::atomic::Ordering::Relaxed);
                            }
                        }
                    }
                });
            }

            // Daily auto update, triggered by the app becoming visible (see auto_update_on_visible).
            // Main-window focus covers launch, tray "open main", Dock/taskbar switches and the
            // single-instance re-open; the popover-show branch of the tray click covers tray-only days.
            if let Some(main) = app.get_webview_window("main") {
                let h = app.handle().clone();
                main.on_window_event(move |event| {
                    if let tauri::WindowEvent::Focused(focused) = event {
                        if *focused {
                            auto_update_on_visible(&h);
                        }
                    }
                });
            }
            // Launch counts as today's first visibility even if no focus event fires (e.g. an
            // autostarted login launch that opens unfocused). Delayed a few seconds so the
            // network/gateway are up before the first check.
            {
                let h = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    let visible = h
                        .get_webview_window("main")
                        .and_then(|w| w.is_visible().ok())
                        .unwrap_or(false);
                    if visible {
                        auto_update_on_visible(&h);
                    }
                });
            }

            // Tray usage title: show the configured token count next to the menu-bar icon
            // (macOS), refreshed on a timer so it tracks new usage without any user action.
            {
                let h = app.handle().clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(1500));
                    loop {
                        update_tray_title(&h);
                        std::thread::sleep(std::time::Duration::from_secs(60));
                    }
                });
            }

            // History live-watch: fs events on the projects dirs → history:changed.
            {
                use notify_debouncer_mini::{new_debouncer, notify::RecursiveMode, DebounceEventResult};
                let app_w = app.handle().clone();
                if let Ok(mut deb) = new_debouncer(
                    std::time::Duration::from_millis(250),
                    move |res: DebounceEventResult| {
                        if let Ok(events) = res {
                            let files: Vec<String> = events.iter().map(|e| e.path.to_string_lossy().to_string()).collect();
                            let _ = app_w.emit("history:changed", json!({ "files": files }));
                            // History changed → drop the stale usage cache and re-warm off-thread
                            // (+ refresh the tray title) so the next popover open stays instant.
                            usage::invalidate_cache();
                            let h = app_w.clone();
                            std::thread::spawn(move || {
                                let cfg = store::read_config();
                                usage::warm_cache(&cfg, "all");
                                if let Some(g) = h.try_state::<std::sync::Arc<gateway::GatewayState>>() {
                                    g.log("info", usage::diag(&cfg, "all"));
                                }
                                update_tray_title(&h);
                            });
                        }
                    },
                ) {
                    for root in history::watch_roots(&store::read_config()) {
                        if root.is_dir() {
                            let _ = deb.watcher().watch(&root, RecursiveMode::Recursive);
                        }
                    }
                    std::mem::forget(deb); // keep watching for the app's lifetime
                }
            }

            // Warm the usage cache at startup (off the click path) so the FIRST popover open is
            // instant instead of paying the ~0.5s cold-scan cost.
            {
                let cfg = store::read_config();
                let h = app.handle().clone();
                std::thread::spawn(move || {
                    usage::warm_cache(&cfg, "all");
                    // Surface the scan shape in the settings Logs panel — the first place to look
                    // when the usage numbers look wrong.
                    if let Some(g) = h.try_state::<std::sync::Arc<gateway::GatewayState>>() {
                        g.log("info", usage::diag(&cfg, "all"));
                    }
                });
            }

            // Reflect persisted Claude Code connection state in the tray menu on launch (the menu
            // is built optimistically as "disconnected"; this corrects it if settings.json already
            // points at us).
            {
                let h = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    refresh_tray_menu(&h);
                });
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            config_get, config_save, provider_upsert, provider_delete, provider_set_active, provider_test,
            plugin_list, plugin_status, plugin_set_enabled, plugin_auth_login, plugin_auth_logout,
            plugin_action, plugin_action_load,
            plugin_install, plugin_uninstall, plugin_open_dir,
            plugin_install_git, plugin_check_update, plugin_update,
            claude_connect, claude_disconnect, set_connect_target, desktop_status, desktop_connect, desktop_disconnect, desktop_replay,
            server_status, gateway_set_enabled, usage_get, monitor_get, monitor_clear, logs_get, logs_clear,
            app_open_main, app_quit, window_settings_mode, window_view_min_width,
            history_projects, history_list, history_get, history_search, history_dirs, history_pick_dir, history_set_active,
            history_import, history_import_paths, history_remove_import, history_set_meta, history_delete_forever, history_export_raw, history_export_html,
            util_copy, util_open_external,
            update_state, update_check, update_download, update_apply, update_set_auto,
            selfcheck_report, selfcheck_routing, selfcheck_gateway, selfcheck_history, selfcheck_desktop, selfcheck_export, selfcheck_import, selfcheck_popover
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            // Keep running in the tray when the user closes the window (hide instead of quit).
            if let tauri::RunEvent::WindowEvent {
                label,
                event: tauri::WindowEvent::CloseRequested { api, .. },
                ..
            } = event
            {
                if let Some(w) = app_handle.get_webview_window(&label) {
                    let _ = w.hide();
                }
                // Closing the main window drops the Dock icon back to menu-bar-only.
                if label == "main" {
                    set_dock_visible(app_handle, false);
                }
                api.prevent_close();
            }
        });
}
