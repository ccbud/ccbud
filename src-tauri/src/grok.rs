// Grok Build CLI session support — reads xAI Grok's on-disk sessions
// (`~/.grok/sessions/<percent-encoded-cwd>/<uuid>/chat_history.jsonl`, sibling `summary.json`
// carrying id/cwd/title/model/git/timestamps) and normalizes them into the SAME session/message
// shape the renderer consumes (see history::Norm), so the 对话 view browses Grok sessions
// without renderer forks.
//
// A chat_history line is one of: `system` (harness prompt — skipped), `user` (content blocks of
// text / data-URL image; the human prose is wrapped in <user_query> tags, harness wrappers like
// <user_info>/<git_status> are dropped), `reasoning` ({summary:[{summary_text}]} → thinking),
// `assistant` ({content, tool_calls:[{id,name,arguments-json}]}), and `tool_result`
// ({tool_call_id, content, images?}). Tool names are mapped onto the renderer's native
// vocabulary (both grok tool-name generations: read_file/Read → Read, Shell → Bash, …).
//
// The same uuid dir also holds events/updates/rewind_points/hunk_records .jsonl — only
// chat_history.jsonl is the conversation; walkers must never sweep the rest.
//
// Title/tags/soft-delete live in the shared foreign-CLI sidecar (~/.ccbud/agent-meta.json)
// keyed `grok:<uuid>` — chat_history stems aren't unique, and the files belong to another tool.

#![allow(dead_code)]

use crate::history::{image_block, Norm};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

fn home() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("."))
}

/// Grok's DEFAULT config dir as a history-dir entry string (`~/.grok`). Honors GROK_HOME the way
/// the grok CLI does (summary.json echoes it as `grok_home`). Only the auto-add migration keys
/// off this — browsing walks every configured dir's `sessions/` tree.
pub fn default_root() -> PathBuf {
    match std::env::var("GROK_HOME") {
        Ok(h) if !h.trim().is_empty() => PathBuf::from(h),
        _ => home().join(".grok"),
    }
}

pub fn grok_label() -> String {
    crate::store::collapse_home(&default_root().to_string_lossy())
}

/// A grok install exists when its sessions tree holds at least one percent-encoded cwd dir.
pub fn root_exists() -> bool {
    let sessions = default_root().join("sessions");
    fs::read_dir(&sessions)
        .map(|entries| {
            entries
                .flatten()
                .any(|e| is_cwd_dir_name(&e.file_name().to_string_lossy()) && e.path().is_dir())
        })
        .unwrap_or(false)
}

/// Grok encodes each workspace cwd as a percent-encoded absolute path dir ("%2FUsers%2F…") —
/// the marker that distinguishes a grok sessions/ child from Codex's YYYY date shards.
pub fn is_cwd_dir_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("%2f") || lower.starts_with("%3a%5c") // unix "/", windows "X:\" oddity-proof
}

/// Session files under one encoded-cwd dir: `<dir>/<uuid>/chat_history.jsonl`.
pub fn walk_cwd_dir<F: FnMut(PathBuf)>(dir: &Path, cb: &mut F) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for ent in entries.flatten() {
        let p = ent.path();
        if !p.is_dir() {
            continue;
        }
        let chat = p.join("chat_history.jsonl");
        if chat.is_file() {
            cb(chat);
        }
    }
}

/// Container-shape test for detail/edit routing: `…/sessions/<enc-cwd>/<uuid>/chat_history.jsonl`.
pub fn looks_grok_path(file: &Path) -> bool {
    if file.file_name().and_then(|n| n.to_str()) != Some("chat_history.jsonl") {
        return false;
    }
    file.parent()
        .and_then(|uuid_dir| uuid_dir.parent())
        .and_then(|enc| enc.file_name())
        .map(|n| is_cwd_dir_name(&n.to_string_lossy()))
        .unwrap_or(false)
}

/// The session uuid (its dir name) — sidecar key and renderer id both build on it.
fn session_uuid(file: &Path) -> String {
    file.parent()
        .and_then(|d| d.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn sidecar_key(file: &Path) -> String {
    format!("grok:{}", session_uuid(file))
}

fn sidecar_meta(file: &Path) -> (Option<String>, Vec<String>, bool) {
    crate::sidecar::meta(&crate::sidecar::agent_file(), &sidecar_key(file))
}

pub fn is_deleted(file: &Path) -> bool {
    sidecar_meta(file).2
}

pub fn set_meta(file: &str, patch: &Value) -> Value {
    let key = sidecar_key(Path::new(file));
    if key == "grok:" {
        return json!({ "ok": false, "reason": "empty" });
    }
    crate::sidecar::set_meta(&crate::sidecar::agent_file(), &key, patch)
}

/// Sibling summary.json of a chat_history.jsonl (grok's own session metadata).
fn summary_of(file: &Path) -> Option<Value> {
    let p = file.parent()?.join("summary.json");
    serde_json::from_str(&fs::read_to_string(p).ok()?).ok()
}

/// Minimal percent-decoding for grok's encoded-cwd dir names (fallback when summary.json
/// is missing; the record cwd wins when present). Also used on Antigravity's file:// uris.
pub(crate) fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn rfc3339_ms(s: &str) -> Option<f64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.timestamp_millis() as f64)
}

/// Harness-injected user text (environment wrappers) — hidden from the timeline.
fn is_meta_user_text(t: &str) -> bool {
    let t = t.trim_start();
    ["<user_info>", "<git_status>", "<system-reminder>", "<project_layout", "<workspace_"]
        .iter()
        .any(|p| t.starts_with(p))
}

/// Unwrap `<user_query>…</user_query>` (the human prose envelope grok writes).
fn unwrap_user_query(t: &str) -> String {
    match t.split_once("<user_query>") {
        Some((_, rest)) => rest.split("</user_query>").next().unwrap_or(rest).trim().to_string(),
        None => t.trim().to_string(),
    }
}

/// Grok tool name + parsed arguments → (renderer tool name, renderer input). Covers both grok
/// tool-name generations (snake_case and CamelCase).
fn map_tool(name: &str, args: &Value) -> (String, Value) {
    let s = |k: &str| args.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let keep = |v: &Value| if v.is_object() { v.clone() } else { json!({}) };
    match name {
        "run_terminal_command" | "Shell" => {
            let mut input = json!({ "command": s("command") });
            if !s("description").is_empty() {
                input["description"] = json!(s("description"));
            }
            ("Bash".into(), input)
        }
        "read_file" | "Read" => {
            let path = if !s("target_file").is_empty() { s("target_file") } else { s("path") };
            let mut input = json!({ "file_path": path });
            for k in ["offset", "limit"] {
                if let Some(v) = args.get(k) {
                    if !v.is_null() {
                        input[k] = v.clone();
                    }
                }
            }
            ("Read".into(), input)
        }
        "grep" | "Grep" | "grep_search" => {
            let mut input = json!({ "pattern": if !s("pattern").is_empty() { s("pattern") } else { s("query") } });
            if !s("path").is_empty() {
                input["path"] = json!(s("path"));
            }
            ("Grep".into(), input)
        }
        "search_replace" => ("Edit".into(), keep(args)),
        "StrReplace" => (
            "Edit".into(),
            json!({ "file_path": s("path"), "old_string": s("old_string"), "new_string": s("new_string") }),
        ),
        "write" => ("Write".into(), keep(args)),
        "Write" => ("Write".into(), json!({ "file_path": s("path"), "content": s("contents") })),
        "list_dir" => ("LS".into(), json!({ "path": s("target_directory") })),
        "Glob" => ("Glob".into(), json!({ "pattern": s("glob_pattern"), "path": s("target_directory") })),
        "todo_write" | "TodoWrite" => ("TodoWrite".into(), keep(args)),
        "web_fetch" | "WebFetch" => ("WebFetch".into(), json!({ "url": s("url") })),
        "WebSearch" => ("WebSearch".into(), json!({ "query": s("search_term") })),
        _ => (name.to_string(), keep(args)),
    }
}

/// Normalize parsed chat_history records (+ the sibling summary) into the renderer's message
/// model. Lines carry no timestamps — session-level times come from summary.json.
pub fn normalize(recs: &[Value], summary: Option<&Value>) -> Norm {
    let mut n = Norm::default();
    let sum = summary.cloned().unwrap_or(Value::Null);
    n.model = sum.get("current_model_id").and_then(|v| v.as_str()).map(|s| s.to_string());
    n.cwd = sum
        .get("info")
        .and_then(|i| i.get("cwd"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    n.session_id = sum
        .get("info")
        .and_then(|i| i.get("id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    n.git_branch = sum.get("head_branch").and_then(|v| v.as_str()).map(|s| s.to_string());
    n.first_ts = sum.get("created_at").and_then(|v| v.as_str()).map(|s| s.to_string());
    n.last_ts = sum
        .get("last_active_at")
        .or_else(|| sum.get("updated_at"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    for rec in recs {
        let ty = rec.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match ty {
            "user" => {
                let mut blocks: Vec<Value> = vec![];
                if let Some(arr) = rec.get("content").and_then(|c| c.as_array()) {
                    for b in arr {
                        match b.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                            "text" => {
                                let raw = b.get("text").and_then(|t| t.as_str()).unwrap_or("");
                                if is_meta_user_text(raw) && !raw.contains("<user_query>") {
                                    continue;
                                }
                                let text = unwrap_user_query(raw);
                                if !text.is_empty() {
                                    blocks.push(json!({ "type": "text", "text": text }));
                                }
                            }
                            "image" => {
                                if let Some(img) =
                                    b.get("url").and_then(|u| u.as_str()).and_then(image_block)
                                {
                                    blocks.push(img);
                                }
                            }
                            _ => {}
                        }
                    }
                } else if let Some(t) = rec.get("content").and_then(|c| c.as_str()) {
                    let text = unwrap_user_query(t);
                    if !text.is_empty() && !is_meta_user_text(t) {
                        blocks.push(json!({ "type": "text", "text": text }));
                    }
                }
                if !blocks.is_empty() {
                    n.messages.push(json!({ "role": "user", "content": blocks }));
                }
            }
            "reasoning" => {
                let txt = rec
                    .get("summary")
                    .and_then(|s| s.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                if !txt.trim().is_empty() {
                    let mut m = json!({ "role": "assistant", "content": [{ "type": "thinking", "thinking": txt }] });
                    if let Some(md) = &n.model {
                        m["modelActual"] = json!(md);
                    }
                    n.messages.push(m);
                }
            }
            "assistant" => {
                let mut blocks: Vec<Value> = vec![];
                let text = rec.get("content").and_then(|c| c.as_str()).unwrap_or("");
                if !text.trim().is_empty() {
                    blocks.push(json!({ "type": "text", "text": text }));
                }
                if let Some(calls) = rec.get("tool_calls").and_then(|c| c.as_array()) {
                    for call in calls {
                        let name = call.get("name").and_then(|v| v.as_str()).unwrap_or("tool");
                        let args: Value = call
                            .get("arguments")
                            .and_then(|v| v.as_str())
                            .and_then(|s| serde_json::from_str(s).ok())
                            .unwrap_or_else(|| call.get("arguments").cloned().unwrap_or(json!({})));
                        let (tname, input) = map_tool(name, &args);
                        let id = call.get("id").and_then(|v| v.as_str()).unwrap_or("");
                        blocks.push(json!({ "type": "tool_use", "id": id, "name": tname, "input": input }));
                    }
                }
                if !blocks.is_empty() {
                    let mut m = json!({ "role": "assistant", "content": blocks });
                    if let Some(md) = &n.model {
                        m["modelActual"] = json!(md);
                    }
                    n.messages.push(m);
                }
            }
            "tool_result" => {
                let id = rec.get("tool_call_id").and_then(|v| v.as_str()).unwrap_or("");
                let text = rec.get("content").and_then(|c| c.as_str()).unwrap_or("").to_string();
                let images: Vec<Value> = rec
                    .get("images")
                    .and_then(|a| a.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|b| b.get("url").and_then(|u| u.as_str()).and_then(image_block))
                            .collect()
                    })
                    .unwrap_or_default();
                let content: Value = if images.is_empty() {
                    json!(text)
                } else {
                    let mut blocks = vec![json!({ "type": "text", "text": text })];
                    blocks.extend(images);
                    json!(blocks)
                };
                n.messages
                    .push(json!({ "role": "user", "content": [{ "type": "tool_result", "tool_use_id": id, "content": content }] }));
            }
            _ => {} // system / unknown: harness plumbing, not conversation
        }
    }
    n
}

/// List-row meta: summary.json carries everything cheap (title/cwd/model/times); the file head
/// is only parsed when grok didn't store a title yet (fallback to first user prose).
pub fn session_meta_from(file: &Path, dir_id: &str, dir_label: &str) -> Option<Value> {
    let meta = fs::metadata(file).ok()?;
    let sum = summary_of(file);
    let uuid = session_uuid(file);
    let (cc_title, cc_tags, cc_deleted) = sidecar_meta(file);
    let sum_title = sum
        .as_ref()
        .and_then(|s| s.get("generated_title").or_else(|| s.get("session_summary")))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let auto_title = sum_title.unwrap_or_else(|| {
        let recs = crate::history::parse_lines(&crate::history::read_head(file, 131072));
        let n = normalize(&recs, sum.as_ref());
        crate::history::first_user_text(&n.messages)
    });
    let cwd = sum
        .as_ref()
        .and_then(|s| s.get("info"))
        .and_then(|i| i.get("cwd"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            file.parent()
                .and_then(|d| d.parent())
                .and_then(|enc| enc.file_name())
                .map(|nm| percent_decode(&nm.to_string_lossy()))
        });
    let created = sum
        .as_ref()
        .and_then(|s| s.get("created_at"))
        .and_then(|v| v.as_str())
        .and_then(rfc3339_ms)
        .unwrap_or_else(|| crate::history::created_ms(file));
    let mt = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0);
    Some(json!({
        "id": format!("grok:{}", uuid),
        "file": file.to_string_lossy(),
        "source": "grok",
        "dirId": dir_id,
        "dirLabel": dir_label,
        "sessionId": sum
            .as_ref()
            .and_then(|s| s.get("info"))
            .and_then(|i| i.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or(&uuid),
        "cwd": cwd.clone(),
        "project": cwd.as_deref().map(crate::history::base_name).unwrap_or_default(),
        "gitBranch": sum.as_ref().and_then(|s| s.get("head_branch")).cloned().unwrap_or(Value::Null),
        "title": cc_title.clone().unwrap_or_else(|| auto_title.clone()),
        "autoTitle": auto_title,
        "tags": cc_tags,
        "model": sum.as_ref().and_then(|s| s.get("current_model_id")).cloned().unwrap_or(Value::Null),
        "isSubagent": false,
        "imported": false,
        "deleted": cc_deleted,
        "createdAt": created,
        "lastActivity": mt,
        "sizeKB": (meta.len() as f64 / 1024.0).round() as i64,
    }))
}

/// Full-detail shape (history.rs get_session routes here).
pub fn session_from_recs(file: &str, recs: &[Value]) -> Value {
    let path = Path::new(file);
    let sum = summary_of(path);
    let n = normalize(recs, sum.as_ref());
    let (cc_title, cc_tags, cc_deleted) = sidecar_meta(path);
    let sum_title = sum
        .as_ref()
        .and_then(|s| s.get("generated_title").or_else(|| s.get("session_summary")))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let auto_title = sum_title.unwrap_or_else(|| crate::history::first_user_text(&n.messages));
    let uuid = session_uuid(path);
    json!({
        "meta": {
            "id": format!("grok:{}", uuid),
            "file": file,
            "source": "grok",
            "assistant": "Grok",
            "title": cc_title.clone().unwrap_or_else(|| auto_title.clone()),
            "autoTitle": auto_title,
            "tags": cc_tags,
            "summary": Value::Null,
            "sessionId": n.session_id.clone().unwrap_or_else(|| uuid.clone()),
            "cwd": n.cwd.clone(),
            "project": n.cwd.as_deref().map(crate::history::base_name).unwrap_or_default(),
            "gitBranch": n.git_branch.clone(),
            "version": Value::Null,
            "isSubagent": false,
            "deleted": cc_deleted,
            "imported": false,
            "importedFrom": Value::Null,
            "importedAt": Value::Null,
            "model": n.model,
            "totals": n.totals,
            "messages": n.messages.len(),
            "subagentCount": 0,
            "firstTs": n.first_ts,
            "lastTs": n.last_ts,
        },
        "messages": n.messages,
        "subagents": {},
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recs() -> Vec<Value> {
        vec![
            json!({ "type": "system", "content": "You are Grok…" }),
            json!({ "type": "user", "content": [{ "type": "text", "text": "<user_info>\nOS: macos\n</user_info>\n\n<git_status>\nclean\n</git_status>\n" }] }),
            json!({ "type": "user", "content": [
                { "type": "text", "text": "<user_query>\n修复登录 bug\n</user_query>" },
                { "type": "image", "url": "data:image/png;base64,QUJD" }
            ] }),
            json!({ "type": "reasoning", "id": "rs_1", "summary": [{ "type": "summary_text", "text": "Scanning the repo" }] }),
            json!({ "type": "assistant", "content": "先看下目录。", "tool_calls": [
                { "id": "call-1", "name": "run_terminal_command", "arguments": "{\"command\":\"ls\",\"description\":\"List files\"}" },
                { "id": "call-2", "name": "read_file", "arguments": "{\"target_file\":\"src/app.js\"}" }
            ] }),
            json!({ "type": "tool_result", "tool_call_id": "call-1", "content": "a.txt\nb.txt" }),
            json!({ "type": "tool_result", "tool_call_id": "call-2", "content": "console.log(1)", "images": [{ "type": "image", "url": "data:image/png;base64,REVG" }] }),
        ]
    }

    fn summary() -> Value {
        json!({
            "info": { "id": "0199-aaaa", "cwd": "/tmp/proj" },
            "generated_title": "Fix login bug",
            "created_at": "2026-06-18T06:27:07.777809Z",
            "last_active_at": "2026-06-18T06:57:37.242478Z",
            "current_model_id": "grok-build",
            "head_branch": "main",
        })
    }

    #[test]
    fn normalizes_conversation() {
        let s = summary();
        let n = normalize(&recs(), Some(&s));
        // harness wrapper user turn dropped; real turns: user, thinking, assistant+tools, 2 results
        assert_eq!(n.messages.len(), 5);
        assert_eq!(n.messages[0]["role"], "user");
        assert_eq!(n.messages[0]["content"][0]["text"], "修复登录 bug");
        assert_eq!(n.messages[0]["content"][1]["type"], "image");
        assert_eq!(n.messages[1]["content"][0]["type"], "thinking");
        let a = &n.messages[2];
        assert_eq!(a["content"][0]["text"], "先看下目录。");
        assert_eq!(a["content"][1]["name"], "Bash");
        assert_eq!(a["content"][1]["input"]["command"], "ls");
        assert_eq!(a["content"][2]["name"], "Read");
        assert_eq!(a["content"][2]["input"]["file_path"], "src/app.js");
        assert_eq!(n.messages[3]["content"][0]["tool_use_id"], "call-1");
        // image-carrying result becomes a block array
        assert_eq!(n.messages[4]["content"][0]["content"][1]["type"], "image");
        assert_eq!(n.model.as_deref(), Some("grok-build"));
        assert_eq!(n.cwd.as_deref(), Some("/tmp/proj"));
    }

    #[test]
    fn detects_cwd_dirs_and_paths() {
        assert!(is_cwd_dir_name("%2FUsers%2Fme%2Fcode"));
        assert!(is_cwd_dir_name("%2fusers%2fme"));
        assert!(!is_cwd_dir_name("2026"));
        assert_eq!(percent_decode("%2FUsers%2Fme"), "/Users/me");
        let p = Path::new("/x/sessions/%2FUsers%2Fme/0199-aaaa/chat_history.jsonl");
        assert!(looks_grok_path(p));
        assert!(!looks_grok_path(Path::new("/x/sessions/2026/01/01/rollout-1.jsonl")));
        assert_eq!(session_uuid(p), "0199-aaaa");
    }
}
