// ccbud Tauri backend.
//
// Every IPC command the renderer calls is registered here. config/provider commands are
// now backed by real persistence (store.rs → ~/.ccbud/config.json); the rest are still
// STUBS returning sensible empty shapes, filled in over later phases:
//   Phase 2 — gateway (proxy.js → Rust)        Phase 4 — system (window/tray/dialogs/claude)
//   Phase 3 — history/usage/export (in progress) Phase 5 — auto-update (tauri-plugin-updater)
//
// Stub params keep their real names (Tauri maps JS invoke args by name), so unused-var
// warnings are suppressed crate-wide until the bodies are filled in.
#![allow(unused_variables)]

mod claude;
mod claudedesktop;
mod counttokens;
mod exporthtml;
mod gateway;
mod history;
mod store;
mod usage;

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

    // Keep Claude Code's settings.json in sync if connected (port/token may have changed).
    if was_connected {
        let port = saved.get("port").and_then(|v| v.as_u64()).unwrap_or(8788) as u16;
        claude::connect(port, &claude::current_token(&saved));
    }

    // History dirs changed → invalidate + re-warm the usage cache and notify the renderer.
    if saved.get("historyDirs").cloned() != prev_dirs {
        usage::invalidate_cache();
        let active = saved
            .get("historyActive")
            .and_then(|v| v.as_str())
            .unwrap_or("all")
            .to_string();
        let cfg2 = saved.clone();
        std::thread::spawn(move || usage::warm_cache(&cfg2, &active));
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
/// Build the upstream `/v1/messages` URL from a provider baseUrl.
fn messages_url(base: &str) -> Option<String> {
    let base = base.trim();
    if !(base.starts_with("http://") || base.starts_with("https://")) {
        return None;
    }
    Some(format!("{}/v1/messages", base.trim_end_matches('/')))
}
/// Live connection test (mirror of main.js testProvider): POST a tiny ping to the provider's
/// /v1/messages and report ok / error / timeout. The renderer localizes the result message.
#[tauri::command]
async fn provider_test(p: Value) -> Value {
    let base = p.get("baseUrl").and_then(|v| v.as_str()).unwrap_or("").trim();
    if base.is_empty() {
        return json!({ "ok": false, "reason": "baseUrlEmpty" });
    }
    let url = match messages_url(base) {
        Some(u) => u,
        None => return json!({ "ok": false, "reason": "baseUrlInvalid" }),
    };
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
    let body = json!({
        "model": model,
        "max_tokens": 16,
        "messages": [{ "role": "user", "content": "ping" }],
    });
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
    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", token))
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .await;
    match resp {
        Ok(r) => {
            let status = r.status().as_u16();
            let text = r.text().await.unwrap_or_default();
            let parsed: Option<Value> = serde_json::from_str(&text).ok();
            let http_ok = (200..300).contains(&status);
            let is_message = parsed
                .as_ref()
                .and_then(|j| j.get("type"))
                .and_then(|v| v.as_str())
                == Some("message");
            if http_ok && is_message {
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

// ---- claude code / desktop integration (Phase 4) ----
#[tauri::command]
async fn claude_connect(
    app: tauri::AppHandle,
    gw: tauri::State<'_, std::sync::Arc<gateway::GatewayState>>,
) -> Result<Value, String> {
    let cfg = store::read_config();
    let n = cfg.get("providers").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
    if n == 0 {
        // reason code → renderer i18n (parity with desktop_connect), not a hardcoded English string.
        return Ok(json!({ "ok": false, "reason": "noProvider" }));
    }
    let port = cfg.get("port").and_then(|v| v.as_u64()).unwrap_or(8788) as u16;
    if let Err(e) = gw.start(port).await {
        // reason for i18n + message keeps the diagnostic detail for this rare path.
        return Ok(json!({ "ok": false, "reason": "portFailed", "message": format!("port {} failed: {}", port, e) }));
    }
    claude::connect(port, &claude::current_token(&cfg));
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
    claude::disconnect();
    gw.stop().await;
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
        .unwrap_or_else(|| "请基于这份对话记录在 Claude 桌面版里继续。".to_string());
    let url = format!("claude://cowork/new?q={}&file={}", pct(&prompt), pct(&file));
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

// ---- server / usage / monitor / logs (Phase 2/3) ----
async fn full_status(gw: &std::sync::Arc<gateway::GatewayState>) -> Value {
    let mut s = gw.status().await;
    let port = gw
        .current_port()
        .await
        .unwrap_or_else(|| store::read_config().get("port").and_then(|v| v.as_u64()).unwrap_or(8788) as u16);
    if let Some(o) = s.as_object_mut() {
        o.insert("connected".into(), json!(claude::is_connected(port)));
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
    let active = cfg.get("historyActive").and_then(|v| v.as_str()).unwrap_or("all").to_string();
    let r = usage::usage_get(&cfg, &active, range.as_deref().unwrap_or("7d"));
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
        let active = config.get("historyActive").and_then(|v| v.as_str()).unwrap_or("all").to_string();
        let tokens = usage::usage_get(&config, &active, &range)
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
    connected_with: &'static str,
    disconnected: &'static str,
    open_main: &'static str,
    disconnect: &'static str,
    connect: &'static str,
    quit: &'static str,
    check_updates: &'static str,
}
fn tray_labels(lang: &str) -> TrayLabels {
    match lang {
        "zh-CN" => TrayLabels { connected_with: "● 已接入：{name}", disconnected: "○ 未接入 Claude Code", open_main: "打开主界面", disconnect: "断开接入", connect: "一键接入", quit: "退出 ccbud", check_updates: "检查更新…" },
        "zh-TW" => TrayLabels { connected_with: "● 已接入：{name}", disconnected: "○ 未接入 Claude Code", open_main: "開啟主視窗", disconnect: "中斷接入", connect: "一鍵接入", quit: "結束 ccbud", check_updates: "檢查更新…" },
        "ja" => TrayLabels { connected_with: "● 接続済み：{name}", disconnected: "○ Claude Code 未接続", open_main: "メインウィンドウを開く", disconnect: "切断", connect: "接続", quit: "ccbud を終了", check_updates: "更新を確認…" },
        "ko" => TrayLabels { connected_with: "● 연결됨: {name}", disconnected: "○ Claude Code 미연결", open_main: "메인 창 열기", disconnect: "연결 해제", connect: "연결", quit: "ccbud 종료", check_updates: "업데이트 확인…" },
        _ => TrayLabels { connected_with: "● Connected: {name}", disconnected: "○ Not connected to Claude Code", open_main: "Open main window", disconnect: "Disconnect", connect: "Connect", quit: "Quit ccbud", check_updates: "Check for updates…" },
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
    connected: bool,
    provider: &str,
    lang: &str,
) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
    let l = tray_labels(lang);
    let status_txt = if connected {
        let name = if provider.is_empty() { "Claude Code" } else { provider };
        l.connected_with.replace("{name}", name)
    } else {
        l.disconnected.to_string()
    };
    // Status row is disabled (it's an indicator, like main.js { enabled: false }).
    let status_i = MenuItem::with_id(app, "tray_status", status_txt, false, None::<&str>)?;
    let open_i = MenuItem::with_id(app, "tray_open", l.open_main, true, None::<&str>)?;
    let conn_i = if connected {
        MenuItem::with_id(app, "tray_disconnect", l.disconnect, true, None::<&str>)?
    } else {
        MenuItem::with_id(app, "tray_connect", l.connect, true, None::<&str>)?
    };
    let check_i = MenuItem::with_id(app, "tray_check", l.check_updates, true, None::<&str>)?;
    let quit_i = MenuItem::with_id(app, "tray_quit", l.quit, true, None::<&str>)?;
    let sep1 = PredefinedMenuItem::separator(app)?;
    let sep2 = PredefinedMenuItem::separator(app)?;
    Menu::with_items(app, &[&status_i, &sep1, &open_i, &conn_i, &check_i, &sep2, &quit_i])
}
/// Rebuild the tray menu to reflect current connection state + locale + active provider.
fn refresh_tray_menu(app: &tauri::AppHandle) {
    let app2 = app.clone();
    let _ = app.run_on_main_thread(move || {
        let config = store::read_config();
        let port = config.get("port").and_then(|v| v.as_u64()).unwrap_or(8788) as u16;
        let connected = claude::is_connected(port);
        let provider = active_provider_name(&config);
        let lang = config_lang(&config);
        if let Ok(menu) = build_tray_menu(&app2, connected, &provider, &lang) {
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

// ---- window / app lifecycle (Phase 4) ----
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

// ---- conversation history (Phase 3) ----
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
fn history_dirs() -> Value {
    let cfg = store::read_config();
    let active = cfg.get("historyActive").and_then(|v| v.as_str()).unwrap_or("all").to_string();
    json!({ "dirs": history::dir_stats(&cfg), "active": active })
}
#[tauri::command]
async fn history_pick_dir() -> Result<Value, String> {
    let folder = rfd::AsyncFileDialog::new().set_title("选择 Claude 配置目录").pick_folder().await;
    match folder {
        // Return the picked path (home-collapsed to `~/…`) and let the renderer persist it
        // via saveConfig — mirrors the Electron `history:pickDir` contract the UI expects.
        Some(f) => {
            let path = store::collapse_home(&f.path().to_string_lossy());
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
    match rfd::AsyncFileDialog::new().add_filter("JSONL", &["jsonl"]).set_title("导入对话记录").pick_files().await {
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
async fn history_export_raw(file: String) -> Result<Value, String> {
    let data = std::fs::read_to_string(&file).map_err(|e| e.to_string())?;
    let base = exporthtml::export_base_name(&file);
    match rfd::AsyncFileDialog::new().set_file_name(format!("{}.jsonl", base)).save_file().await {
        Some(d) => {
            let p = d.path().to_path_buf();
            std::fs::write(&p, data).map_err(|e| e.to_string())?;
            Ok(json!({ "canceled": false, "path": p.to_string_lossy() }))
        }
        None => Ok(json!({ "canceled": true })),
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

// ---- utilities (Phase 4) ----
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
// the user's browser so they don't have to hunt for it in the filesystem — parity with the
// Electron `shell.openPath` (issue #7). Best-effort: a spawn failure must not fail the export.
fn open_path_native(path: &std::path::Path) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(path).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd").args(["/C", "start", ""]).arg(path).spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
}

// ---- in-app updates (Phase 5) ----
// In-app update state, mapped to the shape the renderer's about/update pane expects
// (runningVersion / latestVersion / mode / pending). Tauri's updater is in-app full → mode "hot".
static UPDATE_LATEST: std::sync::Mutex<Option<(String, Option<String>)>> =
    std::sync::Mutex::new(None);
static UPDATE_CHECKED: std::sync::Mutex<bool> = std::sync::Mutex::new(false);
static UPDATE_STAGED: std::sync::Mutex<bool> = std::sync::Mutex::new(false);

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
#[tauri::command]
async fn update_check(app: tauri::AppHandle) -> Result<Value, String> {
    use tauri_plugin_updater::UpdaterExt;
    *UPDATE_CHECKED.lock().unwrap() = true;
    let result = match app.updater() {
        Ok(updater) => updater.check().await,
        Err(e) => Err(e),
    };
    match result {
        Ok(Some(u)) => {
            *UPDATE_LATEST.lock().unwrap() = Some((u.version.clone(), u.body.clone()));
        }
        Ok(None) => {
            *UPDATE_LATEST.lock().unwrap() = None;
        }
        Err(e) => {
            return Ok(json!({
                "ok": false,
                "error": e.to_string(),
                "runningVersion": app.package_info().version.to_string(),
            }));
        }
    }
    let st = build_update_state(&app);
    let _ = app.emit("update:state", st.clone());
    Ok(st)
}
#[tauri::command]
async fn update_download(app: tauri::AppHandle) -> Result<Value, String> {
    use tauri_plugin_updater::UpdaterExt;
    let updater = app.updater().map_err(|e| e.to_string())?;
    match updater.check().await.map_err(|e| e.to_string())? {
        Some(u) => {
            u.download_and_install(|_chunk, _total| {}, || {}).await.map_err(|e| e.to_string())?;
            *UPDATE_STAGED.lock().unwrap() = true;
            let st = build_update_state(&app);
            let _ = app.emit("update:staged", st.clone());
            let _ = app.emit("update:state", st.clone());
            Ok(st)
        }
        None => Ok(json!({ "ok": true, "mode": "none" })),
    }
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
        "hasModels": p.contains("claude-sonnet-4-6") && p.contains("anthropicFamilyTier") && p.contains("isFamilyDefault"),
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
            // Start the localhost gateway on the configured port (proxy.js parity).
            let gw = gateway::GatewayState::new(app.handle().clone());
            app.manage(gw.clone());
            let port = store::read_config()
                .get("port")
                .and_then(|v| v.as_u64())
                .unwrap_or(8788) as u16;
            tauri::async_runtime::spawn(async move {
                if let Err(e) = gw.start(port).await {
                    eprintln!("[ccbud] gateway start failed: {}", e);
                }
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
                        "tray_connect" => {
                            let app = app.clone();
                            tauri::async_runtime::spawn(async move {
                                let cfg = store::read_config();
                                let port = cfg.get("port").and_then(|v| v.as_u64()).unwrap_or(8788) as u16;
                                let gw = app
                                    .try_state::<std::sync::Arc<gateway::GatewayState>>()
                                    .map(|s| s.inner().clone());
                                if let Some(gw) = gw {
                                    if gw.start(port).await.is_ok() {
                                        claude::connect(port, &claude::current_token(&cfg));
                                        let status = full_status(&gw).await;
                                        gw.emit("gateway:status", status);
                                    }
                                }
                                refresh_tray_menu(&app);
                            });
                        }
                        "tray_disconnect" => {
                            let app = app.clone();
                            tauri::async_runtime::spawn(async move {
                                claude::disconnect();
                                let gw = app
                                    .try_state::<std::sync::Arc<gateway::GatewayState>>()
                                    .map(|s| s.inner().clone());
                                if let Some(gw) = gw {
                                    gw.stop().await;
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
                                    // Pick the monitor the TRAY icon sits on — mirrors Electron's
                                    // getDisplayMatching(trayBounds). pop.current_monitor() is the
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
                                let active = cfg
                                    .get("historyActive")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("all")
                                    .to_string();
                                usage::warm_cache(&cfg, &active);
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
                let active = cfg
                    .get("historyActive")
                    .and_then(|v| v.as_str())
                    .unwrap_or("all")
                    .to_string();
                std::thread::spawn(move || {
                    usage::warm_cache(&cfg, &active);
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
            claude_connect, claude_disconnect, desktop_status, desktop_connect, desktop_disconnect, desktop_replay,
            server_status, usage_get, monitor_get, monitor_clear, logs_get, logs_clear,
            app_open_main, app_quit, window_settings_mode, window_view_min_width,
            history_projects, history_list, history_get, history_dirs, history_pick_dir, history_set_active,
            history_import, history_import_paths, history_remove_import, history_set_meta, history_export_raw, history_export_html,
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
