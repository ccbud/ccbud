// Conversation history — Rust port of history.js (browse path).
//
// Reads Claude Code's on-disk sessions across the configured dirs (each a
// `<dir>/projects/<enc-cwd>/<uuid>.jsonl` tree). Implements the BROWSE path the renderer needs:
// list_sessions / list_projects / dir_stats / get_session, plus __ccbud__ custom title+tags.
// TODO (follow-up): subagents nesting, fs.watch live tail, imported-snapshot dir, set_meta writer.

#![allow(dead_code)]

use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

/// Synthetic "recycle bin" bucket id. Not a real projects tree (never in all_dirs /
/// each_session_file) — a cross-cutting view of soft-deleted sessions across every dir.
pub const TRASH_ID: &str = "__trash__";

fn home() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("."))
}

fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        home().join(rest)
    } else if p == "~" {
        home()
    } else {
        PathBuf::from(p)
    }
}

/// Configured dirs → (id, label, projects_dir). id == the dir string (historyActive matches it).
fn config_dirs(config: &Value) -> Vec<(String, String, PathBuf)> {
    let mut out = vec![];
    if let Some(arr) = config.get("historyDirs").and_then(|v| v.as_array()) {
        for d in arr {
            if let Some(s) = d.as_str() {
                out.push((s.to_string(), s.to_string(), expand_tilde(s).join("projects")));
            }
        }
    }
    out
}

fn base_name(p: &str) -> String {
    p.split('/').filter(|s| !s.is_empty()).last().unwrap_or(p).to_string()
}

/// Best-effort decode of an encoded project dir name → cwd (record cwd wins when present).
fn decode_dir_name(name: &str) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    let trimmed = name.trim_start_matches('-');
    Some(format!("/{}", trimmed.replace('-', "/")))
}

fn parse_lines(text: &str) -> Vec<Value> {
    let mut out = vec![];
    for line in text.split('\n') {
        let s = line.trim();
        if s.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(s) {
            out.push(v);
        }
    }
    out
}

fn read_head(file: &Path, max: usize) -> String {
    use std::io::Read;
    let mut f = match fs::File::open(file) {
        Ok(f) => f,
        Err(_) => return String::new(),
    };
    let mut buf = vec![0u8; max];
    let n = f.read(&mut buf).unwrap_or(0);
    buf.truncate(n);
    String::from_utf8_lossy(&buf).into_owned()
}

fn usage_of(u: &Value) -> Value {
    json!({
        "inputTokens": u.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0),
        "outputTokens": u.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0),
        "cacheRead": u.get("cache_read_input_tokens").and_then(|v| v.as_i64()).unwrap_or(0),
        "cacheCreation": u.get("cache_creation_input_tokens").and_then(|v| v.as_i64()).unwrap_or(0),
    })
}

fn content_text(content: &Value) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    if let Some(arr) = content.as_array() {
        return arr
            .iter()
            .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(" ");
    }
    String::new()
}

fn command_label(raw: &str) -> String {
    let name = raw
        .split_once("<command-name>")
        .and_then(|(_, r)| r.split_once("</command-name>"))
        .map(|(n, _)| n.trim().to_string())
        .unwrap_or_default();
    if name.is_empty() {
        return String::new();
    }
    let args = raw
        .split_once("<command-args>")
        .and_then(|(_, r)| r.split_once("</command-args>"))
        .map(|(a, _)| a.trim().to_string())
        .unwrap_or_default();
    format!("{} {}", name, args).trim().to_string()
}

/// First human prose turn (skips slash-command XML / meta / interrupt notices), capped at 90 chars.
fn first_user_text(messages: &[Value]) -> String {
    let mut fallback_cmd = String::new();
    for m in messages {
        if m.get("role").and_then(|r| r.as_str()) != Some("user") {
            continue;
        }
        if m.get("_meta").and_then(|v| v.as_bool()).unwrap_or(false) {
            continue;
        }
        let content = m.get("content").cloned().unwrap_or(Value::Null);
        let raw = content_text(&content);
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        if raw.starts_with('<') {
            if fallback_cmd.is_empty() {
                fallback_cmd = command_label(raw);
            }
            continue;
        }
        let t: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        if t.starts_with("[Request interrupted") || t.starts_with("Caveat:") {
            continue;
        }
        return t.chars().take(90).collect();
    }
    fallback_cmd.chars().take(90).collect()
}

/// __ccbud__ customization (custom title + tags + soft-delete flag) from any record carrying it.
fn read_ccbud(recs: &[Value]) -> (Option<String>, Vec<String>, bool) {
    let c = recs.iter().find_map(|r| r.get("__ccbud__"));
    let title = c
        .and_then(|c| c.get("title"))
        .and_then(|t| t.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let tags = c
        .and_then(|c| c.get("tagList"))
        .and_then(|t| t.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| t.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let deleted = c.and_then(|c| c.get("delete")).and_then(|v| v.as_bool()).unwrap_or(false);
    (title, tags, deleted)
}

/// Process-lifetime memo of soft-delete status, keyed `path -> (mtime, deleted)`. mtime is the
/// invalidation signal: set_ccbud rewrites the file (bumping mtime) whenever the flag flips, so a
/// matching mtime means the cached answer is still valid. This lets dir_stats *stat* unchanged
/// sessions on each refresh instead of re-reading them — only new/changed files get parsed.
fn deleted_cache() -> &'static std::sync::Mutex<std::collections::HashMap<PathBuf, (f64, bool)>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<PathBuf, (f64, bool)>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Cheap soft-delete probe for counting, memoized by mtime. The `__ccbud__.delete` flag rides on the
/// first parseable line, so a small head read suffices (the full meta read happens later in session_meta).
fn is_session_deleted(file: &Path) -> bool {
    let mt = mtime_ms(file);
    if let Ok(cache) = deleted_cache().lock() {
        if let Some(&(cmt, del)) = cache.get(file) {
            if cmt == mt {
                return del;
            }
        }
    }
    let del = read_ccbud(&parse_lines(&read_head(file, 16384))).2;
    if let Ok(mut cache) = deleted_cache().lock() {
        cache.insert(file.to_path_buf(), (mt, del));
    }
    del
}

fn line_to_message(rec: &Value) -> Option<Value> {
    let t = rec.get("type").and_then(|v| v.as_str())?;
    if t != "user" && t != "assistant" {
        return None;
    }
    let m = rec.get("message")?;
    let role = m.get("role").and_then(|v| v.as_str())?;
    let mut out = json!({
        "role": role,
        "content": m.get("content").cloned().unwrap_or(Value::Null),
        "_ts": rec.get("timestamp").cloned().unwrap_or(Value::Null),
        "_sidechain": rec.get("isSidechain").and_then(|v| v.as_bool()).unwrap_or(false),
        "_meta": rec.get("isMeta").and_then(|v| v.as_bool()).unwrap_or(false),
    });
    if t == "assistant" {
        let o = out.as_object_mut().unwrap();
        o.insert("_model".into(), m.get("model").cloned().unwrap_or(Value::Null));
        o.insert("_usage".into(), m.get("usage").map(usage_of).unwrap_or(Value::Null));
        o.insert("_stopReason".into(), m.get("stop_reason").cloned().unwrap_or(Value::Null));
    }
    Some(out)
}

struct Shaped {
    messages: Vec<Value>,
    totals: Value,
    model: Option<String>,
    first_ts: Option<String>,
    last_ts: Option<String>,
}

fn shape_messages(recs: &[Value]) -> Shaped {
    let mut messages = vec![];
    let (mut tin, mut tout, mut tcr, mut tcc, mut turns) = (0i64, 0i64, 0i64, 0i64, 0i64);
    let mut model: Option<String> = None;
    let mut first_ts: Option<String> = None;
    let mut last_ts: Option<String> = None;
    for r in recs {
        let lm = match line_to_message(r) {
            Some(m) => m,
            None => continue,
        };
        if lm.get("_meta").and_then(|v| v.as_bool()).unwrap_or(false) {
            continue;
        }
        let ts = lm.get("_ts").and_then(|v| v.as_str()).map(|s| s.to_string());
        if let Some(t) = &ts {
            if first_ts.is_none() {
                first_ts = Some(t.clone());
            }
            last_ts = Some(t.clone());
        }
        let mut msg = json!({ "role": lm.get("role").cloned().unwrap_or(Value::Null), "content": lm.get("content").cloned().unwrap_or(Value::Null) });
        let mo = msg.as_object_mut().unwrap();
        if lm.get("_sidechain").and_then(|v| v.as_bool()).unwrap_or(false) {
            mo.insert("isSidechain".into(), json!(true));
        }
        if let Some(t) = &ts {
            mo.insert("ts".into(), json!(t));
        }
        if r.get("type").and_then(|v| v.as_str()) == Some("assistant") {
            if let Some(md) = lm.get("_model").and_then(|v| v.as_str()) {
                mo.insert("modelActual".into(), json!(md));
                model = Some(md.to_string());
            }
            let u = lm.get("_usage").cloned().unwrap_or(Value::Null);
            if u.is_object() {
                mo.insert("usage".into(), u.clone());
                tin += u.get("inputTokens").and_then(|v| v.as_i64()).unwrap_or(0);
                tout += u.get("outputTokens").and_then(|v| v.as_i64()).unwrap_or(0);
                tcr += u.get("cacheRead").and_then(|v| v.as_i64()).unwrap_or(0);
                tcc += u.get("cacheCreation").and_then(|v| v.as_i64()).unwrap_or(0);
                turns += 1;
            }
            if let Some(sr) = lm.get("_stopReason").and_then(|v| v.as_str()) {
                mo.insert("stopReason".into(), json!(sr));
            }
        }
        messages.push(msg);
    }
    Shaped {
        messages,
        totals: json!({ "in": tin, "out": tout, "cacheRead": tcr, "cacheCreation": tcc, "turns": turns }),
        model,
        first_ts,
        last_ts,
    }
}

fn mtime_ms(file: &Path) -> f64 {
    fs::metadata(file)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

fn imports_root() -> PathBuf {
    crate::store::ccbud_home().join("imports")
}
/// Configured dirs + the synthetic imported-transcripts store (id `__imported__`).
fn all_dirs(config: &Value) -> Vec<(String, String, PathBuf)> {
    let mut dirs = config_dirs(config);
    dirs.push(("__imported__".to_string(), "导入".to_string(), imports_root().join("projects")));
    dirs
}
/// Projects dirs to watch for live history changes.
pub fn watch_roots(config: &Value) -> Vec<PathBuf> {
    all_dirs(config).into_iter().map(|(_, _, r)| r).collect()
}

/// Walk every session .jsonl across the configured dirs (+ imports), invoking
/// `cb(file, dir_name, dir_id, dir_label)`.
fn each_session_file<F: FnMut(PathBuf, String, &str, &str)>(config: &Value, mut cb: F) {
    for (id, label, root) in all_dirs(config) {
        let entries = match fs::read_dir(&root) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for ent in entries.flatten() {
            if !ent.path().is_dir() {
                continue;
            }
            let dir_name = ent.file_name().to_string_lossy().into_owned();
            let pfiles = match fs::read_dir(ent.path()) {
                Ok(f) => f,
                Err(_) => continue,
            };
            for f in pfiles.flatten() {
                let p = f.path();
                if p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                    cb(p, dir_name.clone(), &id, &label);
                }
            }
        }
    }
}

fn session_meta(file: &Path, dir_name: &str, dir_id: &str, dir_label: &str) -> Option<Value> {
    let meta = fs::metadata(file).ok()?;
    let size = meta.len();
    let head = read_head(file, 131072);
    let recs = parse_lines(&head);
    let meta_rec = recs
        .iter()
        .find(|r| r.get("cwd").is_some())
        .or_else(|| recs.iter().find(|r| r.get("sessionId").is_some()));
    let agent_rec = recs.iter().find(|r| r.get("agentId").is_some());
    let msgs: Vec<Value> = recs.iter().filter_map(line_to_message).collect();
    let (cc_title, cc_tags, cc_deleted) = read_ccbud(&recs);
    let auto_title = first_user_text(&msgs);
    let mut model: Option<String> = None;
    for r in &recs {
        if r.get("type").and_then(|v| v.as_str()) == Some("assistant") {
            if let Some(md) = r.get("message").and_then(|m| m.get("model")).and_then(|v| v.as_str()) {
                model = Some(md.to_string());
            }
        }
    }
    let subagent = agent_rec.is_some();
    let cwd = meta_rec
        .and_then(|r| r.get("cwd"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| decode_dir_name(dir_name));
    let stem = file.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
    let mt = mtime_ms(file);
    Some(json!({
        "id": format!("disk:{}{}", stem, if subagent { ":sub" } else { "" }),
        "file": file.to_string_lossy(),
        "source": "disk",
        "dirId": dir_id,
        "dirLabel": dir_label,
        "sessionId": meta_rec.and_then(|r| r.get("sessionId")).and_then(|v| v.as_str()).unwrap_or(&stem),
        "cwd": cwd.clone(),
        "project": cwd.as_deref().map(base_name).unwrap_or_default(),
        "gitBranch": meta_rec.and_then(|r| r.get("gitBranch")).cloned().unwrap_or(Value::Null),
        "title": cc_title.clone().unwrap_or_else(|| auto_title.clone()),
        "autoTitle": auto_title,
        "tags": cc_tags,
        "model": model,
        "isSubagent": subagent,
        "imported": dir_id == "__imported__",
        "deleted": cc_deleted,
        "lastActivity": mt,
        "sizeKB": (size as f64 / 1024.0).round() as i64,
    }))
}

pub fn list_sessions(config: &Value, active: &str, limit: usize) -> Vec<Value> {
    // The recycle bin spans every dir and shows only soft-deleted sessions; every other view
    // is scoped to its dir and hides them.
    let trash = active == TRASH_ID;
    let mut files: Vec<(PathBuf, String, String, String, f64)> = vec![];
    each_session_file(config, |file, dir_name, id, label| {
        if !trash && active != "all" && id != active {
            return;
        }
        let mt = mtime_ms(&file);
        files.push((file, dir_name, id.to_string(), label.to_string(), mt));
    });
    files.sort_by(|a, b| b.4.partial_cmp(&a.4).unwrap_or(std::cmp::Ordering::Equal));
    // Walk newest-first, reading until `limit` rows land on the right side of the deleted/active
    // partition. Bounds normal-view cost to ~limit reads (deleted sessions are the rare case).
    let mut out: Vec<Value> = Vec::new();
    for (file, dn, id, label, _) in files {
        if out.len() >= limit {
            break;
        }
        if let Some(m) = session_meta(&file, &dn, &id, &label) {
            if m.get("deleted").and_then(|v| v.as_bool()).unwrap_or(false) == trash {
                out.push(m);
            }
        }
    }
    out
}

pub fn list_projects(config: &Value, active: &str) -> Vec<Value> {
    let sessions = list_sessions(config, active, 600);
    let mut order: Vec<String> = vec![];
    let mut groups: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
    for s in sessions {
        let cwd = s.get("cwd").and_then(|v| v.as_str()).unwrap_or("(unknown)").to_string();
        let la = s.get("lastActivity").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let g = groups.entry(cwd.clone()).or_insert_with(|| {
            order.push(cwd.clone());
            json!({ "cwd": s.get("cwd").cloned().unwrap_or(Value::Null), "name": s.get("project").cloned().unwrap_or(Value::Null), "sessions": [], "lastActivity": 0.0 })
        });
        g["sessions"].as_array_mut().unwrap().push(s.clone());
        if la > g["lastActivity"].as_f64().unwrap_or(0.0) {
            g["lastActivity"] = json!(la);
        }
    }
    let mut arr: Vec<Value> = order.into_iter().filter_map(|k| groups.remove(&k)).collect();
    for g in &mut arr {
        g["sessions"].as_array_mut().unwrap().sort_by(|a, b| {
            b.get("lastActivity").and_then(|v| v.as_f64()).unwrap_or(0.0)
                .partial_cmp(&a.get("lastActivity").and_then(|v| v.as_f64()).unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }
    arr.sort_by(|a, b| {
        b.get("lastActivity").and_then(|v| v.as_f64()).unwrap_or(0.0)
            .partial_cmp(&a.get("lastActivity").and_then(|v| v.as_f64()).unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    arr
}

pub fn dir_stats(config: &Value) -> Vec<Value> {
    // Per-dir counts exclude soft-deleted sessions (they're hidden from those views); the deleted
    // ones are tallied separately into the synthetic recycle-bin bucket.
    let mut counts: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    let mut trash = 0i64;
    each_session_file(config, |file, _dn, id, _label| {
        if is_session_deleted(&file) {
            trash += 1;
        } else {
            *counts.entry(id.to_string()).or_insert(0) += 1;
        }
    });
    let mut out: Vec<Value> = all_dirs(config)
        .into_iter()
        .map(|(id, label, pd)| {
            let exists = pd.is_dir();
            let imported = id == "__imported__";
            json!({
                "id": id.clone(), "label": label, "projectsDir": pd.to_string_lossy(),
                "sessions": counts.get(&id).copied().unwrap_or(0), "exists": exists, "imported": imported,
            })
        })
        .collect();
    out.push(json!({
        "id": TRASH_ID, "label": "回收站", "projectsDir": "",
        "sessions": trash, "exists": true, "imported": false, "trash": true,
    }));
    out
}

/// Read a session's child subagent dialogues from `<stem>/subagents/agent-*.jsonl` (+ .meta.json),
/// keyed by the spawning tool_use id so the renderer can nest them. {} when none. (history.js readSubagents)
fn read_subagents(file: &str) -> serde_json::Map<String, Value> {
    let p = Path::new(file);
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let dir = match p.parent() {
        Some(d) => d.join(stem).join("subagents"),
        None => return serde_json::Map::new(),
    };
    let mut by_tool = serde_json::Map::new();
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return by_tool,
    };
    for ent in entries.flatten() {
        let name = ent.file_name().to_string_lossy().to_string();
        if !(name.starts_with("agent-") && name.ends_with(".jsonl")) {
            continue;
        }
        let agent_id = name
            .trim_start_matches("agent-")
            .trim_end_matches(".jsonl")
            .to_string();
        let meta: Value = fs::read_to_string(dir.join(format!("agent-{}.meta.json", agent_id)))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| json!({}));
        let raw = match fs::read_to_string(ent.path()) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let recs = parse_lines(&raw);
        let shaped = shape_messages(&recs);
        let key = meta
            .get("toolUseId")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("agent:{}", agent_id));
        let agent_type = meta
            .get("agentType")
            .and_then(|v| v.as_str())
            .or_else(|| meta.get("subagent_type").and_then(|v| v.as_str()))
            .unwrap_or("agent");
        by_tool.insert(
            key,
            json!({
                "agentId": agent_id,
                "file": ent.path().to_string_lossy(),
                "type": agent_type,
                "description": meta.get("description").and_then(|v| v.as_str()).unwrap_or(""),
                "count": shaped.messages.len(),
                "totals": shaped.totals,
                "messages": shaped.messages,
            }),
        );
    }
    by_tool
}

/// A session's subagents directory: `<dir>/<stem>/subagents`. None when the path has no stem.
fn subagent_dir(file: &Path) -> Option<PathBuf> {
    let stem = file.file_stem().and_then(|s| s.to_str())?;
    file.parent().map(|d| d.join(stem).join("subagents"))
}

/// The raw subagent sidecar files for a session — `(agent-*.jsonl | agent-*.meta.json, bytes)`.
/// Empty when the session spawned no subagents. Shared by bundle export, import, and replay-merge.
fn read_subagent_files(file: &Path) -> Vec<(String, Vec<u8>)> {
    let dir = match subagent_dir(file) {
        Some(d) => d,
        None => return vec![],
    };
    let mut out = vec![];
    if let Ok(entries) = fs::read_dir(&dir) {
        for ent in entries.flatten() {
            let p = ent.path();
            if !p.is_file() {
                continue;
            }
            let name = ent.file_name().to_string_lossy().into_owned();
            let lower = name.to_lowercase();
            if lower.starts_with("agent-") && (lower.ends_with(".jsonl") || lower.ends_with(".meta.json")) {
                if let Ok(bytes) = fs::read(&p) {
                    out.push((name, bytes));
                }
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0)); // deterministic bundle order
    out
}

/// Whether a session has any subagent transcripts (drives export → .zip vs plain .jsonl).
pub fn session_has_subagents(file: &str) -> bool {
    !read_subagent_files(Path::new(file)).is_empty()
}

/// Build a conversation-bundle ZIP: the main session `<basename>.jsonl` at the top level and each
/// subagent file under `subagents/`. Caller uses this only when the session actually has subagents
/// (a plain .jsonl export otherwise). Round-trips through import_zip / splitBundle.
pub fn export_bundle(file: &str) -> std::io::Result<Vec<u8>> {
    let path = Path::new(file);
    let main = fs::read(path)?;
    let main_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("conversation.jsonl")
        .to_string();
    let mut entries = vec![crate::ziputil::Entry { name: main_name, data: main }];
    for (name, bytes) in read_subagent_files(path) {
        entries.push(crate::ziputil::Entry { name: format!("subagents/{}", name), data: bytes });
    }
    Ok(crate::ziputil::build(&entries))
}

/// Merge a session's transcript with its subagent transcripts into one .jsonl (main lines first,
/// then each subagent's lines — which already carry `isSidechain`/`agentId`, so the reader can tell
/// them apart). Returns None when the session has no subagents (caller uses the file as-is). Powers
/// "Claude 分析", so the analysis sees subagent runs, not just the main thread.
pub fn merged_transcript(file: &str) -> Option<Vec<u8>> {
    let path = Path::new(file);
    let subs = read_subagent_files(path);
    if subs.is_empty() {
        return None;
    }
    let mut out = match fs::read(path) {
        Ok(b) => b,
        Err(_) => return None,
    };
    // Only merge the `.jsonl` transcripts, not the `.meta.json` sidecars.
    for (name, bytes) in subs {
        if !name.to_lowercase().ends_with(".jsonl") {
            continue;
        }
        if out.last() != Some(&b'\n') {
            out.push(b'\n');
        }
        out.extend_from_slice(&bytes);
    }
    Some(out)
}

/// Read the import provenance sidecar (`<stem>.import.json`) for an imported transcript.
fn read_import_meta(file: &str) -> Option<Value> {
    let p = Path::new(file);
    let stem = p.file_stem().and_then(|s| s.to_str())?;
    let dir = p.parent()?;
    let raw = fs::read_to_string(dir.join(format!("{}.import.json", stem))).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn get_session(file: &str) -> Value {
    let path = Path::new(file);
    let raw = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return Value::Null,
    };
    let recs = parse_lines(&raw);
    let meta_rec = recs
        .iter()
        .find(|r| r.get("cwd").is_some())
        .or_else(|| recs.iter().find(|r| r.get("sessionId").is_some()));
    let agent_rec = recs.iter().find(|r| r.get("agentId").is_some());
    let agent_id = agent_rec
        .and_then(|r| r.get("agentId"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let summary = recs
        .iter()
        .find(|r| r.get("type").and_then(|v| v.as_str()) == Some("summary") && r.get("summary").is_some())
        .and_then(|r| r.get("summary").cloned());
    let (cc_title, cc_tags, cc_deleted) = read_ccbud(&recs);
    let shaped = shape_messages(&recs);
    let auto_title = first_user_text(&shaped.messages);
    let subagent = agent_rec.is_some();
    let cwd = meta_rec.and_then(|r| r.get("cwd")).and_then(|v| v.as_str()).map(|s| s.to_string());
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
    let base_id = meta_rec
        .and_then(|r| r.get("sessionId"))
        .and_then(|v| v.as_str())
        .unwrap_or(&stem)
        .to_string();
    // A subagent session's id carries the agent suffix; only a top-level session embeds subagents.
    let sess_id = match (subagent, &agent_id) {
        (true, Some(aid)) => format!("{}-{}", base_id, aid),
        _ => base_id.clone(),
    };
    let subs = if subagent { serde_json::Map::new() } else { read_subagents(file) };
    let import_meta = read_import_meta(file);

    json!({
        "meta": {
            "id": format!("disk:{}{}", stem, if subagent { ":sub" } else { "" }),
            "file": file,
            "source": "disk",
            "title": cc_title.clone().unwrap_or_else(|| auto_title.clone()),
            "autoTitle": auto_title,
            "tags": cc_tags,
            "summary": summary,
            "sessionId": sess_id,
            "cwd": cwd.clone(),
            "project": cwd.as_deref().map(base_name).unwrap_or_default(),
            "gitBranch": meta_rec.and_then(|r| r.get("gitBranch")).cloned().unwrap_or(Value::Null),
            "version": meta_rec.and_then(|r| r.get("version")).cloned().unwrap_or(Value::Null),
            "isSubagent": subagent,
            "deleted": cc_deleted,
            "imported": import_meta.is_some(),
            "importedFrom": import_meta.as_ref().and_then(|m| m.get("originalPath")).cloned().unwrap_or(Value::Null),
            "importedAt": import_meta.as_ref().and_then(|m| m.get("importedAt")).cloned().unwrap_or(Value::Null),
            "model": shaped.model,
            "totals": shaped.totals,
            "messages": shaped.messages.len(),
            "subagentCount": subs.len(),
            "firstTs": shaped.first_ts,
            "lastTs": shaped.last_ts,
        },
        "messages": shaped.messages,
        "subagents": subs,
    })
}

/// Write per-conversation customization (custom title + tags) onto the FIRST parseable line as a
/// `__ccbud__` field. Atomic (tmp + rename). Guarded to the configured dirs + the imports store
/// (renderer can't drive an arbitrary-path write, but imported sessions must be titleable/taggable
/// too — mirrors history.js setCcbud, whose getDirs() includes the imported dir).
pub fn set_ccbud(file: &str, patch: &Value, config: &Value) -> Value {
    let target = Path::new(file);
    let within = all_dirs(config).iter().any(|(_, _, pd)| target.starts_with(pd));
    if !within {
        return json!({ "ok": false, "reason": "out-of-scope" });
    }
    let raw = match fs::read_to_string(file) {
        Ok(s) => s,
        Err(_) => return json!({ "ok": false, "reason": "read" }),
    };
    let mut lines: Vec<String> = raw.split('\n').map(|s| s.to_string()).collect();
    let mut found: Option<(usize, Value)> = None;
    for (i, l) in lines.iter().enumerate() {
        let s = l.trim();
        if s.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(s) {
            if v.is_object() {
                found = Some((i, v));
                break;
            }
        }
    }
    let (idx, mut obj) = match found {
        Some(x) => x,
        None => return json!({ "ok": false, "reason": "empty" }),
    };
    let mut next = obj.get("__ccbud__").and_then(|v| v.as_object()).cloned().unwrap_or_default();
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
    // Soft delete / restore: `delete: true` marks the session deleted; `delete: false` (restore)
    // drops the flag. Restore that empties __ccbud__ removes the field wholesale below.
    if let Some(d) = patch.get("delete") {
        if d.as_bool().unwrap_or(false) {
            next.insert("delete".into(), json!(true));
        } else {
            next.remove("delete");
        }
    }
    let o = obj.as_object_mut().unwrap();
    if !next.is_empty() {
        o.insert("__ccbud__".into(), Value::Object(next));
    } else {
        o.remove("__ccbud__");
    }
    lines[idx] = serde_json::to_string(&obj).unwrap_or_default();
    let out = lines.join("\n");
    let tmp = format!("{}.ccbud.tmp", file);
    if fs::write(&tmp, &out).is_err() {
        return json!({ "ok": false, "reason": "write" });
    }
    if fs::rename(&tmp, file).is_err() {
        let _ = fs::remove_file(&tmp);
        return json!({ "ok": false, "reason": "write" });
    }
    json!({ "ok": true })
}

/// Permanently remove a session's .jsonl from disk (recycle-bin "delete forever"). Guarded to the
/// configured dirs + the imports store exactly like set_ccbud, and also drops the session's
/// `<stem>/` subagents tree and any import sidecar (mirrors remove_import's cleanup).
pub fn delete_session_file(file: &str, config: &Value) -> Value {
    let target = Path::new(file);
    let within = all_dirs(config).iter().any(|(_, _, pd)| target.starts_with(pd));
    if !within {
        return json!({ "ok": false, "reason": "out-of-scope" });
    }
    if !target.is_file() {
        return json!({ "ok": false, "reason": "missing" });
    }
    if fs::remove_file(target).is_err() {
        return json!({ "ok": false, "reason": "remove" });
    }
    let dir = target.parent().unwrap_or(Path::new("."));
    let stem = target.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    if !stem.is_empty() {
        let _ = fs::remove_dir_all(dir.join(stem)); // <stem>/subagents/...
        let _ = fs::remove_file(dir.join(format!("{}.import.json", stem)));
    }
    json!({ "ok": true })
}

/// Self-contained round-trip test of set_ccbud + get_session in a throwaway projects tree.
pub fn history_selftest(base_dir: &Path) -> Value {
    let proj = base_dir.join("test-claude").join("projects").join("-test-cwd");
    let _ = fs::create_dir_all(&proj);
    let file = proj.join("sess1.jsonl");
    let content = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hello world from selfcheck\"},\"cwd\":\"/test/cwd\",\"sessionId\":\"sess1\",\"timestamp\":\"2025-01-01T10:00:00.000Z\"}\n";
    let _ = fs::write(&file, content);
    let config = json!({ "historyDirs": [ base_dir.join("test-claude").to_string_lossy() ] });
    let fpath = file.to_string_lossy().to_string();
    let set = set_ccbud(&fpath, &json!({ "title": "My Title", "tags": ["a", "b", "b"] }), &config);
    let sess = get_session(&fpath);
    let title = sess.get("meta").and_then(|m| m.get("title")).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let tags = sess.get("meta").and_then(|m| m.get("tags")).and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
    let auto = sess.get("meta").and_then(|m| m.get("autoTitle")).and_then(|v| v.as_str()).unwrap_or("").to_string();
    // Soft-delete round-trip: marked → hidden from "all" but present in trash → restored → back in "all".
    let _ = set_ccbud(&fpath, &json!({ "delete": true }), &config);
    let after_del = get_session(&fpath).get("meta").and_then(|m| m.get("deleted")).and_then(|v| v.as_bool()).unwrap_or(false);
    let hidden_in_all = !list_sessions(&config, "all", 50).iter().any(|s| s.get("file").and_then(|v| v.as_str()) == Some(fpath.as_str()));
    let shown_in_trash = list_sessions(&config, TRASH_ID, 50).iter().any(|s| s.get("file").and_then(|v| v.as_str()) == Some(fpath.as_str()));
    let _ = set_ccbud(&fpath, &json!({ "delete": false }), &config);
    let restored = !get_session(&fpath).get("meta").and_then(|m| m.get("deleted")).and_then(|v| v.as_bool()).unwrap_or(false);
    json!({
        "setOk": set.get("ok").and_then(|v| v.as_bool()).unwrap_or(false),
        "title": title,
        "tagCount": tags,
        "autoTitle": auto,
        "deletedAfterMark": after_del,
        "hiddenInAll": hidden_in_all,
        "shownInTrash": shown_in_trash,
        "restored": restored,
    })
}

// ---- import (copy someone else's .jsonl into the app-managed store) ----

fn encode_cwd(cwd: Option<&str>) -> String {
    match cwd {
        Some(c) if !c.is_empty() => c.replace(['/', '\\'], "-"),
        _ => "-imported".to_string(),
    }
}

fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for e in fs::read_dir(src)? {
        let e = e?;
        let s = e.path();
        let d = dst.join(e.file_name());
        if s.is_dir() {
            copy_dir(&s, &d)?;
        } else {
            fs::copy(&s, &d)?;
        }
    }
    Ok(())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Snapshot a transcript (already read into `raw`) plus its subagent sidecars into the import store,
/// laid out like a native projects/ tree + a provenance sidecar. `subagents`: (filename, bytes) to
/// drop under `<baseId>/subagents/` — names are basename-reduced and pattern-checked so a crafted
/// entry can't escape the directory. Returns 1 = imported, 2 = skipped (already present),
/// 0 = failed/not-a-transcript. Shared by the plain-.jsonl and .zip-bundle import paths.
fn write_imported(raw: &str, original_path: &str, original_name: &str, subagents: &[(String, Vec<u8>)]) -> i32 {
    let recs = parse_lines(raw);
    let has_msg = recs.iter().any(|r| {
        let t = r.get("type").and_then(|v| v.as_str());
        (t == Some("user") || t == Some("assistant")) && r.get("message").is_some()
    });
    if !has_msg {
        return 0;
    }
    let meta_rec = recs.iter().find(|r| r.get("cwd").is_some()).or_else(|| recs.iter().find(|r| r.get("sessionId").is_some()));
    let cwd = meta_rec.and_then(|r| r.get("cwd")).and_then(|v| v.as_str());
    let base_id = meta_rec
        .and_then(|r| r.get("sessionId"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| Path::new(original_name).file_stem().and_then(|s| s.to_str()).unwrap_or("import").to_string());
    let dest_dir = imports_root().join("projects").join(encode_cwd(cwd));
    let dest_file = dest_dir.join(format!("{}.jsonl", base_id));
    if dest_file.exists() {
        return 2;
    }
    if fs::create_dir_all(&dest_dir).is_err() || fs::write(&dest_file, raw).is_err() {
        return 0;
    }
    if !subagents.is_empty() {
        let sub_dir = dest_dir.join(&base_id).join("subagents");
        if fs::create_dir_all(&sub_dir).is_ok() {
            for (name, bytes) in subagents {
                // file_name() strips any directory component, so the write can't escape sub_dir.
                let safe = Path::new(name).file_name().and_then(|n| n.to_str()).unwrap_or("");
                let lower = safe.to_lowercase();
                if lower.starts_with("agent-") && (lower.ends_with(".jsonl") || lower.ends_with(".meta.json")) {
                    let _ = fs::write(sub_dir.join(safe), bytes);
                }
            }
        }
    }
    let sidecar = dest_dir.join(format!("{}.import.json", base_id));
    let _ = fs::write(
        &sidecar,
        serde_json::to_vec_pretty(&json!({
            "originalPath": original_path,
            "originalName": original_name,
            "sessionId": base_id,
            "importedAt": now_ms(),
        }))
        .unwrap_or_default(),
    );
    1
}

/// Import a plain .jsonl transcript, bringing along its on-disk subagents dir if present.
fn import_one(src: &str) -> i32 {
    let raw = match fs::read_to_string(src) {
        Ok(s) => s,
        Err(_) => return 0,
    };
    let src_path = Path::new(src);
    let subs = read_subagent_files(src_path);
    let original_name = src_path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    write_imported(&raw, src, original_name, &subs)
}

/// Import a conversation-bundle .zip (main session + `subagents/`), restoring the subagent layout so
/// the pipeline nests them exactly as if they'd been captured live. Round-trips export_bundle.
fn import_zip(src: &str) -> i32 {
    let bytes = match fs::read(src) {
        Ok(b) => b,
        Err(_) => return 0,
    };
    let (main, subs) = crate::ziputil::split_bundle(crate::ziputil::read(&bytes));
    let main_data = match main {
        Some((_, data)) => data,
        None => return 0,
    };
    let raw = match String::from_utf8(main_data) {
        Ok(s) => s,
        Err(_) => return 0,
    };
    let original_name = Path::new(src).file_name().and_then(|n| n.to_str()).unwrap_or("");
    write_imported(&raw, src, original_name, &subs)
}

pub fn import_paths(paths: &[String]) -> Value {
    let (mut imported, mut skipped, mut failed) = (0, 0, 0);
    for src in paths {
        let lower = src.to_lowercase();
        let r = if lower.ends_with(".zip") {
            import_zip(src)
        } else if lower.ends_with(".jsonl") {
            import_one(src)
        } else {
            0
        };
        match r {
            1 => imported += 1,
            2 => skipped += 1,
            _ => failed += 1,
        }
    }
    json!({ "imported": imported, "skipped": skipped, "failed": failed })
}

pub fn remove_import(file: &str) -> Value {
    let root = imports_root();
    let f = Path::new(file);
    // Hard safety: only ever delete inside our own import store.
    if !f.starts_with(&root) {
        return json!({ "ok": false, "error": "outside import store" });
    }
    let dir = f.parent().unwrap_or(Path::new("."));
    let base = f.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let _ = fs::remove_file(f);
    let _ = fs::remove_file(dir.join(format!("{}.import.json", base)));
    let _ = fs::remove_dir_all(dir.join(base)); // subagents/
    json!({ "ok": true })
}

/// Self-contained test of import → list-as-imported → re-import-skip → remove.
pub fn import_selftest(base_dir: &Path) -> Value {
    std::env::set_var("CCBUD_HOME", base_dir); // imports_root() honors CCBUD_HOME
    let src_dir = base_dir.join("import-src");
    let _ = fs::create_dir_all(&src_dir);
    let src = src_dir.join("foreign.jsonl");
    let _ = fs::write(&src, "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"imported hello\"},\"cwd\":\"/imp/cwd\",\"sessionId\":\"impsess\",\"timestamp\":\"2025-01-01T10:00:00.000Z\"}\n");
    let srcs = vec![src.to_string_lossy().to_string()];
    let r = import_paths(&srcs);
    let r2 = import_paths(&srcs);
    let config = json!({ "historyDirs": ["~/.claude"] });
    let sessions = list_sessions(&config, "__imported__", 50);
    let found = sessions.iter().any(|s| {
        s.get("imported").and_then(|v| v.as_bool()).unwrap_or(false)
            && s.get("title").and_then(|v| v.as_str()) == Some("imported hello")
    });
    let dest = imports_root().join("projects").join("-imp-cwd").join("impsess.jsonl");
    let rm = remove_import(&dest.to_string_lossy());

    // ---- bundle round-trip: a session WITH subagents exports as a .zip and re-imports with its
    // subagent transcripts restored (the export → import path the 对话 view drives). ----
    let bproj = base_dir.join("bundle-src").join("projects").join("-bnd-cwd");
    let _ = fs::create_dir_all(&bproj);
    let bmain = bproj.join("bundsess.jsonl");
    let _ = fs::write(&bmain, "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"tu9\",\"name\":\"Task\",\"input\":{}}]},\"cwd\":\"/bnd/cwd\",\"sessionId\":\"bundsess\",\"timestamp\":\"2025-01-01T10:00:00.000Z\"}\n");
    let bsub = bproj.join("bundsess").join("subagents");
    let _ = fs::create_dir_all(&bsub);
    let _ = fs::write(bsub.join("agent-b1.jsonl"), "{\"type\":\"assistant\",\"isSidechain\":true,\"agentId\":\"b1\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"sub done\"}]},\"sessionId\":\"bundsess\",\"timestamp\":\"2025-01-01T10:00:01.000Z\"}\n");
    let _ = fs::write(bsub.join("agent-b1.meta.json"), "{\"agentType\":\"general-purpose\",\"description\":\"d\",\"toolUseId\":\"tu9\"}");
    let zip = export_bundle(&bmain.to_string_lossy()).unwrap_or_default();
    let zip_is_zip = zip.starts_with(&[0x50, 0x4b, 0x03, 0x04]);
    let zip_path = base_dir.join("bundle-src").join("bundsess.zip");
    let _ = fs::write(&zip_path, &zip);
    let rb = import_paths(&[zip_path.to_string_lossy().to_string()]);
    let imp_dir = imports_root().join("projects").join("-bnd-cwd");
    let sub_restored = imp_dir.join("bundsess").join("subagents").join("agent-b1.jsonl").exists()
        && imp_dir.join("bundsess").join("subagents").join("agent-b1.meta.json").exists();
    let bundle_sess = get_session(&imp_dir.join("bundsess.jsonl").to_string_lossy());
    let bundle_sub_count = bundle_sess.get("meta").and_then(|m| m.get("subagentCount")).and_then(|v| v.as_i64()).unwrap_or(0);

    json!({
        "imported": r.get("imported"),
        "reskipped": r2.get("skipped"),
        "appearsImported": found,
        "removed": rm.get("ok"),
        "gone": !dest.exists(),
        "bundleZip": zip_is_zip,
        "bundleImported": rb.get("imported"),
        "bundleSubRestored": sub_restored,
        "bundleSubagentCount": bundle_sub_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Export a session-with-subagents and prove the .zip splits back into the main session + both
    // subagent sidecars (the shape import_zip then writes into the store). Avoids mutating CCBUD_HOME
    // so it can't race other threads under `cargo test`; the store round-trip is covered by the
    // in-app import_selftest and confirms in review via write_imported (shared with import_one).
    #[test]
    fn export_bundle_round_trips_through_split() {
        let base = std::env::temp_dir().join("ccbud-bundle-test");
        let _ = fs::remove_dir_all(&base);
        let proj = base.join("projects").join("-bnd-cwd");
        fs::create_dir_all(&proj).unwrap();
        let main = proj.join("bundsess.jsonl");
        fs::write(&main, "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"tu9\",\"name\":\"Task\",\"input\":{}}]},\"cwd\":\"/bnd/cwd\",\"sessionId\":\"bundsess\",\"timestamp\":\"2025-01-01T10:00:00.000Z\"}\n").unwrap();
        let sub = proj.join("bundsess").join("subagents");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("agent-b1.jsonl"), "{\"type\":\"assistant\",\"isSidechain\":true,\"agentId\":\"b1\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"sub done\"}]},\"sessionId\":\"bundsess\",\"timestamp\":\"2025-01-01T10:00:01.000Z\"}\n").unwrap();
        fs::write(sub.join("agent-b1.meta.json"), "{\"agentType\":\"general-purpose\",\"description\":\"d\",\"toolUseId\":\"tu9\"}").unwrap();

        assert!(session_has_subagents(&main.to_string_lossy()));

        let zip = export_bundle(&main.to_string_lossy()).unwrap();
        assert!(zip.starts_with(&[0x50, 0x4b, 0x03, 0x04]), "starts with PK local header");

        let (m, subs) = crate::ziputil::split_bundle(crate::ziputil::read(&zip));
        assert_eq!(m.as_ref().map(|(n, _)| n.as_str()), Some("bundsess.jsonl"));
        assert_eq!(subs.len(), 2);
        assert!(subs.iter().any(|(n, d)| n == "agent-b1.jsonl" && String::from_utf8_lossy(d).contains("sub done")));
        assert!(subs.iter().any(|(n, _)| n == "agent-b1.meta.json"));

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn merged_transcript_appends_subagent_lines() {
        let base = std::env::temp_dir().join("ccbud-merge-test");
        let _ = fs::remove_dir_all(&base);
        let proj = base.join("projects").join("-m-cwd");
        fs::create_dir_all(&proj).unwrap();
        let main = proj.join("m.jsonl");
        fs::write(&main, "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"},\"sessionId\":\"m\"}\n").unwrap();
        // no subagents → None (caller uses the file as-is)
        assert!(merged_transcript(&main.to_string_lossy()).is_none());

        let sub = proj.join("m").join("subagents");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("agent-z.jsonl"), "{\"type\":\"assistant\",\"isSidechain\":true,\"agentId\":\"z\",\"message\":{\"role\":\"assistant\",\"content\":\"sub line\"}}\n").unwrap();
        fs::write(sub.join("agent-z.meta.json"), "{\"toolUseId\":\"t\"}").unwrap();

        let merged = merged_transcript(&main.to_string_lossy()).expect("has subagents");
        let text = String::from_utf8(merged).unwrap();
        assert!(text.contains("\"content\":\"hi\""), "keeps main lines");
        assert!(text.contains("sub line"), "appends subagent transcript");
        assert!(!text.contains("\"toolUseId\":\"t\""), "does not merge the .meta.json sidecar");

        let _ = fs::remove_dir_all(&base);
    }
}
