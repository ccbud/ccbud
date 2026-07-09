// Conversation history.
//
// Reads Claude Code and Codex on-disk sessions across configured dirs, imported snapshots, and
// the app-managed recycle bin. Shapes list/detail payloads for the renderer, including subagents,
// custom title/tags/delete metadata, bundle import/export helpers, and live-watch roots.

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

pub(crate) fn base_name(p: &str) -> String {
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

pub(crate) fn parse_lines(text: &str) -> Vec<Value> {
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
pub(crate) fn first_user_text(messages: &[Value]) -> String {
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
pub(crate) fn read_ccbud(recs: &[Value]) -> (Option<String>, Vec<String>, bool) {
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

/// Cached soft-delete verdict for one file: a Claude session's flag (rides its first line, so it's
/// final for a given mtime), or "this is a Codex rollout" (whose flag lives in the sidecar and can
/// flip WITHOUT touching the file — so only the format verdict is cached, never the flag).
#[derive(Clone, Copy)]
enum DelKind {
    Claude(bool),
    Codex,
}

/// Process-lifetime memo of soft-delete status, keyed `path -> (mtime, kind)`. mtime is the
/// invalidation signal: set_ccbud rewrites a Claude file (bumping mtime) whenever the flag flips,
/// so a matching mtime means the cached answer is still valid. This lets dir_stats *stat*
/// unchanged sessions on each refresh instead of re-reading them.
fn deleted_cache() -> &'static std::sync::Mutex<std::collections::HashMap<PathBuf, (f64, DelKind)>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<PathBuf, (f64, DelKind)>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Cheap soft-delete probe for counting, memoized by mtime. A Claude session's `__ccbud__.delete`
/// rides on the first parseable line, so a small head read suffices; a Codex rollout's flag is
/// re-read from the sidecar every time (itself mtime-cached and cheap).
fn is_session_deleted(file: &Path) -> bool {
    let mt = mtime_ms(file);
    let cached: Option<DelKind> = deleted_cache()
        .lock()
        .ok()
        .and_then(|c| c.get(file).filter(|(cmt, _)| *cmt == mt).map(|(_, k)| *k));
    let kind = cached.unwrap_or_else(|| {
        // Read the same window session_meta uses: a Codex rollout's first (session_meta) line
        // embeds the full system prompt (~22 KB), so a smaller head truncates it, parse yields
        // nothing, and the session mis-sniffs as Claude — desyncing dir vs trash counts.
        let recs = parse_lines(&read_head(file, 131072));
        // Imported codex COPIES carry the flag in-file like Claude sessions (see set_ccbud) —
        // only live rollouts (no .import.json) use the sidecar.
        let kind = if crate::codex::looks_codex(&recs) && read_import_meta(&file.to_string_lossy()).is_none() {
            DelKind::Codex
        } else {
            DelKind::Claude(read_ccbud(&recs).2)
        };
        if let Ok(mut cache) = deleted_cache().lock() {
            cache.insert(file.to_path_buf(), (mt, kind));
        }
        kind
    });
    match kind {
        DelKind::Claude(del) => del,
        DelKind::Codex => crate::codex::is_deleted(file),
    }
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

/// File creation (birth) time in ms; mtime on filesystems that don't record one. NOT stable
/// across a title/tag edit — set_ccbud rewrites via tmp+rename, which gives the path the tmp
/// file's (fresh) birth time — so this is only the FALLBACK sort key when a session's records
/// carry no timestamp; record_created_ms is the real one.
pub(crate) fn created_ms(file: &Path) -> f64 {
    fs::metadata(file)
        .and_then(|m| m.created())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as f64)
        .filter(|v| *v > 0.0)
        .unwrap_or_else(|| mtime_ms(file))
}

/// Session creation time for ORDERING: the first record's timestamp, i.e. content-derived and
/// therefore immune to file rewrites — renaming/tagging a conversation (tmp+rename resets the
/// fs birth time) must never reshuffle the list. Falls back to fs times when no record carries
/// a timestamp. Claude records and Codex rollout lines both put `timestamp` at the top level.
pub(crate) fn record_created_ms(recs: &[Value], file: &Path) -> f64 {
    for r in recs {
        if let Some(ts) = r.get("timestamp").and_then(|v| v.as_str()) {
            if let Ok(d) = chrono::DateTime::parse_from_rfc3339(ts) {
                return d.timestamp_millis() as f64;
            }
        }
    }
    created_ms(file)
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
/// A dir entry's Codex data tree: sibling `sessions/` next to its `projects/` — every work dir
/// is probed for BOTH layouts (Claude Code writes `<dir>/projects/…`, Codex `<dir>/sessions/…`),
/// so `~/.codex` is just another configured dir rather than a special case.
fn sessions_dir(projects_dir: &Path) -> Option<PathBuf> {
    projects_dir.parent().map(|b| b.join("sessions"))
}

/// Dirs to watch for live history changes — each work dir's projects/ AND sessions/ tree.
pub fn watch_roots(config: &Value) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = vec![];
    for (_, _, pd) in all_dirs(config) {
        if let Some(sd) = sessions_dir(&pd) {
            roots.push(sd);
        }
        roots.push(pd);
    }
    roots
}

/// Walk every session .jsonl across the configured dirs (+ imports), invoking
/// `cb(file, dir_name, dir_id, dir_label)` — both the Claude projects/ tree and the
/// Codex sessions/ tree of each dir.
fn each_session_file<F: FnMut(PathBuf, String, &str, &str)>(config: &Value, mut cb: F) {
    for (id, label, root) in all_dirs(config) {
        if let Ok(entries) = fs::read_dir(&root) {
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
        // Codex rollouts live in a date-sharded sessions/ tree, so it gets its own walk.
        if let Some(sd) = sessions_dir(&root) {
            crate::codex::walk_sessions(&sd, |p| cb(p, String::new(), &id, &label));
        }
    }
}

fn session_meta(file: &Path, dir_name: &str, dir_id: &str, dir_label: &str) -> Option<Value> {
    let meta = fs::metadata(file).ok()?;
    let size = meta.len();
    let head = read_head(file, 131072);
    let recs = parse_lines(&head);
    // Codex rollouts (a dir's sessions/ tree, or snapshots imported into the app store) list
    // through the codex shaper — the record format shares nothing with Claude's.
    if crate::codex::looks_codex(&recs) {
        return crate::codex::session_meta_from(file, &recs, dir_id, dir_label);
    }
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
        "createdAt": record_created_ms(&recs, file),
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
        // Pre-sort by fs creation time — only a read-bounding heuristic for the limit walk below;
        // the returned order is re-keyed on the content-derived createdAt.
        let ct = created_ms(&file);
        files.push((file, dir_name, id.to_string(), label.to_string(), ct));
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
    // Final order: the session's OWN creation time (first record timestamp), newest first — stable
    // across title/tag edits, which rewrite the file and reset its fs birth time.
    let key = |v: &Value| v.get("createdAt").and_then(|x| x.as_f64()).unwrap_or(0.0);
    out.sort_by(|a, b| key(b).partial_cmp(&key(a)).unwrap_or(std::cmp::Ordering::Equal));
    out
}

pub fn list_projects(config: &Value, active: &str) -> Vec<Value> {
    let sessions = list_sessions(config, active, 600);
    let mut order: Vec<String> = vec![];
    let mut groups: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
    for s in sessions {
        let cwd = s.get("cwd").and_then(|v| v.as_str()).unwrap_or("(unknown)").to_string();
        let la = s.get("lastActivity").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let ct = s.get("createdAt").and_then(|v| v.as_f64()).unwrap_or(la);
        let g = groups.entry(cwd.clone()).or_insert_with(|| {
            order.push(cwd.clone());
            json!({ "cwd": s.get("cwd").cloned().unwrap_or(Value::Null), "name": s.get("project").cloned().unwrap_or(Value::Null), "sessions": [], "lastActivity": 0.0, "createdAt": 0.0 })
        });
        g["sessions"].as_array_mut().unwrap().push(s.clone());
        if la > g["lastActivity"].as_f64().unwrap_or(0.0) {
            g["lastActivity"] = json!(la);
        }
        if ct > g["createdAt"].as_f64().unwrap_or(0.0) {
            g["createdAt"] = json!(ct);
        }
    }
    // Sort sessions + groups by creation time (stable across tag/title edits), newest first.
    let sort_key = |v: &Value| v.get("createdAt").and_then(|x| x.as_f64()).unwrap_or(0.0);
    let mut arr: Vec<Value> = order.into_iter().filter_map(|k| groups.remove(&k)).collect();
    for g in &mut arr {
        g["sessions"].as_array_mut().unwrap().sort_by(|a, b| {
            sort_key(b).partial_cmp(&sort_key(a)).unwrap_or(std::cmp::Ordering::Equal)
        });
    }
    arr.sort_by(|a, b| sort_key(b).partial_cmp(&sort_key(a)).unwrap_or(std::cmp::Ordering::Equal));
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
            // A dir "exists" when EITHER data tree is on disk — ~/.codex has only sessions/.
            let exists = pd.is_dir() || sessions_dir(&pd).map(|s| s.is_dir()).unwrap_or(false);
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

/// Absolute paths of a session's subagent transcripts (`<stem>/subagents/agent-*.jsonl`), sorted.
/// Empty when the session has no subagents. Powers "Claude 分析": every subagent transcript is
/// attached alongside the main session in the Cowork deep link (which takes a repeated `file=` param),
/// so the analysis covers subagent runs — not just the main thread.
pub fn subagent_transcript_paths(file: &str) -> Vec<String> {
    let dir = match subagent_dir(Path::new(file)) {
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
            let name = ent.file_name().to_string_lossy().to_lowercase();
            if name.starts_with("agent-") && name.ends_with(".jsonl") {
                out.push(p.to_string_lossy().into_owned());
            }
        }
    }
    out.sort();
    out
}

/// Read the import provenance sidecar (`<stem>.import.json`) for an imported transcript.
pub(crate) fn read_import_meta(file: &str) -> Option<Value> {
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
    if crate::codex::looks_codex(&recs) {
        return crate::codex::session_from_recs(file, &recs);
    }
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

// ---- content search (the session list's "big search") ----
//
// Scans session CONTENT (message text / thinking / tool calls + results) across every listed
// session — main threads, their subagent transcripts, and Codex rollouts — and reports, per
// matching session, WHERE the first match lives ("main" or a subagent's tool_use key) plus a
// display snippet. The renderer opens the session, switches the panel to that agent, and
// re-finds the query locally, so list hits and in-conversation positioning stay aligned.
//
// Performance model (this runs per keystroke, debounced):
//  - extraction cache: path -> (mtime, size, extracted text), so repeated queries pay the JSON
//    parse + shaping once per file version;
//  - raw prefilter: on a cache miss the raw JSONL bytes are substring-scanned first, and only
//    files that could match are parsed at all (JSON escapes quotes/backslashes/control chars,
//    so the prefilter is skipped for queries containing those);
//  - parallel scan: per-file work fans out over a small thread pool.

/// ASCII-case-insensitive substring search (byte-wise; non-ASCII must match exactly — CJK has no
/// case). A valid-UTF-8 needle can only match at char boundaries of valid-UTF-8 text (ASCII bytes
/// never equal continuation bytes), so the returned byte offset is safe to slice on.
fn ifind(hay: &str, needle: &str, from: usize) -> Option<usize> {
    let h = hay.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() || h.len() < n.len() {
        return None;
    }
    let last = h.len() - n.len();
    let n0 = n[0].to_ascii_lowercase();
    let mut i = from;
    while i <= last {
        if h[i].to_ascii_lowercase() == n0 {
            let mut k = 1;
            while k < n.len() && h[i + k].to_ascii_lowercase() == n[k].to_ascii_lowercase() {
                k += 1;
            }
            if k == n.len() {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Non-overlapping case-insensitive occurrence count (same fold as ifind).
fn icount(hay: &str, needle: &str) -> usize {
    let (mut i, mut c) = (0usize, 0usize);
    while let Some(p) = ifind(hay, needle, i) {
        c += 1;
        i = p + needle.len().max(1);
    }
    c
}

/// Strip harness-injected blocks from user prose — mirrors the renderer's stripInjected, so what
/// the big search matches is exactly what the in-conversation search (and the panel) will show.
fn strip_injected(s: &str) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(
            r"(?s)<system-reminder>.*?</system-reminder>|<command-[a-z-]+>.*?</command-[a-z-]+>|<local-command-[a-z]+>.*?</local-command-[a-z]+>",
        )
        .unwrap()
    });
    re.replace_all(s, "").trim().to_string()
}

fn tool_result_search_text(c: &Value) -> String {
    if let Some(s) = c.as_str() {
        return s.to_string();
    }
    if let Some(arr) = c.as_array() {
        return arr
            .iter()
            .filter_map(|x| x.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n");
    }
    String::new()
}

/// One searchable text blob for a shaped message list — the renderer's messagePlainText, flattened:
/// user prose (injected blocks stripped), assistant text, thinking, tool name + input JSON, and
/// tool results. Images and raw structure are skipped so a hit here is findable in the panel.
fn extract_search_text(messages: &[Value]) -> String {
    let mut out = String::new();
    let mut push = |t: &str| {
        if !t.is_empty() {
            out.push_str(t);
            out.push('\n');
        }
    };
    for m in messages {
        let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("");
        let content = match m.get("content") {
            Some(c) => c,
            None => continue,
        };
        if let Some(s) = content.as_str() {
            if role == "user" {
                push(&strip_injected(s));
            } else {
                push(s);
            }
            continue;
        }
        let arr = match content.as_array() {
            Some(a) => a,
            None => continue,
        };
        for b in arr {
            match b.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                "text" => {
                    let t = b.get("text").and_then(|v| v.as_str()).unwrap_or("");
                    if role == "user" {
                        push(&strip_injected(t));
                    } else {
                        push(t);
                    }
                }
                "thinking" => push(b.get("thinking").and_then(|v| v.as_str()).unwrap_or("")),
                "tool_use" => {
                    let name = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let input = b.get("input").map(|i| i.to_string()).unwrap_or_default();
                    push(&format!("{} {}", name, input));
                }
                "tool_result" => {
                    push(&tool_result_search_text(b.get("content").unwrap_or(&Value::Null)))
                }
                _ => {}
            }
        }
    }
    out
}

struct SearchCache {
    map: std::collections::HashMap<PathBuf, (f64, u64, std::sync::Arc<String>)>,
    bytes: usize,
}
/// Extracted-text memo, keyed path -> (mtime, size, text). Cleared wholesale past the byte budget
/// (crude but safe — the next search simply re-extracts what it touches).
fn search_cache() -> &'static std::sync::Mutex<SearchCache> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<SearchCache>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(SearchCache { map: std::collections::HashMap::new(), bytes: 0 }))
}
const SEARCH_CACHE_BUDGET: usize = 128 * 1024 * 1024;

/// Search one transcript file for `q`: (extracted text, first-match byte offset), or None.
/// Serves from the extraction cache when fresh; otherwise prefilters the raw bytes and only
/// parses candidates — files that can't match are neither parsed nor cached.
fn thread_scan(path: &Path, q: &str, raw_safe: bool) -> Option<(std::sync::Arc<String>, usize)> {
    let meta = fs::metadata(path).ok()?;
    let (mt, sz) = (mtime_ms(path), meta.len());
    if let Ok(cache) = search_cache().lock() {
        if let Some((cmt, csz, text)) = cache.map.get(path) {
            if *cmt == mt && *csz == sz {
                let t = text.clone();
                drop(cache);
                return ifind(&t, q, 0).map(|p| (t, p));
            }
        }
    }
    let raw = fs::read_to_string(path).ok()?;
    if raw_safe && ifind(&raw, q, 0).is_none() {
        return None;
    }
    let recs = parse_lines(&raw);
    let messages: Vec<Value> = if crate::codex::looks_codex(&recs) {
        crate::codex::session_from_recs(&path.to_string_lossy(), &recs)
            .get("messages")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
    } else {
        shape_messages(&recs).messages
    };
    let text = std::sync::Arc::new(extract_search_text(&messages));
    if let Ok(mut cache) = search_cache().lock() {
        if cache.bytes + text.len() > SEARCH_CACHE_BUDGET {
            cache.map.clear();
            cache.bytes = 0;
        }
        if let Some((_, _, old)) = cache.map.insert(path.to_path_buf(), (mt, sz, text.clone())) {
            cache.bytes = cache.bytes.saturating_sub(old.len()); // replaced a stale entry
        }
        cache.bytes += text.len();
    }
    ifind(&text, q, 0).map(|p| (text, p))
}

/// Display snippet around the first match: ~56 chars of context either side, whitespace collapsed,
/// ellipsized at cut edges. Slice bounds snap outward/inward to char boundaries.
fn snippet_around(text: &str, pos: usize, match_len: usize) -> String {
    const CTX: usize = 56;
    let mut start = pos.saturating_sub(CTX);
    while start > 0 && !text.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (pos + match_len + CTX).min(text.len());
    while end < text.len() && !text.is_char_boundary(end) {
        end += 1;
    }
    let body = text[start..end].split_whitespace().collect::<Vec<_>>().join(" ");
    format!("{}{}{}", if start > 0 { "…" } else { "" }, body, if end < text.len() { "…" } else { "" })
}

/// Scan one session — main thread first, then each subagent transcript — and shape the hit the
/// renderer needs to auto-locate: which agent matched, a snippet, and the occurrence count.
fn scan_session(file: &Path, q: &str, raw_safe: bool) -> Option<Value> {
    if let Some((text, pos)) = thread_scan(file, q, raw_safe) {
        return Some(json!({
            "file": file.to_string_lossy(),
            "agent": "main",
            "snippet": snippet_around(&text, pos, q.len()),
            "count": icount(&text, q),
        }));
    }
    let dir = subagent_dir(file)?;
    let mut names: Vec<String> = vec![];
    if let Ok(entries) = fs::read_dir(&dir) {
        for ent in entries.flatten() {
            let name = ent.file_name().to_string_lossy().into_owned();
            if name.starts_with("agent-") && name.ends_with(".jsonl") {
                names.push(name);
            }
        }
    }
    names.sort();
    for name in names {
        if let Some((text, pos)) = thread_scan(&dir.join(&name), q, raw_safe) {
            let agent_id = name.trim_start_matches("agent-").trim_end_matches(".jsonl").to_string();
            let meta: Value = fs::read_to_string(dir.join(format!("agent-{}.meta.json", agent_id)))
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_else(|| json!({}));
            // Key by the spawning tool_use id — the same key read_subagents uses, so the renderer
            // can switch its panel straight to this agent.
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
            return Some(json!({
                "file": file.to_string_lossy(),
                "agent": key,
                "agentType": agent_type,
                "snippet": snippet_around(&text, pos, q.len()),
                "count": icount(&text, q),
            }));
        }
    }
    None
}

/// Content search over the same candidate set (and dir/trash scoping) as the list view, newest
/// first. Returns [{ file, agent, agentType?, snippet, count }] for up to `limit` sessions.
pub fn search_sessions(config: &Value, active: &str, query: &str, limit: usize) -> Vec<Value> {
    let q = query.trim();
    if q.is_empty() {
        return vec![];
    }
    let trash = active == TRASH_ID;
    // JSON string encoding escapes quotes/backslashes/control chars, so only queries free of
    // those can be prefiltered against the raw bytes.
    let raw_safe = q.bytes().all(|b| b != b'"' && b != b'\\' && b >= 0x20);
    let mut files: Vec<(PathBuf, f64)> = vec![];
    each_session_file(config, |file, _dn, id, _label| {
        if !trash && active != "all" && id != active {
            return;
        }
        let ct = created_ms(&file);
        files.push((file, ct));
    });
    files.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    files.truncate(600); // the list view's own cap — search what the list can show
    let hits = std::sync::Mutex::new(Vec::<(f64, Value)>::new());
    let next = std::sync::atomic::AtomicUsize::new(0);
    let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 8);
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if i >= files.len() {
                    break;
                }
                let (file, ct) = &files[i];
                if is_session_deleted(file) != trash {
                    continue;
                }
                if let Some(hit) = scan_session(file, q, raw_safe) {
                    if let Ok(mut h) = hits.lock() {
                        h.push((*ct, hit));
                    }
                }
            });
        }
    });
    let mut hits = hits.into_inner().unwrap_or_else(|e| e.into_inner());
    hits.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    hits.truncate(limit);
    hits.into_iter().map(|(_, v)| v).collect()
}

/// Write per-conversation customization (custom title + tags) onto the FIRST parseable line as a
/// `__ccbud__` field. Atomic (tmp + rename). Guarded to the configured dirs + the imports store
/// (renderer can't drive an arbitrary-path write, but imported sessions must be titleable/taggable
/// too — mirrors history.js setCcbud, whose getDirs() includes the imported dir).
pub fn set_ccbud(file: &str, patch: &Value, config: &Value) -> Value {
    let target = Path::new(file);
    if !within_scope(target, config) {
        return json!({ "ok": false, "reason": "out-of-scope" });
    }
    let raw = match fs::read_to_string(file) {
        Ok(s) => s,
        Err(_) => return json!({ "ok": false, "reason": "read" }),
    };
    // Live Codex rollouts are another tool's files — their title/tags/delete flag live in the
    // app-owned sidecar instead of being written into the rollout. Imported codex COPIES sit
    // inside our store (marked by .import.json) and take the normal in-file path below.
    let head: Vec<Value> = raw.lines().take(8).filter_map(|l| serde_json::from_str(l.trim()).ok()).collect();
    if crate::codex::looks_codex(&head) && read_import_meta(file).is_none() {
        return crate::codex::set_meta(file, patch);
    }
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
/// Renderer-driven writes/deletes are confined to the configured work dirs' data trees
/// (projects/ AND sessions/) plus the imports store.
fn within_scope(target: &Path, config: &Value) -> bool {
    all_dirs(config).iter().any(|(_, _, pd)| {
        target.starts_with(pd) || sessions_dir(pd).map(|sd| target.starts_with(sd)).unwrap_or(false)
    })
}

pub fn delete_session_file(file: &str, config: &Value) -> Value {
    let target = Path::new(file);
    if !within_scope(target, config) {
        return json!({ "ok": false, "reason": "out-of-scope" });
    }
    if !target.is_file() {
        return json!({ "ok": false, "reason": "missing" });
    }
    // A LIVE Codex rollout is another tool's file — the app only ever soft-deletes it via the
    // sidecar and never rewrites it (see set_ccbud), so "delete forever" must not rm the source
    // either. Imported codex COPIES (marked by an .import.json) are our own snapshots and stay
    // hard-deletable, like Claude sessions the app manages in the configured dirs.
    let head = parse_lines(&read_head(target, 131072));
    if crate::codex::looks_codex(&head) && read_import_meta(file).is_none() {
        return json!({ "ok": false, "reason": "foreign" });
    }
    if fs::remove_file(target).is_err() {
        return json!({ "ok": false, "reason": "remove" });
    }
    crate::codex::remove_meta(file); // drop any codex sidecar entry (no-op for Claude sessions)
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
    let is_codex = crate::codex::looks_codex(&recs);
    let has_msg = recs.iter().any(|r| {
        let t = r.get("type").and_then(|v| v.as_str());
        (t == Some("user") || t == Some("assistant")) && r.get("message").is_some()
    });
    if !has_msg && !is_codex {
        return 0;
    }
    let name_stem = || Path::new(original_name).file_stem().and_then(|s| s.to_str()).unwrap_or("import").to_string();
    // Codex rollouts keep cwd/session id inside the session_meta payload, not on the records.
    let (cwd_owned, base_id) = if is_codex {
        let (c, s) = crate::codex::head_ids(&recs);
        (c, s.unwrap_or_else(name_stem))
    } else {
        let meta_rec = recs.iter().find(|r| r.get("cwd").is_some()).or_else(|| recs.iter().find(|r| r.get("sessionId").is_some()));
        (
            meta_rec.and_then(|r| r.get("cwd")).and_then(|v| v.as_str()).map(|s| s.to_string()),
            meta_rec
                .and_then(|r| r.get("sessionId"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(name_stem),
        )
    };
    let cwd = cwd_owned.as_deref();
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

    // A live Codex rollout (a work dir's sessions/ tree, no .import.json) must NEVER be hard-deleted
    // by "delete forever" — it's another tool's file. delete_session_file must refuse and leave it on
    // disk. A Claude session in the same dir's projects/ tree is still deletable.
    #[test]
    fn delete_forever_refuses_live_codex_rollout() {
        let base = std::env::temp_dir().join("ccbud-codex-del-test");
        let _ = fs::remove_dir_all(&base);
        // codex rollout under <base>/sessions/…
        let sdir = base.join("sessions").join("2026").join("07").join("04");
        fs::create_dir_all(&sdir).unwrap();
        let codex_file = sdir.join("rollout-x.jsonl");
        fs::write(
            &codex_file,
            "{\"timestamp\":\"2026-07-04T00:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"session_id\":\"x\",\"cwd\":\"/x\"}}\n\
             {\"timestamp\":\"2026-07-04T00:00:01Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"hi\"}]}}\n",
        )
        .unwrap();
        // claude session under <base>/projects/…
        let pdir = base.join("projects").join("-x");
        fs::create_dir_all(&pdir).unwrap();
        let claude_file = pdir.join("s1.jsonl");
        fs::write(&claude_file, "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"},\"cwd\":\"/x\",\"sessionId\":\"s1\"}\n").unwrap();

        let config = json!({ "historyDirs": [ base.to_string_lossy() ] });

        // live codex → refused, file survives
        let r = delete_session_file(&codex_file.to_string_lossy(), &config);
        assert_eq!(r.get("reason").and_then(|v| v.as_str()), Some("foreign"), "live codex must be refused");
        assert!(codex_file.is_file(), "codex rollout must NOT be deleted");

        // claude session → deleted
        let r2 = delete_session_file(&claude_file.to_string_lossy(), &config);
        assert_eq!(r2.get("ok").and_then(|v| v.as_bool()), Some(true));
        assert!(!claude_file.is_file(), "claude session should be gone");

        let _ = fs::remove_dir_all(&base);
    }

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

    // The list is ordered by the session's FIRST RECORD TIMESTAMP, not fs times — a title/tag
    // edit rewrites the file via tmp+rename (which resets its fs birth time to "now") and must
    // NOT reshuffle the list.
    #[test]
    fn list_order_survives_title_and_tag_edits() {
        let base = std::env::temp_dir().join("ccbud-order-test");
        let _ = fs::remove_dir_all(&base);
        let proj = base.join("projects").join("-ord-cwd");
        fs::create_dir_all(&proj).unwrap();
        let older = proj.join("older.jsonl");
        let newer = proj.join("newer.jsonl");
        fs::write(&older, "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"old one\"},\"cwd\":\"/ord/cwd\",\"sessionId\":\"older\",\"timestamp\":\"2025-01-01T10:00:00.000Z\"}\n").unwrap();
        fs::write(&newer, "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"new one\"},\"cwd\":\"/ord/cwd\",\"sessionId\":\"newer\",\"timestamp\":\"2025-06-01T10:00:00.000Z\"}\n").unwrap();
        let config = json!({ "historyDirs": [ base.to_string_lossy() ] });
        let order = |cfg: &Value| -> Vec<String> {
            list_sessions(cfg, "all", 50)
                .iter()
                .filter(|s| s.get("cwd").and_then(|v| v.as_str()) == Some("/ord/cwd"))
                .map(|s| s.get("sessionId").and_then(|v| v.as_str()).unwrap_or("").to_string())
                .collect()
        };
        assert_eq!(order(&config), vec!["newer", "older"], "newest record time first");

        // Rename + tag the OLDER session: the file is rewritten through a fresh tmp inode, yet
        // the list order must not change.
        let r = set_ccbud(&older.to_string_lossy(), &json!({ "title": "Renamed", "tags": ["pinned"] }), &config);
        assert_eq!(r.get("ok").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(order(&config), vec!["newer", "older"], "tag/title edit must not reshuffle");

        // And the row's createdAt still reflects the record timestamp, not the rewrite moment.
        let rows = list_sessions(&config, "all", 50);
        let row = rows.iter().find(|s| s.get("sessionId").and_then(|v| v.as_str()) == Some("older")).unwrap();
        let want = chrono::DateTime::parse_from_rfc3339("2025-01-01T10:00:00.000Z").unwrap().timestamp_millis() as f64;
        assert_eq!(row.get("createdAt").and_then(|v| v.as_f64()), Some(want));

        let _ = fs::remove_dir_all(&base);
    }

    // Content search: a main-thread hit reports agent "main"; a subagent-only hit reports the
    // spawning tool_use key (+ agent type); injected <system-reminder> text never matches; and
    // ASCII case folds. Runs twice so the second pass exercises the extraction cache.
    #[test]
    fn search_sessions_finds_main_and_subagent_content() {
        let base = std::env::temp_dir().join("ccbud-search-test");
        let _ = fs::remove_dir_all(&base);
        let proj = base.join("projects").join("-srch-cwd");
        fs::create_dir_all(&proj).unwrap();
        let main = proj.join("srchsess.jsonl");
        fs::write(
            &main,
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"find the zebra crossing<system-reminder>reminder-secret</system-reminder>\"},\"cwd\":\"/srch/cwd\",\"sessionId\":\"srchsess\",\"timestamp\":\"2025-01-01T10:00:00.000Z\"}\n\
             {\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"tu1\",\"name\":\"Task\",\"input\":{}}]},\"sessionId\":\"srchsess\",\"timestamp\":\"2025-01-01T10:00:01.000Z\"}\n",
        )
        .unwrap();
        let sub = proj.join("srchsess").join("subagents");
        fs::create_dir_all(&sub).unwrap();
        fs::write(
            sub.join("agent-s1.jsonl"),
            "{\"type\":\"assistant\",\"isSidechain\":true,\"agentId\":\"s1\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"the quokka was found here\"}]},\"sessionId\":\"srchsess\",\"timestamp\":\"2025-01-01T10:00:02.000Z\"}\n",
        )
        .unwrap();
        fs::write(sub.join("agent-s1.meta.json"), "{\"agentType\":\"explore\",\"description\":\"d\",\"toolUseId\":\"tu1\"}").unwrap();
        // A codex rollout in the same work dir's sessions/ tree — its own record format, scanned
        // through the codex shaper.
        let cdir = base.join("sessions").join("2026").join("07").join("04");
        fs::create_dir_all(&cdir).unwrap();
        fs::write(
            cdir.join("rollout-c.jsonl"),
            "{\"timestamp\":\"2026-07-04T00:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"session_id\":\"c1\",\"cwd\":\"/cx\"}}\n\
             {\"timestamp\":\"2026-07-04T00:00:01Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"codex kangaroo request\"}]}}\n",
        )
        .unwrap();
        let config = json!({ "historyDirs": [ base.to_string_lossy() ] });

        for pass in 0..2 {
            // main-thread hit
            let hits = search_sessions(&config, "all", "zebra crossing", 50);
            assert_eq!(hits.len(), 1, "pass {}: one session matches", pass);
            assert_eq!(hits[0].get("agent").and_then(|v| v.as_str()), Some("main"));
            assert!(hits[0].get("snippet").and_then(|v| v.as_str()).unwrap_or("").contains("zebra"));

            // subagent-only hit → keyed by the spawning tool_use id, labeled with the agent type
            let hits = search_sessions(&config, "all", "QUOKKA", 50); // also proves case folding
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].get("agent").and_then(|v| v.as_str()), Some("tu1"));
            assert_eq!(hits[0].get("agentType").and_then(|v| v.as_str()), Some("explore"));

            // codex rollout content is searchable too
            let hits = search_sessions(&config, "all", "kangaroo", 50);
            assert_eq!(hits.len(), 1, "pass {}: codex rollout matches", pass);
            assert_eq!(hits[0].get("agent").and_then(|v| v.as_str()), Some("main"));

            // injected system-reminder content is NOT searchable (matches the renderer)
            assert!(search_sessions(&config, "all", "reminder-secret", 50).is_empty());
            // no match at all
            assert!(search_sessions(&config, "all", "wombat", 50).is_empty());
        }

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn subagent_transcript_paths_lists_only_agent_jsonl() {
        let base = std::env::temp_dir().join("ccbud-subpaths-test");
        let _ = fs::remove_dir_all(&base);
        let proj = base.join("projects").join("-m-cwd");
        fs::create_dir_all(&proj).unwrap();
        let main = proj.join("m.jsonl");
        fs::write(&main, "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"},\"sessionId\":\"m\"}\n").unwrap();
        // no subagents → empty (caller attaches only the main file)
        assert!(subagent_transcript_paths(&main.to_string_lossy()).is_empty());

        let sub = proj.join("m").join("subagents");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("agent-a.jsonl"), "{}\n").unwrap();
        fs::write(sub.join("agent-b.jsonl"), "{}\n").unwrap();
        fs::write(sub.join("agent-a.meta.json"), "{}").unwrap(); // sidecar must be excluded

        let paths = subagent_transcript_paths(&main.to_string_lossy());
        assert_eq!(paths.len(), 2, "only the two agent-*.jsonl, not the .meta.json");
        assert!(paths.iter().all(|p| p.ends_with(".jsonl")));
        assert!(paths.iter().any(|p| p.ends_with("agent-a.jsonl")));
        assert!(paths.iter().any(|p| p.ends_with("agent-b.jsonl")));

        let _ = fs::remove_dir_all(&base);
    }
}
