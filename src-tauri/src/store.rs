// Config persistence.
//
// All settings live under ~/.ccbud/config.json (override the dir with CCBUD_HOME, used by
// tests/self-check). Writes are atomic (temp file + rename, mode 0600) so a crash mid-write
// never tears the file. `normalize` keeps the on-disk schema stable across releases.

use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};

pub fn ccbud_home() -> PathBuf {
    if let Ok(d) = std::env::var("CCBUD_HOME") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".ccbud")
}

fn config_file() -> PathBuf {
    ccbud_home().join("config.json")
}

fn config_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn default_config() -> Value {
    json!({
        "port": 8788,
        "activeProviderId": null,
        "requireToken": false,
        "gatewayToken": "",
        "gatewayEnabled": true,
        "openAtLogin": false,
        "claudeBackup": null,
        "trayUsage": { "enabled": false, "range": "7d" },
        "language": null,
        "historyDirs": ["~/.claude"],
        "historyActive": "all",
        "connectTargets": [],
        "retry429": { "enabled": true, "max": 3, "baseMs": 500 },
        "insecureSkipVerify": false,
        "autoUpdate": { "check": true, "autoDownload": true },
        "providers": []
    })
}

/// Collapse a home-prefixed absolute path back to `~` form so the UI shows
/// `~/.claude` instead of `/Users/<name>/.claude`. Inverse of history::expand_tilde.
pub fn collapse_home(p: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    if home.is_empty() {
        return p.to_string();
    }
    let home = home.trim_end_matches('/');
    if p == home {
        return "~".to_string();
    }
    if let Some(rest) = p.strip_prefix(&format!("{}/", home)) {
        return format!("~/{}", rest);
    }
    p.to_string()
}

fn str_of(v: Option<&Value>) -> String {
    v.and_then(|x| x.as_str()).unwrap_or("").to_string()
}
fn bool_of(v: Option<&Value>, default: bool) -> bool {
    v.and_then(|x| x.as_bool()).unwrap_or(default)
}

/// Mirror of store.js `normalize`: merge over defaults, then sanitize every field.
pub fn normalize(input: Value) -> Value {
    let mut c = default_config();
    if let Value::Object(src) = &input {
        let obj = c.as_object_mut().unwrap();
        for (k, v) in src {
            obj.insert(k.clone(), v.clone());
        }
    }

    // ---- providers ----
    let mut norm_provs: Vec<Value> = vec![];
    if let Some(Value::Array(arr)) = c.get("providers") {
        for p in arr {
            let name = {
                let n = str_of(p.get("name"));
                if n.is_empty() { "Unnamed".to_string() } else { n }
            };
            let map_default = p
                .get("mapDefaultModels")
                .map(|v| v.as_bool().unwrap_or(true))
                .unwrap_or(true);
            let mut models: Vec<Value> = vec![];
            if let Some(Value::Array(ms)) = p.get("models") {
                for m in ms {
                    let alias = str_of(m.get("alias"));
                    let upstream = str_of(m.get("upstream"));
                    if !alias.is_empty() || !upstream.is_empty() {
                        models.push(json!({ "alias": alias, "upstream": upstream }));
                    }
                }
            }
            // Upstream wire protocol. Default 'anthropic' = today's verbatim passthrough; the
            // other two make the gateway translate Claude Code's Anthropic Messages into the
            // provider's format (see src/protocol/). Anything unrecognized falls back to anthropic.
            let protocol = match p.get("protocol").and_then(|v| v.as_str()) {
                Some("openai-chat") => "openai-chat",
                Some("openai-responses") => "openai-responses",
                _ => "anthropic",
            };
            // Zhipu's Anthropic-compatible endpoint is versioned. The old preset omitted `/v1`;
            // its unversioned path returns HTTP 200 with an embedded `404 NOT_FOUND`, which cannot
            // trigger the gateway's status-based compatibility retry. Normalize only that exact
            // legacy preset URL, leaving every custom/provider URL authoritative.
            let mut base_url = str_of(p.get("baseUrl"));
            if protocol == "anthropic"
                && base_url.trim_end_matches('/') == "https://open.bigmodel.cn/api/anthropic"
            {
                base_url = "https://open.bigmodel.cn/api/anthropic/v1".to_string();
            }
            let mut np = json!({
                "id": p.get("id").cloned().unwrap_or(Value::Null),
                "name": name,
                "baseUrl": base_url,
                "authToken": str_of(p.get("authToken")),
                "defaultModel": str_of(p.get("defaultModel")),
                "smallFastModel": str_of(p.get("smallFastModel")),
                "mapDefaultModels": map_default,
                "protocol": protocol,
                "models": models,
            });
            if let Some(ic) = p.get("icon").and_then(|v| v.as_str()) {
                if !ic.trim().is_empty() {
                    np.as_object_mut()
                        .unwrap()
                        .insert("icon".into(), json!(ic.trim()));
                }
            }
            // Backend type. 'http' (default) = an ordinary upstream at baseUrl. 'plugin' = fronted
            // by a local sidecar plugin process (see plugin.rs); its baseUrl points at the plugin's
            // localhost port, maintained by PluginManager. pluginId links back to the plugin.
            let backend = match p.get("backend").and_then(|v| v.as_str()) {
                Some("plugin") => "plugin",
                _ => "http",
            };
            np.as_object_mut()
                .unwrap()
                .insert("backend".into(), json!(backend));
            if backend == "plugin" {
                np.as_object_mut()
                    .unwrap()
                    .insert("pluginId".into(), json!(str_of(p.get("pluginId"))));
            }
            norm_provs.push(np);
        }
    }
    // activeProviderId: keep if it points at a real provider, else first provider, else null.
    let active = c.get("activeProviderId").cloned().unwrap_or(Value::Null);
    let active_ok = norm_provs.iter().any(|p| p.get("id") == Some(&active));
    let active = if active_ok {
        active
    } else {
        norm_provs
            .first()
            .and_then(|p| p.get("id").cloned())
            .unwrap_or(Value::Null)
    };

    let obj = c.as_object_mut().unwrap();
    obj.insert("providers".into(), json!(norm_provs));
    obj.insert("activeProviderId".into(), active);

    // ---- scalars ----
    let port = obj
        .get("port")
        .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
        .filter(|n| *n > 0)
        .unwrap_or(8788);
    obj.insert("port".into(), json!(port));
    obj.insert("requireToken".into(), json!(bool_of(obj.get("requireToken"), false)));
    obj.insert("gatewayEnabled".into(), json!(bool_of(obj.get("gatewayEnabled"), true)));
    obj.insert("gatewayToken".into(), json!(str_of(obj.get("gatewayToken"))));
    obj.insert("openAtLogin".into(), json!(bool_of(obj.get("openAtLogin"), false)));
    if obj.get("claudeBackup").map(|v| v.is_null()).unwrap_or(true) {
        obj.insert("claudeBackup".into(), Value::Null);
    }

    // trayUsage
    let tu = obj.get("trayUsage").cloned().unwrap_or(json!({}));
    let tu_enabled = bool_of(tu.get("enabled"), false);
    let tu_range = tu
        .get("range")
        .and_then(|v| v.as_str())
        .filter(|r| ["1d", "7d", "30d", "all"].contains(r))
        .unwrap_or("7d");
    obj.insert("trayUsage".into(), json!({ "enabled": tu_enabled, "range": tu_range }));

    // retry429 (clamped)
    let rr = obj.get("retry429").cloned().unwrap_or(json!({}));
    let rr_enabled = rr.get("enabled").map(|v| v.as_bool().unwrap_or(true)).unwrap_or(true);
    let rr_max = rr.get("max").and_then(|v| v.as_i64()).filter(|n| *n >= 0).map(|n| n.min(10)).unwrap_or(3);
    let rr_base = rr.get("baseMs").and_then(|v| v.as_i64()).filter(|n| *n >= 0).map(|n| n.min(10000)).unwrap_or(500);
    obj.insert("retry429".into(), json!({ "enabled": rr_enabled, "max": rr_max, "baseMs": rr_base }));

    obj.insert("insecureSkipVerify".into(), json!(bool_of(obj.get("insecureSkipVerify"), false)));

    // autoUpdate
    let au = obj.get("autoUpdate").cloned().unwrap_or(json!({}));
    let au_check = au.get("check").map(|v| v.as_bool().unwrap_or(true)).unwrap_or(true);
    let au_dl = au.get("autoDownload").map(|v| v.as_bool().unwrap_or(true)).unwrap_or(true);
    obj.insert("autoUpdate".into(), json!({ "check": au_check, "autoDownload": au_dl }));

    // language: only the supported set, else null
    let lang = obj
        .get("language")
        .and_then(|v| v.as_str())
        .filter(|l| ["en", "zh", "zh-TW", "ja", "ko"].contains(l))
        .map(|s| s.to_string());
    obj.insert("language".into(), lang.map(Value::String).unwrap_or(Value::Null));

    // historyDirs: trim, strip trailing slashes, dedup, ensure ~/.claude present
    let mut dirs: Vec<String> = vec![];
    if let Some(Value::Array(ds)) = obj.get("historyDirs") {
        for d in ds {
            if let Some(s) = d.as_str() {
                // Collapse home-prefixed absolute paths to `~/…` for a tidy, portable display.
                let t = collapse_home(s.trim().trim_end_matches(['/', '\\']));
                if !t.is_empty() && !dirs.iter().any(|x| *x == t) {
                    dirs.push(t);
                }
            }
        }
    }
    if !dirs.iter().any(|d| d == "~/.claude") {
        dirs.insert(0, "~/.claude".to_string());
    }
    obj.insert("historyDirs".into(), json!(dirs));

    // connectTargets: which coding CLIs are wired to the gateway. Subset of {claude, codex}, deduped.
    // Empty is a VALID state (everything disconnected) — don't snap it back to ["claude"], or the UI
    // toggle for the last-remaining CLI could never turn off. Fresh and legacy configs deliberately
    // normalize to [] so startup never mistakes a schema default for an explicit connection choice.
    let mut targets: Vec<String> = vec![];
    if let Some(arr) = obj.get("connectTargets").and_then(|v| v.as_array()) {
        for t in arr {
            if let Some(s) = t.as_str() {
                if (s == "claude" || s == "codex") && !targets.iter().any(|x| x == s) {
                    targets.push(s.to_string());
                }
            }
        }
    }
    obj.insert("connectTargets".into(), json!(targets));

    // historyActive: 'all' | '__imported__' | '__trash__' (recycle bin) | a configured dir, else 'all'.
    // '__codex__' is the retired synthetic Codex bucket — map it onto the real ~/.codex dir entry.
    let ha = obj.get("historyActive").and_then(|v| v.as_str()).unwrap_or("all").to_string();
    let ha = if ha == "__codex__" { crate::codex::codex_label() } else { ha };
    let ha_ok = ha == "all" || ha == "__imported__" || ha == "__trash__" || dirs.iter().any(|d| *d == ha);
    obj.insert("historyActive".into(), json!(if ha_ok { ha } else { "all".to_string() }));

    c
}

fn read_config_unlocked() -> Value {
    match fs::read_to_string(config_file()) {
        Ok(s) => match serde_json::from_str::<Value>(&s) {
            Ok(v) => normalize(v),
            Err(_) => default_config(),
        },
        Err(_) => default_config(),
    }
}

pub fn read_config() -> Value {
    let _guard = config_lock();
    read_config_unlocked()
}

fn write_config_unlocked(next: Value) -> (Value, bool) {
    let normalized = normalize(next);
    let dir = ccbud_home();
    if fs::create_dir_all(&dir).is_err() {
        return (normalized, false);
    }
    let file = config_file();
    let tmp = dir.join("config.json.tmp");
    if let Ok(bytes) = serde_json::to_vec_pretty(&normalized) {
        if fs::write(&tmp, &bytes).is_ok() {
            set_0600(&tmp);
            if fs::rename(&tmp, &file).is_ok() {
                set_0600(&file);
                return (normalized, true);
            }
        }
    }
    let _ = fs::remove_file(tmp);
    (normalized, false)
}

pub fn write_config(next: Value) -> Value {
    let _guard = config_lock();
    write_config_unlocked(next).0
}

fn update_provider_base_url_to_v1(
    config: &mut Value,
    provider_id: &str,
    expected_base_url: &str,
) -> bool {
    let Some(provider) = config
        .get_mut("providers")
        .and_then(Value::as_array_mut)
        .and_then(|providers| {
            providers
                .iter_mut()
                .find(|provider| provider.get("id").and_then(Value::as_str) == Some(provider_id))
        })
    else {
        return false;
    };
    if provider.get("backend").and_then(Value::as_str) == Some("plugin")
        || provider.get("baseUrl").and_then(Value::as_str) != Some(expected_base_url)
    {
        return false;
    }
    let Some(provider) = provider.as_object_mut() else {
        return false;
    };
    provider.insert(
        "baseUrl".into(),
        json!(format!("{}/v1", expected_base_url.trim_end_matches('/'))),
    );
    true
}

/// Atomically migrate one HTTP provider's base URL after a successful `/v1` fallback.
/// The expected URL is a compare-and-swap guard against overwriting a concurrent user edit.
pub fn migrate_provider_base_url_to_v1(
    provider_id: &str,
    expected_base_url: &str,
) -> Option<Value> {
    let _guard = config_lock();
    let mut config = read_config_unlocked();
    if !update_provider_base_url_to_v1(&mut config, provider_id, expected_base_url) {
        return None;
    }
    let (saved, persisted) = write_config_unlocked(config);
    persisted.then_some(saved)
}

/// One-time startup migration: when a Codex install exists (its sessions tree is on disk),
/// add its config dir (`~/.codex`, CODEX_HOME-aware) to historyDirs so Codex conversations
/// appear in 对话 like any other work dir. The `codexDirAutoAdded` flag makes this run once —
/// a user who later REMOVES the dir isn't fighting an auto-re-add. Returns true if it changed
/// the config (caller refreshes the history views). Mirrors main.js ensureCodexDir.
pub fn ensure_codex_dir() -> bool {
    let mut cfg = read_config();
    if cfg.get("codexDirAutoAdded").and_then(|v| v.as_bool()).unwrap_or(false) {
        return false;
    }
    if !crate::codex::root_exists() {
        return false; // no Codex install yet — keep probing on future launches
    }
    let label = crate::codex::codex_label();
    let obj = cfg.as_object_mut().unwrap();
    let mut dirs: Vec<String> = obj
        .get("historyDirs")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|d| d.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
    if !dirs.iter().any(|d| *d == label) {
        dirs.push(label);
    }
    obj.insert("historyDirs".into(), json!(dirs));
    obj.insert("codexDirAutoAdded".into(), json!(true));
    write_config(cfg);
    true
}

/// Shared body of the ensure_*_dir migrations: when `exists` and the run-once `flag` hasn't
/// fired, add `label` to historyDirs (dedup) and set the flag. Returns true when the config
/// changed (caller refreshes the history views). A user who later REMOVES the dir isn't
/// fighting an auto-re-add; a missing install keeps probing on future launches.
fn ensure_history_dir(flag: &str, exists: bool, label: String) -> bool {
    let mut cfg = read_config();
    if cfg.get(flag).and_then(|v| v.as_bool()).unwrap_or(false) {
        return false;
    }
    if !exists {
        return false; // nothing there yet — keep probing on future launches
    }
    let obj = cfg.as_object_mut().unwrap();
    let mut dirs: Vec<String> = obj
        .get("historyDirs")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|d| d.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
    if !dirs.iter().any(|d| *d == label) {
        dirs.push(label);
    }
    obj.insert("historyDirs".into(), json!(dirs));
    obj.insert(flag.into(), json!(true));
    write_config(cfg);
    true
}

/// One-time startup migrations for the other coding CLIs whose sessions the 对话 view can
/// browse: Grok Build (~/.grok, GROK_HOME-aware), GitHub Copilot CLI (~/.copilot), and the
/// Antigravity CLI (~/.gemini/antigravity-cli). Same run-once contract as ensure_codex_dir.
pub fn ensure_grok_dir() -> bool {
    ensure_history_dir("grokDirAutoAdded", crate::grok::root_exists(), crate::grok::grok_label())
}

pub fn ensure_copilot_dir() -> bool {
    ensure_history_dir("copilotDirAutoAdded", crate::copilot::root_exists(), crate::copilot::copilot_label())
}

pub fn ensure_antigravity_dir() -> bool {
    ensure_history_dir("antigravityDirAutoAdded", crate::antigravity::root_exists(), crate::antigravity::agy_label())
}

/// One-time startup migration (ccusage parity): Claude Code also writes history under the XDG
/// config dir (`$XDG_CONFIG_HOME/claude`, default `~/.config/claude`) — when that tree exists,
/// add it to historyDirs so its sessions count toward conversations and usage. Same run-once
/// contract as ensure_codex_dir.
pub fn ensure_xdg_claude_dir() -> bool {
    let mut cfg = read_config();
    if cfg.get("xdgClaudeDirAutoAdded").and_then(|v| v.as_bool()).unwrap_or(false) {
        return false;
    }
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(".config")
        });
    let dir = base.join("claude");
    if !dir.join("projects").is_dir() {
        return false; // nothing there yet — keep probing on future launches
    }
    let label = dir.to_string_lossy().to_string();
    let obj = cfg.as_object_mut().unwrap();
    let mut dirs: Vec<String> = obj
        .get("historyDirs")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|d| d.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
    if !dirs.iter().any(|d| *d == label) {
        dirs.push(label);
    }
    obj.insert("historyDirs".into(), json!(dirs));
    obj.insert("xdgClaudeDirAutoAdded".into(), json!(true));
    write_config(cfg);
    true
}

#[cfg(unix)]
fn set_0600(p: &PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(p, fs::Permissions::from_mode(0o600));
}
#[cfg(not(unix))]
fn set_0600(_p: &PathBuf) {}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fresh_and_legacy_configs_do_not_select_a_startup_connection() {
        assert_eq!(default_config()["connectTargets"], json!([]));
        assert_eq!(normalize(json!({}))["connectTargets"], json!([]));

        let explicit = normalize(json!({
            "connectTargets": ["codex", "claude", "codex", "invalid"]
        }));
        assert_eq!(explicit["connectTargets"], json!(["codex", "claude"]));
    }

    #[test]
    fn normalize_sanitizes_providers_and_active() {
        let input = json!({
            "port": 9000,
            "providers": [{ "name": "X", "baseUrl": "u", "authToken": "t", "extra": "drop",
                "models": [{ "alias": "a", "upstream": "u" }, { "alias": "", "upstream": "" }] }]
        });
        let n = normalize(input);
        assert_eq!(n["port"], 9000);
        assert_eq!(n["providers"][0]["name"], "X");
        assert!(n["providers"][0].get("extra").is_none(), "unknown field must be dropped");
        assert_eq!(n["providers"][0]["models"].as_array().unwrap().len(), 1, "empty model dropped");
        assert_eq!(n["activeProviderId"], n["providers"][0]["id"], "active auto-set to first provider");
        assert!(n["historyDirs"].as_array().unwrap().iter().any(|d| d == "~/.claude"));
        assert_eq!(n["providers"][0]["protocol"], "anthropic", "protocol defaults to anthropic (passthrough)");
    }

    #[test]
    fn provider_protocol_normalized() {
        let ok = normalize(json!({ "providers": [{ "name": "O", "protocol": "openai-chat" }] }));
        assert_eq!(ok["providers"][0]["protocol"], "openai-chat");
        // unrecognized → safe passthrough default
        let bad = normalize(json!({ "providers": [{ "name": "B", "protocol": "grpc" }] }));
        assert_eq!(bad["providers"][0]["protocol"], "anthropic");
    }
    #[test]
    fn normalize_migrates_legacy_glm_anthropic_base_url() {
        let legacy = normalize(json!({ "providers": [{
            "name": "GLM",
            "baseUrl": "https://open.bigmodel.cn/api/anthropic/",
            "protocol": "anthropic"
        }] }));
        assert_eq!(
            legacy["providers"][0]["baseUrl"],
            "https://open.bigmodel.cn/api/anthropic/v1"
        );

        let custom = normalize(json!({ "providers": [{
            "name": "Custom",
            "baseUrl": "https://example.com/api/anthropic",
            "protocol": "anthropic"
        }] }));
        assert_eq!(custom["providers"][0]["baseUrl"], "https://example.com/api/anthropic");
    }
    #[test]
    fn normalize_clamps_retry() {
        let n = normalize(json!({ "retry429": { "max": 999, "baseMs": 99999 } }));
        assert_eq!(n["retry429"]["max"], 10);
        assert_eq!(n["retry429"]["baseMs"], 10000);
    }
    #[test]
    fn normalize_keeps_recycle_bin_active() {
        // Synthetic buckets must survive normalize, else history_set_active("__trash__") is
        // silently reset to "all" and the recycle bin can never be opened.
        assert_eq!(normalize(json!({ "historyActive": "__trash__" }))["historyActive"], "__trash__");
        assert_eq!(normalize(json!({ "historyActive": "__imported__" }))["historyActive"], "__imported__");
        assert_eq!(normalize(json!({ "historyActive": "bogus-dir" }))["historyActive"], "all");
    }

    #[test]
    fn provider_base_url_v1_migration_updates_only_the_matching_url() {
        let mut config = json!({
            "port": 9000,
            "customSetting": { "keep": true },
            "providers": [
                {
                    "id": "target",
                    "name": "Target",
                    "backend": "http",
                    "baseUrl": "https://example.com/api/",
                    "authToken": "secret",
                    "defaultModel": "model-a",
                    "models": [{ "alias": "fast", "upstream": "model-b" }]
                },
                {
                    "id": "other",
                    "backend": "http",
                    "baseUrl": "https://other.example/api",
                    "authToken": "other-secret"
                }
            ]
        });
        let before_other = config["providers"][1].clone();
        let before_settings = config["customSetting"].clone();

        assert!(update_provider_base_url_to_v1(
            &mut config,
            "target",
            "https://example.com/api/"
        ));
        assert_eq!(
            config["providers"][0]["baseUrl"],
            "https://example.com/api/v1"
        );
        assert_eq!(config["providers"][0]["authToken"], "secret");
        assert_eq!(config["providers"][0]["defaultModel"], "model-a");
        assert_eq!(
            config["providers"][0]["models"],
            json!([{ "alias": "fast", "upstream": "model-b" }])
        );
        assert_eq!(config["providers"][1], before_other);
        assert_eq!(config["customSetting"], before_settings);
        assert_eq!(config["port"], 9000);
    }

    #[test]
    fn provider_base_url_v1_migration_requires_expected_old_url() {
        let mut config = json!({
            "providers": [{
                "id": "target",
                "backend": "http",
                "baseUrl": "https://example.com/user-edit"
            }]
        });
        let before = config.clone();

        assert!(!update_provider_base_url_to_v1(
            &mut config,
            "target",
            "https://example.com/old"
        ));
        assert_eq!(config, before);
    }

    #[test]
    fn provider_base_url_v1_migration_skips_plugins() {
        let mut config = json!({
            "providers": [{
                "id": "target",
                "backend": "plugin",
                "baseUrl": "http://127.0.0.1:12345"
            }]
        });
        let before = config.clone();

        assert!(!update_provider_base_url_to_v1(
            &mut config,
            "target",
            "http://127.0.0.1:12345"
        ));
        assert_eq!(config, before);
    }
}

/// Stable-enough unique id for a new provider (single-user, serialized writes).
pub fn gen_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("p{}", n)
}
