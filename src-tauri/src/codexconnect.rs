// Codex CLI integration — point Codex at the local gateway by injecting a custom model provider
// into ~/.codex/config.toml (CODEX_HOME-aware). Mirrors claude.rs's connect/disconnect+backup, but
// for Codex's TOML config: we add a `[model_providers.ccbud]` block (base_url → gateway, a static
// dev bearer token, requires_openai_auth=false so Codex doesn't demand an sk- prefix) and switch
// `model_provider`/`model` to it. The user's prior model/model_provider are backed up into
// config.codexBackup once; Disconnect restores them and removes our block. Editing is done with
// toml_edit so the user's other settings, comments, and formatting survive untouched.
//
// wire_api = "chat": Codex speaks OpenAI Chat Completions to the gateway, which then translates to
// whatever protocol the ACTIVE provider uses (chat passthrough, or chat→messages for an Anthropic
// provider). Config-path override for tests: CCBUD_CODEX_CONFIG.

#![allow(dead_code)]

use crate::store;
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use toml_edit::{value, DocumentMut, Item, Table};

const PROVIDER_ID: &str = "ccbud";

fn home() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("."))
}

pub fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("CCBUD_CODEX_CONFIG") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    match std::env::var("CODEX_HOME") {
        Ok(h) if !h.trim().is_empty() => PathBuf::from(h).join("config.toml"),
        _ => home().join(".codex").join("config.toml"),
    }
}

/// Whether Codex is installed enough to connect (its config dir or config file exists). We don't
/// require the file to pre-exist — connect creates it — but we do want ~/.codex to be present so we
/// don't spuriously offer Codex to users who don't have it.
pub fn is_available() -> bool {
    let p = config_path();
    p.exists() || p.parent().map(|d| d.is_dir()).unwrap_or(false)
}

fn read_doc() -> DocumentMut {
    fs::read_to_string(config_path())
        .ok()
        .and_then(|s| s.parse::<DocumentMut>().ok())
        .unwrap_or_default()
}

fn write_doc(doc: &DocumentMut) -> std::io::Result<()> {
    let p = config_path();
    if let Some(dir) = p.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let tmp = p.with_extension("ccbud.tmp");
    fs::write(&tmp, doc.to_string())?;
    fs::rename(&tmp, &p)
}

fn gateway_base(port: u16) -> String {
    format!("http://localhost:{}/v1", port)
}

pub fn is_connected(port: u16) -> bool {
    let doc = read_doc();
    doc.get("model_providers")
        .and_then(|mp| mp.as_table())
        .and_then(|t| t.get(PROVIDER_ID))
        .and_then(|p| p.as_table())
        .and_then(|t| t.get("base_url"))
        .and_then(|b| b.as_str())
        .map(|b| b == gateway_base(port))
        .unwrap_or(false)
}

/// Connect Codex to the gateway. `model` is the model Codex will request (routed by the gateway);
/// `token` is the bearer written inline (a local placeholder unless the gateway enforces a token).
pub fn connect(port: u16, token: &str, model: &str) {
    let mut doc = read_doc();

    // Back up the user's prior model/model_provider exactly once (before we overwrite them).
    let cfg = store::read_config();
    if cfg.get("codexBackup").map(|v| v.is_null()).unwrap_or(true) {
        let prior_model = doc.get("model").and_then(|v| v.as_str()).map(|s| s.to_string());
        let prior_provider = doc.get("model_provider").and_then(|v| v.as_str()).map(|s| s.to_string());
        let backup = json!({
            "model": prior_model.map(Value::String).unwrap_or(Value::Null),
            "model_provider": prior_provider.map(Value::String).unwrap_or(Value::Null),
        });
        let mut next = cfg.clone();
        next["codexBackup"] = backup;
        store::write_config(next);
    }

    // Point Codex at our provider.
    doc["model_provider"] = value(PROVIDER_ID);
    if !model.is_empty() {
        doc["model"] = value(model);
    }

    // Ensure [model_providers] exists as a real table, then set our block.
    if !doc.contains_key("model_providers") {
        doc["model_providers"] = Item::Table(Table::new());
    }
    let mut block = Table::new();
    block.insert("name", value("ccbud"));
    block.insert("base_url", value(gateway_base(port)));
    block.insert("wire_api", value("chat"));
    block.insert("requires_openai_auth", value(false));
    block.insert("experimental_bearer_token", value(token));
    if let Some(mp) = doc["model_providers"].as_table_mut() {
        mp.insert(PROVIDER_ID, Item::Table(block));
    }

    let _ = write_doc(&doc);
}

/// Disconnect Codex: restore the backed-up model/model_provider and remove our provider block.
pub fn disconnect() {
    let cfg = store::read_config();
    let backup = cfg.get("codexBackup").cloned().unwrap_or(Value::Null);
    let mut doc = read_doc();

    // Remove our provider block.
    if let Some(mp) = doc.get_mut("model_providers").and_then(|v| v.as_table_mut()) {
        mp.remove(PROVIDER_ID);
        // Drop the whole table if it's now empty so we don't leave `[model_providers]` dangling.
        if mp.is_empty() {
            doc.as_table_mut().remove("model_providers");
        }
    }

    if backup.is_object() {
        match backup.get("model_provider").cloned().unwrap_or(Value::Null) {
            Value::String(s) => doc["model_provider"] = value(s),
            _ => {
                doc.as_table_mut().remove("model_provider");
            }
        }
        match backup.get("model").cloned().unwrap_or(Value::Null) {
            Value::String(s) => doc["model"] = value(s),
            _ => {
                doc.as_table_mut().remove("model");
            }
        }
        let mut next = cfg.clone();
        next["codexBackup"] = Value::Null;
        store::write_config(next);
    } else {
        // No backup (connected out-of-band): just drop the pointer we would have set.
        if doc.get("model_provider").and_then(|v| v.as_str()) == Some(PROVIDER_ID) {
            doc.as_table_mut().remove("model_provider");
        }
    }

    let _ = write_doc(&doc);
}

#[cfg(test)]
mod tests {
    use super::*;

    // One test (CCBUD_HOME / CCBUD_CODEX_CONFIG are process-global env, so a single sequential test
    // avoids racing other tests on them).
    #[test]
    fn connect_disconnect_round_trip() {
        let dir = std::env::temp_dir().join(format!("ccbud-codexconn-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        std::env::set_var("CCBUD_CODEX_CONFIG", dir.join("config.toml"));
        std::env::set_var("CCBUD_HOME", dir.join("ccbud-home"));

        // --- case 1: pre-existing config with a comment + unrelated setting must survive ---
        fs::write(config_path(), "# my codex config\nmodel = \"gpt-5\"\nmodel_provider = \"openai\"\napproval_policy = \"on-request\"\n").unwrap();
        connect(4321, "ccbud-local", "z-ai/glm-5.2");
        assert!(is_connected(4321));
        let raw = fs::read_to_string(config_path()).unwrap();
        assert!(raw.contains("# my codex config"), "user comment preserved");
        assert!(raw.contains("approval_policy"), "unrelated setting preserved");
        assert!(raw.contains("[model_providers.ccbud]"));
        assert!(raw.contains("base_url = \"http://localhost:4321/v1\""));
        assert!(raw.contains("requires_openai_auth = false"));
        assert!(raw.contains("experimental_bearer_token = \"ccbud-local\""));
        assert!(raw.contains("model_provider = \"ccbud\""));
        assert!(raw.contains("model = \"z-ai/glm-5.2\""));

        disconnect();
        assert!(!is_connected(4321));
        let raw = fs::read_to_string(config_path()).unwrap();
        assert!(!raw.contains("ccbud"), "our block + pointer gone: {}", raw);
        assert!(raw.contains("model = \"gpt-5\""), "prior model restored");
        assert!(raw.contains("model_provider = \"openai\""), "prior provider restored");
        assert!(raw.contains("approval_policy"), "unrelated setting still there");

        // --- case 2: no config file at all → connect creates one, disconnect leaves no ccbud ---
        let _ = fs::remove_file(config_path());
        {
            // clear the backup from case 1 so case 2 records its own (none)
            let mut c = store::read_config();
            c["codexBackup"] = Value::Null;
            store::write_config(c);
        }
        connect(8788, "tok", "m1");
        assert!(is_connected(8788));
        disconnect();
        let raw = fs::read_to_string(config_path()).unwrap_or_default();
        assert!(!raw.contains("ccbud"));

        let _ = fs::remove_dir_all(&dir);
    }
}
