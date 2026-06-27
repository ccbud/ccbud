// Claude Code integration — Rust port of claude.js.
//
// Connect: point Claude Code at the local gateway (env.ANTHROPIC_BASE_URL / ANTHROPIC_AUTH_TOKEN)
// and clear model-name overrides so it sends native claude-* names (the gateway tier-maps them).
// The user's original values are backed up into config.claudeBackup; Disconnect restores them.
// Settings path overridable via CCBUD_CLAUDE_SETTINGS (tests never touch the real config).

#![allow(dead_code)]

use crate::store;
use serde_json::{json, Map, Value};
use std::fs;
use std::path::PathBuf;

const MODEL_ENV_KEYS: &[&str] = &[
    "ANTHROPIC_MODEL",
    "ANTHROPIC_SMALL_FAST_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
];
fn all_backup_keys() -> Vec<&'static str> {
    let mut v = vec!["ANTHROPIC_BASE_URL", "ANTHROPIC_AUTH_TOKEN"];
    v.extend_from_slice(MODEL_ENV_KEYS);
    v
}

pub fn settings_path() -> PathBuf {
    if let Ok(p) = std::env::var("CCBUD_CLAUDE_SETTINGS") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".claude").join("settings.json")
}

pub fn read_settings() -> Value {
    match fs::read_to_string(settings_path()) {
        Ok(raw) => serde_json::from_str::<Value>(&raw).ok().filter(|v| v.is_object()).unwrap_or_else(|| json!({})),
        Err(_) => json!({}),
    }
}

fn write_settings(obj: &Value) -> std::io::Result<()> {
    let p = settings_path();
    if let Some(dir) = p.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let tmp = p.with_extension("ccbud.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(obj).unwrap_or_default())?;
    fs::rename(&tmp, &p)
}

fn is_gateway_url(url: &str, port: u16) -> bool {
    if url.is_empty() {
        return false;
    }
    url.contains(&format!("localhost:{}", port)) || url.contains(&format!("127.0.0.1:{}", port))
}

pub fn is_connected(port: u16) -> bool {
    let s = read_settings();
    let url = s
        .get("env")
        .and_then(|e| e.get("ANTHROPIC_BASE_URL"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    is_gateway_url(url, port)
}

/// Connect Claude Code to the gateway, backing up prior values into config.claudeBackup once.
pub fn connect(port: u16, token: &str) {
    let mut s = read_settings();
    let env = s.get("env").and_then(|v| v.as_object()).cloned().unwrap_or_default();

    // Back up the original values exactly once.
    let cfg = store::read_config();
    if cfg.get("claudeBackup").map(|v| v.is_null()).unwrap_or(true) {
        let mut benv = Map::new();
        for k in all_backup_keys() {
            benv.insert(k.to_string(), env.get(k).cloned().unwrap_or(Value::Null));
        }
        let backup = json!({ "model": s.get("model").cloned().unwrap_or(Value::Null), "env": benv });
        let mut next = cfg.clone();
        next["claudeBackup"] = backup;
        store::write_config(next);
    }

    let sobj = s.as_object_mut().unwrap();
    let mut envobj = sobj.get("env").and_then(|v| v.as_object()).cloned().unwrap_or_default();
    envobj.insert("ANTHROPIC_BASE_URL".into(), json!(format!("http://localhost:{}", port)));
    envobj.insert("ANTHROPIC_AUTH_TOKEN".into(), json!(token));
    for k in MODEL_ENV_KEYS {
        envobj.remove(*k);
    }
    sobj.insert("env".into(), Value::Object(envobj));
    sobj.remove("model");
    let _ = write_settings(&s);
}

/// Disconnect Claude Code: restore the backed-up state (or just remove our keys).
pub fn disconnect() {
    let cfg = store::read_config();
    let backup = cfg.get("claudeBackup").cloned().unwrap_or(Value::Null);
    let mut s = read_settings();
    let sobj = s.as_object_mut().unwrap();
    let mut envobj = sobj.get("env").and_then(|v| v.as_object()).cloned().unwrap_or_default();

    if backup.is_object() {
        let benv = backup.get("env").and_then(|v| v.as_object());
        for k in all_backup_keys() {
            let bv = benv.and_then(|e| e.get(k)).cloned().unwrap_or(Value::Null);
            if bv.is_null() {
                envobj.remove(k);
            } else {
                envobj.insert(k.to_string(), bv);
            }
        }
        match backup.get("model").cloned().unwrap_or(Value::Null) {
            Value::Null => {
                sobj.remove("model");
            }
            m => {
                sobj.insert("model".into(), m);
            }
        }
        let mut next = cfg.clone();
        next["claudeBackup"] = Value::Null;
        store::write_config(next);
    } else {
        envobj.remove("ANTHROPIC_BASE_URL");
        envobj.remove("ANTHROPIC_AUTH_TOKEN");
    }

    if envobj.is_empty() {
        sobj.remove("env");
    } else {
        sobj.insert("env".into(), Value::Object(envobj));
    }
    let _ = write_settings(&s);
}

/// Token written as ANTHROPIC_AUTH_TOKEN: the gateway token when enforced, else a local placeholder
/// (the gateway only validates it when requireToken is on).
pub fn current_token(config: &Value) -> String {
    let require = config.get("requireToken").and_then(|v| v.as_bool()).unwrap_or(false);
    let token = config.get("gatewayToken").and_then(|v| v.as_str()).unwrap_or("");
    if require && !token.is_empty() {
        token.to_string()
    } else {
        "ccbud-local".to_string()
    }
}
