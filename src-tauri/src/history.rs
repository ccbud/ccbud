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

/// __ccbud__ customization (custom title + tags) from any record carrying it.
fn read_ccbud(recs: &[Value]) -> (Option<String>, Vec<String>) {
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
    (title, tags)
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
    let (cc_title, cc_tags) = read_ccbud(&recs);
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
        "lastActivity": mt,
        "sizeKB": (size as f64 / 1024.0).round() as i64,
    }))
}

pub fn list_sessions(config: &Value, active: &str, limit: usize) -> Vec<Value> {
    let mut files: Vec<(PathBuf, String, String, String, f64)> = vec![];
    each_session_file(config, |file, dir_name, id, label| {
        if active != "all" && id != active {
            return;
        }
        let mt = mtime_ms(&file);
        files.push((file, dir_name, id.to_string(), label.to_string(), mt));
    });
    files.sort_by(|a, b| b.4.partial_cmp(&a.4).unwrap_or(std::cmp::Ordering::Equal));
    files
        .into_iter()
        .take(limit)
        .filter_map(|(file, dn, id, label, _)| session_meta(&file, &dn, &id, &label))
        .collect()
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
    let mut counts: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    each_session_file(config, |_file, _dn, id, _label| {
        *counts.entry(id.to_string()).or_insert(0) += 1;
    });
    all_dirs(config)
        .into_iter()
        .map(|(id, label, pd)| {
            let exists = pd.is_dir();
            let imported = id == "__imported__";
            json!({
                "id": id.clone(), "label": label, "projectsDir": pd.to_string_lossy(),
                "sessions": counts.get(&id).copied().unwrap_or(0), "exists": exists, "imported": imported,
            })
        })
        .collect()
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
    let summary = recs
        .iter()
        .find(|r| r.get("type").and_then(|v| v.as_str()) == Some("summary") && r.get("summary").is_some())
        .and_then(|r| r.get("summary").cloned());
    let (cc_title, cc_tags) = read_ccbud(&recs);
    let shaped = shape_messages(&recs);
    let auto_title = first_user_text(&shaped.messages);
    let subagent = agent_rec.is_some();
    let cwd = meta_rec.and_then(|r| r.get("cwd")).and_then(|v| v.as_str()).map(|s| s.to_string());
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();

    json!({
        "meta": {
            "id": format!("disk:{}{}", stem, if subagent { ":sub" } else { "" }),
            "file": file,
            "source": "disk",
            "title": cc_title.clone().unwrap_or_else(|| auto_title.clone()),
            "autoTitle": auto_title,
            "tags": cc_tags,
            "summary": summary,
            "sessionId": meta_rec.and_then(|r| r.get("sessionId")).and_then(|v| v.as_str()).unwrap_or(&stem),
            "cwd": cwd.clone(),
            "project": cwd.as_deref().map(base_name).unwrap_or_default(),
            "gitBranch": meta_rec.and_then(|r| r.get("gitBranch")).cloned().unwrap_or(Value::Null),
            "version": meta_rec.and_then(|r| r.get("version")).cloned().unwrap_or(Value::Null),
            "isSubagent": subagent,
            "imported": false,
            "model": shaped.model,
            "totals": shaped.totals,
            "messages": shaped.messages.len(),
            "subagentCount": 0,
            "firstTs": shaped.first_ts,
            "lastTs": shaped.last_ts,
        },
        "messages": shaped.messages,
        "subagents": {},
    })
}

/// Write per-conversation customization (custom title + tags) onto the FIRST parseable line as a
/// `__ccbud__` field. Atomic (tmp + rename). Guarded to the configured dirs (renderer can't drive
/// an arbitrary-path write). Mirrors history.js setCcbud.
pub fn set_ccbud(file: &str, patch: &Value, config: &Value) -> Value {
    let target = Path::new(file);
    let within = config_dirs(config).iter().any(|(_, _, pd)| target.starts_with(pd));
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
    json!({
        "setOk": set.get("ok").and_then(|v| v.as_bool()).unwrap_or(false),
        "title": title,
        "tagCount": tags,
        "autoTitle": auto,
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

/// Import one transcript: validates it's a real Claude Code session, then snapshots it into the
/// import store laid out like a native projects/ tree + a sidecar provenance file. Returns
/// 1 = imported, 2 = skipped (already present), 0 = failed/not-a-transcript.
fn import_one(src: &str) -> i32 {
    let raw = match fs::read_to_string(src) {
        Ok(s) => s,
        Err(_) => return 0,
    };
    let recs = parse_lines(&raw);
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
        .unwrap_or_else(|| Path::new(src).file_stem().and_then(|s| s.to_str()).unwrap_or("import").to_string());
    let dest_dir = imports_root().join("projects").join(encode_cwd(cwd));
    let dest_file = dest_dir.join(format!("{}.jsonl", base_id));
    if dest_file.exists() {
        return 2;
    }
    if fs::create_dir_all(&dest_dir).is_err() || fs::copy(src, &dest_file).is_err() {
        return 0;
    }
    // bring along subagents if present next to the source
    let src_stem = Path::new(src).file_stem().and_then(|s| s.to_str()).unwrap_or("");
    if let Some(parent) = Path::new(src).parent() {
        let src_sub = parent.join(src_stem).join("subagents");
        if src_sub.is_dir() {
            let _ = copy_dir(&src_sub, &dest_dir.join(&base_id).join("subagents"));
        }
    }
    let imported_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let sidecar = dest_dir.join(format!("{}.import.json", base_id));
    let _ = fs::write(
        &sidecar,
        serde_json::to_vec_pretty(&json!({
            "originalPath": src,
            "originalName": Path::new(src).file_name().and_then(|n| n.to_str()).unwrap_or(""),
            "sessionId": base_id,
            "importedAt": imported_at,
        }))
        .unwrap_or_default(),
    );
    1
}

pub fn import_paths(paths: &[String]) -> Value {
    let (mut imported, mut skipped, mut failed) = (0, 0, 0);
    for src in paths {
        if src.to_lowercase().ends_with(".jsonl") {
            match import_one(src) {
                1 => imported += 1,
                2 => skipped += 1,
                _ => failed += 1,
            }
        } else {
            failed += 1;
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
    json!({
        "imported": r.get("imported"),
        "reskipped": r2.get("skipped"),
        "appearsImported": found,
        "removed": rm.get("ok"),
        "gone": !dest.exists(),
    })
}
