// GitHub Copilot CLI session support — reads Copilot's on-disk session event logs and
// normalizes them into the SAME session/message shape the renderer consumes (history::Norm).
//
// Two layouts under `~/.copilot/session-state/`:
//   new (≥1.0):  <uuid>/events.jsonl  + sibling workspace.yaml (id/cwd/name/branch/timestamps,
//                flat "key: value" lines — parsed without a YAML dependency)
//   old:         <uuid>.jsonl flat files (same event schema; early builds carry no cwd at all,
//                so those sessions group under the unknown-project bucket)
//
// An event line is `{type, data, id, timestamp, parentId}`. Conversation content:
//   session.start            → cwd/session id/version (data.context.cwd on newer builds)
//   session.model_change     → model (data.newModel)
//   user.message             → user text (data.content)
//   assistant.message        → assistant text + tool_use blocks (data.content,
//                              data.toolRequests[{toolCallId,name,arguments}], data.model)
//   tool.execution_complete  → tool_result (data.toolCallId, data.success, data.result.content)
// Everything else (session.info, system.*, turn markers, tool.execution_start) is harness
// plumbing and skipped — tool arguments already ride the assistant.message request.
//
// Title/tags/soft-delete live in the shared foreign-CLI sidecar (~/.ccbud/agent-meta.json)
// keyed `copilot:<uuid>` — the files belong to another tool and are never rewritten.

#![allow(dead_code)]

use crate::history::Norm;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

fn home() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("."))
}

/// Copilot's config dir as a history-dir entry string (`~/.copilot`).
pub fn default_root() -> PathBuf {
    home().join(".copilot")
}

pub fn copilot_label() -> String {
    crate::store::collapse_home(&default_root().to_string_lossy())
}

pub fn root_exists() -> bool {
    default_root().join("session-state").is_dir()
}

/// Walk every session log under a `session-state/` tree: flat `<uuid>.jsonl` (old) and
/// `<uuid>/events.jsonl` (new). Dirs without an events.jsonl (created-but-unused sessions,
/// checkpoint-only remnants) hold no conversation and are skipped.
pub fn walk<F: FnMut(PathBuf)>(state_dir: &Path, cb: &mut F) {
    let entries = match fs::read_dir(state_dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for ent in entries.flatten() {
        let p = ent.path();
        if p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            cb(p);
        } else if p.is_dir() {
            let events = p.join("events.jsonl");
            if events.is_file() {
                cb(events);
            }
        }
    }
}

/// Container-shape test for detail/edit routing: a .jsonl directly in `session-state/`, or an
/// `events.jsonl` whose grandparent is `session-state/`.
pub fn looks_copilot_path(file: &Path) -> bool {
    let parent_named = |p: &Path, name: &str| {
        p.file_name().and_then(|n| n.to_str()).map(|n| n == name).unwrap_or(false)
    };
    match file.file_name().and_then(|n| n.to_str()) {
        Some("events.jsonl") => file
            .parent()
            .and_then(|d| d.parent())
            .map(|gp| parent_named(gp, "session-state"))
            .unwrap_or(false),
        Some(n) if n.ends_with(".jsonl") => {
            file.parent().map(|d| parent_named(d, "session-state")).unwrap_or(false)
        }
        _ => false,
    }
}

/// The session uuid — the flat file's stem, or the events.jsonl dir name.
fn session_uuid(file: &Path) -> String {
    if file.file_name().and_then(|n| n.to_str()) == Some("events.jsonl") {
        file.parent()
            .and_then(|d| d.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    } else {
        file.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string()
    }
}

fn sidecar_key(file: &Path) -> String {
    format!("copilot:{}", session_uuid(file))
}

fn sidecar_meta(file: &Path) -> (Option<String>, Vec<String>, bool) {
    crate::sidecar::meta(&crate::sidecar::agent_file(), &sidecar_key(file))
}

pub fn is_deleted(file: &Path) -> bool {
    sidecar_meta(file).2
}

pub fn set_meta(file: &str, patch: &Value) -> Value {
    let key = sidecar_key(Path::new(file));
    if key == "copilot:" {
        return json!({ "ok": false, "reason": "empty" });
    }
    crate::sidecar::set_meta(&crate::sidecar::agent_file(), &key, patch)
}

/// Sibling workspace.yaml of an events.jsonl, parsed as flat `key: value` lines (the file is
/// machine-written and flat; no YAML dependency needed). None for old flat sessions.
fn workspace_yaml(file: &Path) -> Option<serde_json::Map<String, Value>> {
    if file.file_name().and_then(|n| n.to_str()) != Some("events.jsonl") {
        return None;
    }
    let text = fs::read_to_string(file.parent()?.join("workspace.yaml")).ok()?;
    let mut map = serde_json::Map::new();
    for line in text.lines() {
        if let Some((k, v)) = line.split_once(':') {
            let (k, v) = (k.trim(), v.trim());
            if !k.is_empty() && !k.starts_with('#') && !v.is_empty() {
                map.insert(k.to_string(), json!(v.trim_matches('"').trim_matches('\'')));
            }
        }
    }
    Some(map)
}

/// Copilot tool name + arguments (already an object) → (renderer tool name, renderer input).
fn map_tool(name: &str, args: &Value) -> (String, Value) {
    let s = |k: &str| args.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let keep = |v: &Value| if v.is_object() { v.clone() } else { json!({}) };
    match name {
        "bash" => {
            let mut input = json!({ "command": s("command") });
            if !s("description").is_empty() {
                input["description"] = json!(s("description"));
            }
            ("Bash".into(), input)
        }
        "view" => ("Read".into(), json!({ "file_path": s("path") })),
        "edit" | "str_replace" => (
            "Edit".into(),
            json!({ "file_path": s("path"), "old_string": s("old_str"), "new_string": s("new_str") }),
        ),
        "create" => ("Write".into(), json!({ "file_path": s("path"), "content": s("file_text") })),
        "rg" => {
            let mut input = json!({ "pattern": s("pattern") });
            if let Some(p) = args.get("paths") {
                input["path"] = if p.is_array() {
                    json!(p.as_array().unwrap().iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" "))
                } else {
                    p.clone()
                };
            }
            if !s("glob").is_empty() {
                input["glob"] = json!(s("glob"));
            }
            ("Grep".into(), input)
        }
        "glob" => ("Glob".into(), json!({ "pattern": s("pattern"), "path": s("paths") })),
        "apply_patch" => ("ApplyPatch".into(), json!({ "patch": s("str") })),
        _ => (name.to_string(), keep(args)),
    }
}

/// Normalize parsed event records into the renderer's message model.
pub fn normalize(recs: &[Value]) -> Norm {
    let mut n = Norm::default();
    for rec in recs {
        let ty = rec.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let data = rec.get("data").cloned().unwrap_or(Value::Null);
        let ts = rec.get("timestamp").and_then(|v| v.as_str());
        let with_ts = |mut m: Value| {
            if let Some(t) = ts {
                m["ts"] = json!(t);
            }
            m
        };
        match ty {
            "session.start" => {
                if n.session_id.is_none() {
                    n.session_id = data.get("sessionId").and_then(|v| v.as_str()).map(|s| s.to_string());
                }
                if n.version.is_none() {
                    n.version = data.get("copilotVersion").and_then(|v| v.as_str()).map(|s| s.to_string());
                }
                if n.cwd.is_none() {
                    n.cwd = data
                        .get("context")
                        .and_then(|c| c.get("cwd"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                }
                if n.git_branch.is_none() {
                    n.git_branch = data
                        .get("context")
                        .and_then(|c| c.get("branch"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                }
            }
            "session.model_change" => {
                if let Some(m) = data.get("newModel").and_then(|v| v.as_str()) {
                    n.model = Some(m.to_string());
                }
            }
            "user.message" => {
                let text = data.get("content").and_then(|v| v.as_str()).unwrap_or("");
                if !text.trim().is_empty() {
                    n.messages
                        .push(with_ts(json!({ "role": "user", "content": [{ "type": "text", "text": text }] })));
                }
            }
            "assistant.message" => {
                if let Some(m) = data.get("model").and_then(|v| v.as_str()) {
                    n.model = Some(m.to_string());
                }
                let mut blocks: Vec<Value> = vec![];
                let text = data.get("content").and_then(|v| v.as_str()).unwrap_or("");
                if !text.trim().is_empty() {
                    blocks.push(json!({ "type": "text", "text": text }));
                }
                if let Some(calls) = data.get("toolRequests").and_then(|c| c.as_array()) {
                    for call in calls {
                        let name = call.get("name").and_then(|v| v.as_str()).unwrap_or("tool");
                        let args = call.get("arguments").cloned().unwrap_or(json!({}));
                        let (tname, input) = map_tool(name, &args);
                        let id = call.get("toolCallId").and_then(|v| v.as_str()).unwrap_or("");
                        blocks.push(json!({ "type": "tool_use", "id": id, "name": tname, "input": input }));
                    }
                }
                if !blocks.is_empty() {
                    let mut m = json!({ "role": "assistant", "content": blocks });
                    if let Some(md) = &n.model {
                        m["modelActual"] = json!(md);
                    }
                    n.messages.push(with_ts(m));
                }
            }
            "tool.execution_complete" => {
                let id = data.get("toolCallId").and_then(|v| v.as_str()).unwrap_or("");
                let text = data
                    .get("result")
                    .and_then(|r| r.get("content"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let mut tr = json!({ "type": "tool_result", "tool_use_id": id, "content": text });
                if data.get("success").and_then(|v| v.as_bool()) == Some(false) {
                    tr["is_error"] = json!(true);
                }
                n.messages.push(with_ts(json!({ "role": "user", "content": [tr] })));
            }
            _ => {} // session.info / system.* / turn markers / execution_start: harness plumbing
        }
    }
    n.first_ts = n.messages.first().and_then(|m| m.get("ts")).and_then(|v| v.as_str()).map(|s| s.to_string());
    n.last_ts = n.messages.last().and_then(|m| m.get("ts")).and_then(|v| v.as_str()).map(|s| s.to_string());
    n
}

/// List-row meta: workspace.yaml when present (new layout — has copilot's own session name),
/// else the event head (old flat layout).
pub fn session_meta_from(file: &Path, recs: &[Value], dir_id: &str, dir_label: &str) -> Option<Value> {
    let meta = fs::metadata(file).ok()?;
    let ws = workspace_yaml(file);
    let n = normalize(recs);
    let uuid = session_uuid(file);
    let (cc_title, cc_tags, cc_deleted) = sidecar_meta(file);
    let ws_str = |k: &str| {
        ws.as_ref()
            .and_then(|m| m.get(k))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
    };
    let auto_title = ws_str("name").unwrap_or_else(|| crate::history::first_user_text(&n.messages));
    let cwd = ws_str("cwd").or_else(|| n.cwd.clone());
    let created = ws_str("created_at")
        .or_else(|| n.first_ts.clone())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
        .map(|d| d.timestamp_millis() as f64)
        .unwrap_or_else(|| crate::history::created_ms(file));
    let mt = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0);
    Some(json!({
        "id": format!("copilot:{}", uuid),
        "file": file.to_string_lossy(),
        "source": "copilot",
        "dirId": dir_id,
        "dirLabel": dir_label,
        "sessionId": n.session_id.clone().unwrap_or_else(|| uuid.clone()),
        "cwd": cwd.clone(),
        "project": cwd.as_deref().map(crate::history::base_name).unwrap_or_default(),
        "gitBranch": ws_str("branch").or_else(|| n.git_branch.clone()).map(Value::from).unwrap_or(Value::Null),
        "title": cc_title.clone().unwrap_or_else(|| auto_title.clone()),
        "autoTitle": auto_title,
        "tags": cc_tags,
        "model": n.model,
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
    let n = normalize(recs);
    let ws = workspace_yaml(path);
    let (cc_title, cc_tags, cc_deleted) = sidecar_meta(path);
    let ws_name = ws
        .as_ref()
        .and_then(|m| m.get("name"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty());
    let auto_title = ws_name.unwrap_or_else(|| crate::history::first_user_text(&n.messages));
    let uuid = session_uuid(path);
    let cwd = ws
        .as_ref()
        .and_then(|m| m.get("cwd"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| n.cwd.clone());
    json!({
        "meta": {
            "id": format!("copilot:{}", uuid),
            "file": file,
            "source": "copilot",
            "assistant": "Copilot",
            "title": cc_title.clone().unwrap_or_else(|| auto_title.clone()),
            "autoTitle": auto_title,
            "tags": cc_tags,
            "summary": Value::Null,
            "sessionId": n.session_id.clone().unwrap_or_else(|| uuid.clone()),
            "cwd": cwd.clone(),
            "project": cwd.as_deref().map(crate::history::base_name).unwrap_or_default(),
            "gitBranch": n.git_branch.clone(),
            "version": n.version.clone(),
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

    fn ev(ts: &str, ty: &str, data: Value) -> Value {
        json!({ "type": ty, "data": data, "id": "e", "timestamp": ts, "parentId": null })
    }

    fn recs() -> Vec<Value> {
        vec![
            ev("2026-07-12T07:26:54.363Z", "session.start", json!({
                "sessionId": "d34d-1111", "copilotVersion": "1.0.70",
                "context": { "cwd": "/tmp/shhh", "gitRoot": "/tmp/shhh", "branch": "main" }
            })),
            ev("2026-07-12T07:27:00.685Z", "session.model_change", json!({ "newModel": "gpt-5.6" })),
            ev("2026-07-12T07:27:05.000Z", "system.message", json!({ "role": "system", "content": "You are Copilot" })),
            ev("2026-07-12T07:27:14.000Z", "user.message", json!({ "content": "修沙盒问题", "attachments": [] })),
            ev("2026-07-12T07:27:15.000Z", "assistant.message", json!({
                "messageId": "m1", "model": "gpt-5.6", "content": "我先搜一下。",
                "toolRequests": [
                    { "toolCallId": "call_A", "name": "rg", "arguments": { "pattern": "sandbox", "paths": ".", "glob": "*.plist" } },
                    { "toolCallId": "call_B", "name": "bash", "arguments": { "command": "ls", "description": "List", "mode": "sync", "sessionId": "main" } }
                ]
            })),
            ev("2026-07-12T07:27:16.000Z", "tool.execution_start", json!({ "toolCallId": "call_A", "toolName": "rg" })),
            ev("2026-07-12T07:27:17.000Z", "tool.execution_complete", json!({
                "toolCallId": "call_A", "success": true, "result": { "content": "a.plist: sandbox" }
            })),
            ev("2026-07-12T07:27:18.000Z", "tool.execution_complete", json!({
                "toolCallId": "call_B", "success": false, "result": { "content": "boom" }
            })),
        ]
    }

    #[test]
    fn normalizes_events() {
        let n = normalize(&recs());
        assert_eq!(n.messages.len(), 4); // user, assistant(+2 tools), 2 results
        assert_eq!(n.messages[0]["content"][0]["text"], "修沙盒问题");
        let a = &n.messages[1];
        assert_eq!(a["content"][0]["text"], "我先搜一下。");
        assert_eq!(a["content"][1]["name"], "Grep");
        assert_eq!(a["content"][1]["input"]["pattern"], "sandbox");
        assert_eq!(a["content"][2]["name"], "Bash");
        assert_eq!(n.messages[2]["content"][0]["tool_use_id"], "call_A");
        assert_eq!(n.messages[3]["content"][0]["is_error"], true);
        assert_eq!(n.model.as_deref(), Some("gpt-5.6"));
        assert_eq!(n.cwd.as_deref(), Some("/tmp/shhh"));
        assert_eq!(n.session_id.as_deref(), Some("d34d-1111"));
        assert_eq!(n.first_ts.as_deref(), Some("2026-07-12T07:27:14.000Z"));
    }

    #[test]
    fn detects_paths_and_uuids() {
        let new = Path::new("/x/.copilot/session-state/abcd-1/events.jsonl");
        let old = Path::new("/x/.copilot/session-state/abcd-2.jsonl");
        assert!(looks_copilot_path(new));
        assert!(looks_copilot_path(old));
        assert!(!looks_copilot_path(Path::new("/x/projects/-tmp/abcd.jsonl")));
        assert_eq!(session_uuid(new), "abcd-1");
        assert_eq!(session_uuid(old), "abcd-2");
    }
}
