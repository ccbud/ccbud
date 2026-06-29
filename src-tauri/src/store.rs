// Config persistence — Rust port of src/main/store.js.
//
// All settings live under ~/.ccbud/config.json (override the dir with CCBUD_HOME, used by
// tests/self-check). Writes are atomic (temp file + rename, mode 0600) so a crash mid-write
// never tears the file. `normalize` mirrors store.js exactly so a config written by either
// the Electron build or this one round-trips identically.

use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;

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

pub fn default_config() -> Value {
    json!({
        "port": 8788,
        "activeProviderId": null,
        "requireToken": false,
        "gatewayToken": "",
        "openAtLogin": false,
        "claudeBackup": null,
        "trayUsage": { "enabled": false, "range": "7d" },
        "language": null,
        "historyDirs": ["~/.claude"],
        "historyActive": "all",
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
            let mut np = json!({
                "id": p.get("id").cloned().unwrap_or(Value::Null),
                "name": name,
                "baseUrl": str_of(p.get("baseUrl")),
                "authToken": str_of(p.get("authToken")),
                "defaultModel": str_of(p.get("defaultModel")),
                "smallFastModel": str_of(p.get("smallFastModel")),
                "mapDefaultModels": map_default,
                "models": models,
            });
            if let Some(ic) = p.get("icon").and_then(|v| v.as_str()) {
                if !ic.trim().is_empty() {
                    np.as_object_mut()
                        .unwrap()
                        .insert("icon".into(), json!(ic.trim()));
                }
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

    // historyActive: 'all' | '__imported__' | a configured dir, else 'all'
    let ha = obj.get("historyActive").and_then(|v| v.as_str()).unwrap_or("all").to_string();
    let ha_ok = ha == "all" || ha == "__imported__" || dirs.iter().any(|d| *d == ha);
    obj.insert("historyActive".into(), json!(if ha_ok { ha } else { "all".to_string() }));

    c
}

pub fn read_config() -> Value {
    match fs::read_to_string(config_file()) {
        Ok(s) => match serde_json::from_str::<Value>(&s) {
            Ok(v) => normalize(v),
            Err(_) => default_config(),
        },
        Err(_) => default_config(),
    }
}

pub fn write_config(next: Value) -> Value {
    let normalized = normalize(next);
    let dir = ccbud_home();
    let _ = fs::create_dir_all(&dir);
    let file = config_file();
    let tmp = dir.join("config.json.tmp");
    if let Ok(bytes) = serde_json::to_vec_pretty(&normalized) {
        if fs::write(&tmp, &bytes).is_ok() {
            set_0600(&tmp);
            let _ = fs::rename(&tmp, &file);
            set_0600(&file);
        }
    }
    normalized
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
    }
    #[test]
    fn normalize_clamps_retry() {
        let n = normalize(json!({ "retry429": { "max": 999, "baseMs": 99999 } }));
        assert_eq!(n["retry429"]["max"], 10);
        assert_eq!(n["retry429"]["baseMs"], 10000);
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
