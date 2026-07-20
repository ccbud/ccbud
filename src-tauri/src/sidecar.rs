// Shared JSON sidecar for per-session customization (title / tags / soft-delete) of sessions
// whose source files the app must never rewrite — Codex rollouts and the foreign-CLI stores
// (Grok / Copilot / Antigravity). One JSON map per store file: { "<key>": {title?, tagList?,
// delete?} }, atomic tmp+rename writes, mtime-keyed process cache. Codex keeps its historical
// codex-meta.json (keys = rollout file stems); the newer CLIs share agent-meta.json with
// "<source>:<session-uuid>" keys (their on-disk names — chat_history/events — aren't unique).

use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

pub fn codex_file() -> PathBuf {
    crate::store::ccbud_home().join("codex-meta.json")
}

pub fn agent_file() -> PathBuf {
    crate::store::ccbud_home().join("agent-meta.json")
}

fn cache() -> &'static std::sync::Mutex<HashMap<PathBuf, (f64, Map<String, Value>)>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<HashMap<PathBuf, (f64, Map<String, Value>)>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

fn mtime(path: &PathBuf) -> f64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

fn read(path: &PathBuf) -> Map<String, Value> {
    let mt = mtime(path);
    if let Ok(guard) = cache().lock() {
        if let Some((cmt, map)) = guard.get(path) {
            if *cmt == mt {
                return map.clone();
            }
        }
    }
    let map = fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    if let Ok(mut guard) = cache().lock() {
        guard.insert(path.clone(), (mt, map.clone()));
    }
    map
}

fn write(path: &PathBuf, map: &Map<String, Value>) -> bool {
    let dir = match path.parent() {
        Some(d) => d.to_path_buf(),
        None => return false,
    };
    let _ = fs::create_dir_all(&dir);
    let tmp = path.with_extension("json.tmp");
    let bytes = match serde_json::to_vec_pretty(&Value::Object(map.clone())) {
        Ok(b) => b,
        Err(_) => return false,
    };
    if fs::write(&tmp, bytes).is_err() {
        return false;
    }
    if fs::rename(&tmp, path).is_err() {
        let _ = fs::remove_file(&tmp);
        return false;
    }
    if let Ok(mut guard) = cache().lock() {
        guard.insert(path.clone(), (mtime(path), map.clone()));
    }
    true
}

/// (custom title, tags, deleted) for one key.
pub fn meta(path: &PathBuf, key: &str) -> (Option<String>, Vec<String>, bool) {
    let map = read(path);
    let c = match map.get(key) {
        Some(v) => v,
        None => return (None, vec![], false),
    };
    let title = c
        .get("title")
        .and_then(|t| t.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let tags = c
        .get("tagList")
        .and_then(|t| t.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| t.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let deleted = c.get("delete").and_then(|v| v.as_bool()).unwrap_or(false);
    (title, tags, deleted)
}

/// set_ccbud-equivalent patch ({title?, tags?, delete?}) applied to one key.
pub fn set_meta(path: &PathBuf, key: &str, patch: &Value) -> Value {
    if key.is_empty() {
        return json!({ "ok": false, "reason": "empty" });
    }
    let mut map = read(path);
    let mut next = map
        .get(key)
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    if let Some(t) = patch.get("title") {
        let t = t.as_str().unwrap_or("").trim().to_string();
        if !t.is_empty() {
            next.insert("title".into(), json!(t));
        } else {
            next.remove("title");
        }
    }
    if let Some(tags) = patch.get("tags") {
        let mut arr: Vec<String> = vec![];
        if let Some(ta) = tags.as_array() {
            for x in ta {
                if let Some(s) = x.as_str() {
                    let s = s.trim();
                    if !s.is_empty() && !arr.iter().any(|y| y == s) {
                        arr.push(s.to_string());
                    }
                }
            }
        }
        if !arr.is_empty() {
            next.insert("tagList".into(), json!(arr));
        } else {
            next.remove("tagList");
        }
    }
    if let Some(d) = patch.get("delete") {
        if d.as_bool().unwrap_or(false) {
            next.insert("delete".into(), json!(true));
        } else {
            next.remove("delete");
        }
    }
    if next.is_empty() {
        map.remove(key);
    } else {
        map.insert(key.to_string(), Value::Object(next));
    }
    if write(path, &map) {
        json!({ "ok": true })
    } else {
        json!({ "ok": false, "reason": "write" })
    }
}

/// Drop one key's entry (after its session is deleted forever).
pub fn remove_meta(path: &PathBuf, key: &str) {
    let mut map = read(path);
    if map.remove(key).is_some() {
        let _ = write(path, &map);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_meta_per_key() {
        let path = std::env::temp_dir().join("ccbud-sidecar-test").join("agent-meta.json");
        let _ = fs::remove_file(&path);

        assert_eq!(meta(&path, "grok:u1"), (None, vec![], false));
        let r = set_meta(&path, "grok:u1", &json!({ "title": "改个名", "tags": ["a", "a", "b"] }));
        assert_eq!(r["ok"], true);
        let r = set_meta(&path, "copilot:u2", &json!({ "delete": true }));
        assert_eq!(r["ok"], true);

        let (title, tags, deleted) = meta(&path, "grok:u1");
        assert_eq!(title.as_deref(), Some("改个名"));
        assert_eq!(tags, vec!["a", "b"]);
        assert!(!deleted);
        assert!(meta(&path, "copilot:u2").2);
        // keys are namespaced per source — same uuid under another source stays untouched
        assert_eq!(meta(&path, "antigravity:u1"), (None, vec![], false));

        // restore drops the flag; emptying every field drops the key wholesale
        set_meta(&path, "copilot:u2", &json!({ "delete": false }));
        assert!(!meta(&path, "copilot:u2").2);
        set_meta(&path, "grok:u1", &json!({ "title": "", "tags": [] }));
        assert_eq!(meta(&path, "grok:u1"), (None, vec![], false));

        remove_meta(&path, "copilot:u2");
        assert_eq!(meta(&path, "copilot:u2"), (None, vec![], false));
        assert_eq!(set_meta(&path, "", &json!({ "title": "x" }))["ok"], false);
    }
}
