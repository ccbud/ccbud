// Codex CLI session support — reads OpenAI Codex's on-disk rollout logs
// (`~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`) and normalizes them into the SAME
// session/message shape the renderer consumes for Claude Code history, so the 对话 view
// (list / detail / search / live-follow / export) browses both without renderer forks.
//
// A rollout line is `{timestamp, type, payload}` with type ∈ {session_meta, turn_context,
// response_item, event_msg, compacted}. Conversation content lives in response_item payloads
// (message / reasoning / function_call / function_call_output / local_shell_call /
// custom_tool_call / web_search_call); event_msg mostly duplicates that content (ignored)
// but its token_count records carry per-turn usage (harvested). Very old Codex builds wrote
// payload objects directly per line (no envelope) — handled by treating such a line as its
// own payload.
//
// Tool calls are mapped onto the tool vocabulary the renderer already draws natively:
// shell/exec_command/local_shell_call → Bash, update_plan → TodoWrite, view_image → Read,
// web_search → WebSearch, apply_patch → ApplyPatch (a codex-specific card).
//
// Title/tags/soft-delete: Codex files belong to another tool, so per-conversation
// customization never rewrites them (unlike Claude's in-file `__ccbud__`) — it lives in a
// sidecar map at `~/.ccbud/codex-meta.json`, keyed by the rollout file stem.

#![allow(dead_code)]

use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};

/// The DEFAULT config dir as a history-dir entry string (`~/.codex`), used by the one-time
/// startup migration that adds it to `historyDirs`. Honors CODEX_HOME like the codex CLI.
pub fn codex_label() -> String {
    let root = sessions_root();
    let dir = root.parent().unwrap_or(&root);
    crate::store::collapse_home(&dir.to_string_lossy())
}

fn home() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("."))
}

/// Codex's DEFAULT sessions tree. Honors CODEX_HOME the way the codex CLI does. Only the
/// auto-add migration keys off this — browsing walks `<dir>/sessions` of every configured dir.
pub fn sessions_root() -> PathBuf {
    match std::env::var("CODEX_HOME") {
        Ok(h) if !h.trim().is_empty() => PathBuf::from(h).join("sessions"),
        _ => home().join(".codex").join("sessions"),
    }
}

pub fn root_exists() -> bool {
    sessions_root().is_dir()
}

/// Walk every rollout .jsonl under a sessions tree (date-sharded YYYY/MM/DD, but walked
/// generically so a layout change doesn't lose sessions). Depth-capped against cycles.
pub fn walk_sessions<F: FnMut(PathBuf)>(root: &Path, mut cb: F) {
    fn walk<F: FnMut(PathBuf)>(dir: &Path, depth: u32, cb: &mut F) {
        if depth > 6 {
            return;
        }
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for ent in entries.flatten() {
            let p = ent.path();
            if p.is_dir() {
                walk(&p, depth + 1, cb);
            } else if p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                cb(p);
            }
        }
    }
    walk(root, 0, &mut cb);
}

/// Format sniff on parsed records — routes files that LOOK like Codex rollouts (incl. copies
/// imported into the app store, where the path no longer says so). Claude Code records never
/// use these type tags, and old-format bare Codex items lack Claude's `.message` wrapper.
pub fn looks_codex(recs: &[Value]) -> bool {
    recs.iter().take(8).any(|r| {
        match r.get("type").and_then(|v| v.as_str()) {
            Some("session_meta") | Some("turn_context") | Some("event_msg") | Some("compacted") => true,
            Some("response_item") => r.get("payload").is_some(),
            // old envelope-less rollout: response items at the top level
            Some("message") | Some("function_call") | Some("function_call_output")
            | Some("reasoning") | Some("local_shell_call") => r.get("message").is_none(),
            _ => r.get("record_type").is_some(),
        }
    })
}

/// (type, payload, timestamp) of a rollout line, tolerating the old envelope-less format.
fn split_line(rec: &Value) -> (&str, &Value, Option<&str>) {
    let ts = rec.get("timestamp").and_then(|v| v.as_str());
    let t = rec.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if let Some(p) = rec.get("payload") {
        return (t, p, ts);
    }
    match t {
        "message" | "function_call" | "function_call_output" | "reasoning" | "local_shell_call"
        | "custom_tool_call" | "custom_tool_call_output" | "web_search_call" => ("response_item", rec, ts),
        // old first line: bare SessionMeta {id, timestamp, instructions, cwd?, git?}
        "" if rec.get("id").is_some() && rec.get("timestamp").is_some() => ("session_meta", rec, ts),
        _ => (t, rec, ts),
    }
}

/// Harness-injected user turns (environment/permissions/instructions wrappers) that aren't
/// human prose — hidden from the timeline, exactly like Claude's isMeta records.
fn is_meta_user_text(t: &str) -> bool {
    let t = t.trim_start();
    ["<environment_context>", "<user_instructions>", "<permissions", "<ide_", "<turn_context", "<AGENTS", "<workspace_"]
        .iter()
        .any(|p| t.starts_with(p))
}

fn joined_text(content: &Value, kinds: &[&str]) -> String {
    let arr = match content.as_array() {
        Some(a) => a,
        None => return content.as_str().unwrap_or("").to_string(),
    };
    arr.iter()
        .filter(|b| kinds.contains(&b.get("type").and_then(|t| t.as_str()).unwrap_or("")))
        .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// argv → display command: unwrap the ["bash","-lc", script] convention, else shell-ish join.
fn join_argv(cmd: &Value) -> String {
    if let Some(s) = cmd.as_str() {
        return s.to_string();
    }
    let arr = match cmd.as_array() {
        Some(a) => a,
        None => return String::new(),
    };
    let parts: Vec<String> = arr.iter().map(|x| x.as_str().unwrap_or_default().to_string()).collect();
    if parts.len() == 3
        && ["bash", "sh", "zsh", "dash"].contains(&parts[0].as_str())
        && ["-lc", "-c"].contains(&parts[1].as_str())
    {
        return parts[2].clone();
    }
    parts
        .iter()
        .map(|p| {
            if p.is_empty() || p.chars().any(|c| c.is_whitespace() || c == '"' || c == '\'') {
                format!("{:?}", p) // debug-quote args with spaces/quotes
            } else {
                p.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Codex tool name + parsed arguments → (renderer tool name, renderer input).
fn map_tool(name: &str, args: &Value) -> (String, Value) {
    let s = |k: &str| args.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    match name {
        "shell" | "local_shell" | "container.exec" => {
            let mut input = json!({ "command": join_argv(args.get("command").unwrap_or(&Value::Null)) });
            let desc = if !s("justification").is_empty() { s("justification") } else { s("workdir") };
            if !desc.is_empty() {
                input["description"] = json!(desc);
            }
            ("Bash".into(), input)
        }
        "shell_command" => ("Bash".into(), json!({ "command": s("command") })),
        "exec_command" => {
            let cmd = if !s("cmd").is_empty() { s("cmd") } else { s("command") };
            ("Bash".into(), json!({ "command": cmd }))
        }
        "apply_patch" => {
            let patch = if !s("input").is_empty() { s("input") } else { s("patch") };
            ("ApplyPatch".into(), json!({ "patch": patch }))
        }
        "update_plan" => {
            let todos: Vec<Value> = args
                .get("plan")
                .and_then(|p| p.as_array())
                .map(|a| {
                    a.iter()
                        .map(|st| {
                            json!({
                                "content": st.get("step").and_then(|v| v.as_str()).unwrap_or(""),
                                "status": st.get("status").and_then(|v| v.as_str()).unwrap_or("pending"),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            ("TodoWrite".into(), json!({ "todos": todos }))
        }
        "view_image" => ("Read".into(), json!({ "file_path": s("path") })),
        "web_search" => ("WebSearch".into(), json!({ "query": s("query") })),
        _ => (
            name.to_string(),
            if args.is_object() { args.clone() } else { json!({}) },
        ),
    }
}

/// Tool output payload → (display text, is_error). Unwraps codex's JSON-wrapped shell output
/// ({"output","metadata":{exit_code}}) and reads exec_command's "exited with code N" header.
fn shape_output(out: &Value) -> (String, bool) {
    // structured payload: { content, success? }
    if out.is_object() {
        let text = out
            .get("content")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| serde_json::to_string_pretty(out).unwrap_or_default());
        let err = out.get("success").and_then(|v| v.as_bool()) == Some(false);
        return (text, err);
    }
    let s = out.as_str().unwrap_or("").to_string();
    if let Ok(v) = serde_json::from_str::<Value>(&s) {
        if v.is_object() {
            if let Some(o) = v.get("output").and_then(|x| x.as_str()) {
                let code = v
                    .get("metadata")
                    .and_then(|m| m.get("exit_code"))
                    .and_then(|c| c.as_i64())
                    .unwrap_or(0);
                return (o.to_string(), code != 0);
            }
            if let Some(c) = v.get("content").and_then(|x| x.as_str()) {
                let err = v.get("success").and_then(|x| x.as_bool()) == Some(false);
                return (c.to_string(), err);
            }
        }
    }
    // exec_command header: "…\nProcess exited with code N\n…" near the top
    let head: String = s.chars().take(240).collect();
    if let Some(pos) = head.find("exited with code ") {
        let digits: String = head[pos + "exited with code ".len()..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if let Ok(code) = digits.parse::<i64>() {
            return (s, code != 0);
        }
    }
    (s, false)
}

/// data-URL input_image → Claude-style image source block, else None.
fn image_block(url: &str) -> Option<Value> {
    let rest = url.strip_prefix("data:")?;
    let (mime, b64) = rest.split_once(";base64,")?;
    Some(json!({ "type": "image", "source": { "type": "base64", "media_type": mime, "data": b64 } }))
}

pub struct Norm {
    pub messages: Vec<Value>,
    pub totals: Value,
    pub model: Option<String>,
    pub first_ts: Option<String>,
    pub last_ts: Option<String>,
    pub cwd: Option<String>,
    pub session_id: Option<String>,
    pub git_branch: Option<String>,
    pub version: Option<String>,
}

/// Normalize parsed rollout records into the renderer's message model.
pub fn normalize(recs: &[Value]) -> Norm {
    let mut messages: Vec<Value> = vec![];
    let (mut tin, mut tout, mut tcr, mut turns) = (0i64, 0i64, 0i64, 0i64);
    let mut model: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut session_id: Option<String> = None;
    let mut git_branch: Option<String> = None;
    let mut version: Option<String> = None;

    for rec in recs {
        let (ty, p, ts) = split_line(rec);
        let with_ts = |mut m: Value| {
            if let Some(t) = ts {
                m["ts"] = json!(t);
            }
            m
        };
        match ty {
            "session_meta" => {
                let sid = p
                    .get("session_id")
                    .or_else(|| p.get("id"))
                    .and_then(|v| v.as_str());
                if session_id.is_none() {
                    session_id = sid.map(|s| s.to_string());
                }
                if cwd.is_none() {
                    cwd = p.get("cwd").and_then(|v| v.as_str()).map(|s| s.to_string());
                }
                if version.is_none() {
                    version = p.get("cli_version").and_then(|v| v.as_str()).map(|s| s.to_string());
                }
                if git_branch.is_none() {
                    git_branch = p
                        .get("git")
                        .and_then(|g| g.get("branch"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                }
            }
            "turn_context" => {
                if let Some(m) = p.get("model").and_then(|v| v.as_str()) {
                    model = Some(m.to_string());
                }
                if cwd.is_none() {
                    cwd = p.get("cwd").and_then(|v| v.as_str()).map(|s| s.to_string());
                }
            }
            "compacted" => {
                let text = p.get("message").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
                if !text.is_empty() {
                    messages.push(with_ts(json!({ "role": "user", "content": [{ "type": "text", "text": text }] })));
                }
            }
            "event_msg" => match p.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                "token_count" => {
                    let u = p.get("info").and_then(|i| i.get("last_token_usage"));
                    if let Some(u) = u {
                        let input = u.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                        let cached = u.get("cached_input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                        let output = u.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                        if input + cached + output > 0 {
                            let usage = json!({
                                "inputTokens": (input - cached).max(0),
                                "outputTokens": output,
                                "cacheRead": cached,
                                "cacheCreation": 0,
                            });
                            tin += (input - cached).max(0);
                            tout += output;
                            tcr += cached;
                            turns += 1;
                            // Per-turn usage rides the turn's last assistant message (codex emits
                            // one token_count per model turn).
                            if let Some(m) = messages
                                .iter_mut()
                                .rev()
                                .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("assistant") && m.get("usage").is_none())
                            {
                                m["usage"] = usage;
                            }
                        }
                    }
                }
                "turn_aborted" => {
                    messages.push(with_ts(json!({
                        "role": "user",
                        "content": [{ "type": "text", "text": "[Request interrupted by user]" }],
                    })));
                }
                _ => {}
            },
            "response_item" => {
                let it = p.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match it {
                    "message" => {
                        let role = p.get("role").and_then(|v| v.as_str()).unwrap_or("");
                        let content = p.get("content").cloned().unwrap_or(Value::Null);
                        if role == "assistant" {
                            let text = joined_text(&content, &["output_text", "text"]);
                            if !text.trim().is_empty() {
                                let mut m = json!({ "role": "assistant", "content": [{ "type": "text", "text": text }] });
                                if let Some(md) = &model {
                                    m["modelActual"] = json!(md);
                                }
                                messages.push(with_ts(m));
                            }
                        } else if role == "user" {
                            let text = joined_text(&content, &["input_text", "text"]);
                            if is_meta_user_text(&text) {
                                continue;
                            }
                            let mut blocks: Vec<Value> = vec![];
                            if !text.trim().is_empty() {
                                blocks.push(json!({ "type": "text", "text": text }));
                            }
                            if let Some(arr) = content.as_array() {
                                for b in arr {
                                    if b.get("type").and_then(|t| t.as_str()) == Some("input_image") {
                                        if let Some(img) = b
                                            .get("image_url")
                                            .and_then(|u| u.as_str())
                                            .and_then(image_block)
                                        {
                                            blocks.push(img);
                                        }
                                    }
                                }
                            }
                            if !blocks.is_empty() {
                                messages.push(with_ts(json!({ "role": "user", "content": blocks })));
                            }
                        } // system / developer turns: harness plumbing, not conversation
                    }
                    "reasoning" => {
                        let mut txt = joined_text(&p.get("summary").cloned().unwrap_or(Value::Null), &["summary_text", "text"]);
                        let extra = joined_text(&p.get("content").cloned().unwrap_or(Value::Null), &["reasoning_text", "text"]);
                        if !extra.trim().is_empty() {
                            if !txt.trim().is_empty() {
                                txt.push_str("\n\n");
                            }
                            txt.push_str(&extra);
                        }
                        if !txt.trim().is_empty() {
                            let mut m = json!({ "role": "assistant", "content": [{ "type": "thinking", "thinking": txt }] });
                            if let Some(md) = &model {
                                m["modelActual"] = json!(md);
                            }
                            messages.push(with_ts(m));
                        }
                    }
                    "function_call" => {
                        let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("tool");
                        let args: Value = p
                            .get("arguments")
                            .and_then(|v| v.as_str())
                            .and_then(|s| serde_json::from_str(s).ok())
                            .unwrap_or_else(|| p.get("arguments").cloned().unwrap_or(json!({})));
                        let (tname, input) = map_tool(name, &args);
                        let id = p
                            .get("call_id")
                            .or_else(|| p.get("id"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let mut m = json!({
                            "role": "assistant",
                            "content": [{ "type": "tool_use", "id": id, "name": tname, "input": input }],
                        });
                        if let Some(md) = &model {
                            m["modelActual"] = json!(md);
                        }
                        messages.push(with_ts(m));
                    }
                    "local_shell_call" => {
                        let cmd = p
                            .get("action")
                            .and_then(|a| a.get("command"))
                            .cloned()
                            .unwrap_or(Value::Null);
                        let id = p
                            .get("call_id")
                            .or_else(|| p.get("id"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let mut m = json!({
                            "role": "assistant",
                            "content": [{ "type": "tool_use", "id": id, "name": "Bash", "input": { "command": join_argv(&cmd) } }],
                        });
                        if let Some(md) = &model {
                            m["modelActual"] = json!(md);
                        }
                        messages.push(with_ts(m));
                    }
                    "custom_tool_call" => {
                        let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("tool");
                        let input_s = p.get("input").and_then(|v| v.as_str()).unwrap_or("");
                        let (tname, input) = if name == "apply_patch" {
                            ("ApplyPatch".to_string(), json!({ "patch": input_s }))
                        } else {
                            (name.to_string(), json!({ "input": input_s }))
                        };
                        let id = p
                            .get("call_id")
                            .or_else(|| p.get("id"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let mut m = json!({
                            "role": "assistant",
                            "content": [{ "type": "tool_use", "id": id, "name": tname, "input": input }],
                        });
                        if let Some(md) = &model {
                            m["modelActual"] = json!(md);
                        }
                        messages.push(with_ts(m));
                    }
                    "function_call_output" | "custom_tool_call_output" => {
                        let (text, err) = shape_output(p.get("output").unwrap_or(&Value::Null));
                        let id = p.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
                        let mut tr = json!({ "type": "tool_result", "tool_use_id": id, "content": text });
                        if err {
                            tr["is_error"] = json!(true);
                        }
                        messages.push(with_ts(json!({ "role": "user", "content": [tr] })));
                    }
                    "web_search_call" => {
                        let q = p
                            .get("action")
                            .and_then(|a| a.get("query"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let id = p
                            .get("id")
                            .or_else(|| p.get("call_id"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let mut m = json!({
                            "role": "assistant",
                            "content": [{ "type": "tool_use", "id": id, "name": "WebSearch", "input": { "query": q } }],
                        });
                        if let Some(md) = &model {
                            m["modelActual"] = json!(md);
                        }
                        messages.push(with_ts(m));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    let first_ts = messages
        .first()
        .and_then(|m| m.get("ts"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let last_ts = messages
        .last()
        .and_then(|m| m.get("ts"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    Norm {
        messages,
        totals: json!({ "in": tin, "out": tout, "cacheRead": tcr, "cacheCreation": 0, "turns": turns }),
        model,
        first_ts,
        last_ts,
        cwd,
        session_id,
        git_branch,
        version,
    }
}

/// (cwd, session_id) from a codex head — used by the import path to lay out the store copy.
pub fn head_ids(recs: &[Value]) -> (Option<String>, Option<String>) {
    for rec in recs {
        let (ty, p, _) = split_line(rec);
        if ty == "session_meta" {
            let cwd = p.get("cwd").and_then(|v| v.as_str()).map(|s| s.to_string());
            let sid = p
                .get("session_id")
                .or_else(|| p.get("id"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            return (cwd, sid);
        }
    }
    (None, None)
}

// ---- sidecar customization (~/.ccbud/codex-meta.json): { "<stem>": {title?, tagList?, delete?} } ----

fn sidecar_path() -> PathBuf {
    crate::store::ccbud_home().join("codex-meta.json")
}

fn sidecar_cache() -> &'static std::sync::Mutex<Option<(f64, Map<String, Value>)>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<Option<(f64, Map<String, Value>)>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(None))
}

fn sidecar_mtime() -> f64 {
    fs::metadata(sidecar_path())
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

fn read_sidecar() -> Map<String, Value> {
    let mt = sidecar_mtime();
    if let Ok(guard) = sidecar_cache().lock() {
        if let Some((cmt, map)) = guard.as_ref() {
            if *cmt == mt {
                return map.clone();
            }
        }
    }
    let map = fs::read_to_string(sidecar_path())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    if let Ok(mut guard) = sidecar_cache().lock() {
        *guard = Some((mt, map.clone()));
    }
    map
}

fn write_sidecar(map: &Map<String, Value>) -> bool {
    let dir = crate::store::ccbud_home();
    let _ = fs::create_dir_all(&dir);
    let file = sidecar_path();
    let tmp = dir.join("codex-meta.json.tmp");
    let bytes = match serde_json::to_vec_pretty(&Value::Object(map.clone())) {
        Ok(b) => b,
        Err(_) => return false,
    };
    if fs::write(&tmp, bytes).is_err() {
        return false;
    }
    if fs::rename(&tmp, &file).is_err() {
        let _ = fs::remove_file(&tmp);
        return false;
    }
    if let Ok(mut guard) = sidecar_cache().lock() {
        *guard = Some((sidecar_mtime(), map.clone()));
    }
    true
}

fn stem_of(file: &Path) -> String {
    file.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string()
}

/// (custom title, tags, deleted) for a codex session, from the sidecar.
fn sidecar_meta(file: &Path) -> (Option<String>, Vec<String>, bool) {
    let map = read_sidecar();
    let c = match map.get(&stem_of(file)) {
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

pub fn is_deleted(file: &Path) -> bool {
    sidecar_meta(file).2
}

/// set_ccbud-equivalent for codex sessions: same patch semantics ({title?, tags?, delete?}),
/// persisted to the sidecar instead of the rollout file (never mutate another tool's data).
pub fn set_meta(file: &str, patch: &Value) -> Value {
    let stem = stem_of(Path::new(file));
    if stem.is_empty() {
        return json!({ "ok": false, "reason": "empty" });
    }
    let mut map = read_sidecar();
    let mut next = map
        .get(&stem)
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
        map.remove(&stem);
    } else {
        map.insert(stem, Value::Object(next));
    }
    if write_sidecar(&map) {
        json!({ "ok": true })
    } else {
        json!({ "ok": false, "reason": "write" })
    }
}

/// Drop a session's sidecar entry (after its rollout file is deleted forever).
pub fn remove_meta(file: &str) {
    let stem = stem_of(Path::new(file));
    let mut map = read_sidecar();
    if map.remove(&stem).is_some() {
        let _ = write_sidecar(&map);
    }
}

// ---- list/detail shapes (codex flavors of history.rs session_meta / get_session) ----

/// List-row meta from already-parsed head records. `dir_id` is `__codex__` for the live tree
/// or `__imported__` for snapshots copied into the app store.
pub fn session_meta_from(file: &Path, recs: &[Value], dir_id: &str, dir_label: &str) -> Option<Value> {
    let meta = fs::metadata(file).ok()?;
    let n = normalize(recs);
    // Live rollouts customize via the sidecar (never rewrite another tool's files); imported
    // COPIES (marked by an .import.json) are our own files, where the standard in-file
    // __ccbud__ (written by set_ccbud) applies.
    let native = crate::history::read_import_meta(&file.to_string_lossy()).is_none();
    let (cc_title, cc_tags, cc_deleted) = if native {
        sidecar_meta(file)
    } else {
        crate::history::read_ccbud(recs)
    };
    let auto_title = crate::history::first_user_text(&n.messages);
    let stem = stem_of(file);
    let mt = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0);
    Some(json!({
        "id": format!("codex:{}", stem),
        "file": file.to_string_lossy(),
        "source": "codex",
        "dirId": dir_id,
        "dirLabel": dir_label,
        "sessionId": n.session_id.clone().unwrap_or_else(|| stem.clone()),
        "cwd": n.cwd.clone(),
        "project": n.cwd.as_deref().map(crate::history::base_name).unwrap_or_default(),
        "gitBranch": n.git_branch.clone(),
        "title": cc_title.clone().unwrap_or_else(|| auto_title.clone()),
        "autoTitle": auto_title,
        "tags": cc_tags,
        "model": n.model,
        "isSubagent": false,
        "imported": dir_id == "__imported__",
        "deleted": cc_deleted,
        "lastActivity": mt,
        "sizeKB": (meta.len() as f64 / 1024.0).round() as i64,
    }))
}

/// Full-detail shape from already-parsed records (history.rs get_session routes here).
pub fn session_from_recs(file: &str, recs: &[Value]) -> Value {
    let path = Path::new(file);
    let n = normalize(recs);
    let import_meta = crate::history::read_import_meta(file);
    // Same sidecar-vs-in-file split as session_meta_from.
    let (cc_title, cc_tags, cc_deleted) = if import_meta.is_none() {
        sidecar_meta(path)
    } else {
        crate::history::read_ccbud(recs)
    };
    let auto_title = crate::history::first_user_text(&n.messages);
    let stem = stem_of(path);
    json!({
        "meta": {
            "id": format!("codex:{}", stem),
            "file": file,
            "source": "codex",
            "assistant": "Codex",
            "title": cc_title.clone().unwrap_or_else(|| auto_title.clone()),
            "autoTitle": auto_title,
            "tags": cc_tags,
            "summary": Value::Null,
            "sessionId": n.session_id.clone().unwrap_or_else(|| stem.clone()),
            "cwd": n.cwd.clone(),
            "project": n.cwd.as_deref().map(crate::history::base_name).unwrap_or_default(),
            "gitBranch": n.git_branch.clone(),
            "version": n.version.clone(),
            "isSubagent": false,
            "deleted": cc_deleted,
            "imported": import_meta.is_some(),
            "importedFrom": import_meta.as_ref().and_then(|m| m.get("originalPath")).cloned().unwrap_or(Value::Null),
            "importedAt": import_meta.as_ref().and_then(|m| m.get("importedAt")).cloned().unwrap_or(Value::Null),
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

    fn line(ts: &str, ty: &str, payload: Value) -> String {
        serde_json::to_string(&json!({ "timestamp": ts, "type": ty, "payload": payload })).unwrap()
    }

    fn fixture() -> Vec<Value> {
        let lines = vec![
            line("2026-07-04T07:13:08.965Z", "session_meta", json!({
                "session_id": "019f-abc", "id": "019f-abc", "timestamp": "2026-07-04T07:13:07.386Z",
                "cwd": "/tmp/projx", "originator": "codex-tui", "cli_version": "0.142.5",
                "git": { "branch": "main" }
            })),
            line("2026-07-04T07:13:08.967Z", "turn_context", json!({ "cwd": "/tmp/projx", "model": "gpt-5.5" })),
            line("2026-07-04T07:13:08.967Z", "response_item", json!({
                "type": "message", "role": "user",
                "content": [{ "type": "input_text", "text": "<environment_context>\n<cwd>/tmp/projx</cwd>\n</environment_context>" }]
            })),
            line("2026-07-04T07:13:08.969Z", "response_item", json!({
                "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "fix the bug please" }]
            })),
            line("2026-07-04T07:13:09.100Z", "event_msg", json!({ "type": "user_message", "message": "fix the bug please" })),
            line("2026-07-04T07:13:10.000Z", "response_item", json!({
                "type": "reasoning", "summary": [{ "type": "summary_text", "text": "Looking at the repo" }], "encrypted_content": "xxx"
            })),
            line("2026-07-04T07:13:11.000Z", "response_item", json!({
                "type": "function_call", "name": "exec_command",
                "arguments": "{\"cmd\": \"ls -la\", \"yield_time_ms\": 10000}", "call_id": "call_1"
            })),
            line("2026-07-04T07:13:12.000Z", "response_item", json!({
                "type": "function_call_output", "call_id": "call_1",
                "output": "Chunk ID: x\nWall time: 0.1 seconds\nProcess exited with code 0\nOutput:\n---\na.txt\nb.txt"
            })),
            line("2026-07-04T07:13:13.000Z", "response_item", json!({
                "type": "function_call", "name": "shell",
                "arguments": "{\"command\": [\"bash\", \"-lc\", \"cargo test\"], \"workdir\": \"/tmp/projx\"}", "call_id": "call_2"
            })),
            line("2026-07-04T07:13:14.000Z", "response_item", json!({
                "type": "function_call_output", "call_id": "call_2",
                "output": "{\"output\": \"error: it broke\", \"metadata\": {\"exit_code\": 101, \"duration_seconds\": 1.5}}"
            })),
            line("2026-07-04T07:13:15.000Z", "response_item", json!({
                "type": "function_call", "name": "update_plan",
                "arguments": "{\"plan\": [{\"step\": \"read code\", \"status\": \"completed\"}, {\"step\": \"fix bug\", \"status\": \"in_progress\"}]}",
                "call_id": "call_3"
            })),
            line("2026-07-04T07:13:16.000Z", "response_item", json!({
                "type": "custom_tool_call", "name": "apply_patch", "call_id": "call_4",
                "input": "*** Begin Patch\n*** Update File: src/a.rs\n@@\n-old\n+new\n*** End Patch"
            })),
            line("2026-07-04T07:13:17.000Z", "response_item", json!({
                "type": "message", "role": "assistant",
                "content": [{ "type": "output_text", "text": "Done — fixed." }], "phase": "final_answer"
            })),
            line("2026-07-04T07:13:17.500Z", "event_msg", json!({
                "type": "token_count",
                "info": {
                    "total_token_usage": { "input_tokens": 900, "cached_input_tokens": 600, "output_tokens": 80, "total_tokens": 980 },
                    "last_token_usage": { "input_tokens": 900, "cached_input_tokens": 600, "output_tokens": 80, "total_tokens": 980 },
                    "model_context_window": 258400
                }
            })),
        ];
        lines
            .iter()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .collect()
    }

    #[test]
    fn normalizes_rollout_into_renderer_model() {
        let recs = fixture();
        assert!(looks_codex(&recs));
        let n = normalize(&recs);

        assert_eq!(n.session_id.as_deref(), Some("019f-abc"));
        assert_eq!(n.cwd.as_deref(), Some("/tmp/projx"));
        assert_eq!(n.version.as_deref(), Some("0.142.5"));
        assert_eq!(n.git_branch.as_deref(), Some("main"));
        assert_eq!(n.model.as_deref(), Some("gpt-5.5"));

        // env-context user turn skipped; real prose, reasoning, 4 tool calls, 2 results, final text
        let roles: Vec<&str> = n.messages.iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(roles, vec!["user", "assistant", "assistant", "user", "assistant", "user", "assistant", "assistant", "assistant"]);

        let title = crate::history::first_user_text(&n.messages);
        assert_eq!(title, "fix the bug please");

        // exec_command → Bash card with the raw command
        let tu1 = &n.messages[2]["content"][0];
        assert_eq!(tu1["type"], "tool_use");
        assert_eq!(tu1["name"], "Bash");
        assert_eq!(tu1["input"]["command"], "ls -la");
        // its ok result pairs by call id and is not an error
        let tr1 = &n.messages[3]["content"][0];
        assert_eq!(tr1["tool_use_id"], "call_1");
        assert!(tr1.get("is_error").is_none());

        // shell argv ["bash","-lc","cargo test"] unwraps; exit_code 101 marks the result as error
        let tu2 = &n.messages[4]["content"][0];
        assert_eq!(tu2["input"]["command"], "cargo test");
        let tr2 = &n.messages[5]["content"][0];
        assert_eq!(tr2["is_error"], true);
        assert_eq!(tr2["content"], "error: it broke");

        // update_plan → TodoWrite todos
        let tu3 = &n.messages[6]["content"][0];
        assert_eq!(tu3["name"], "TodoWrite");
        assert_eq!(tu3["input"]["todos"][1]["status"], "in_progress");

        // apply_patch custom tool → ApplyPatch {patch}
        let tu4 = &n.messages[7]["content"][0];
        assert_eq!(tu4["name"], "ApplyPatch");
        assert!(tu4["input"]["patch"].as_str().unwrap().contains("*** Update File: src/a.rs"));

        // reasoning became a thinking block
        assert_eq!(n.messages[1]["content"][0]["type"], "thinking");

        // token_count landed on the final assistant text turn and rolled into totals
        let last = n.messages.last().unwrap();
        assert_eq!(last["usage"]["inputTokens"], 300); // input − cached
        assert_eq!(last["usage"]["cacheRead"], 600);
        assert_eq!(n.totals["out"], 80);
        assert_eq!(n.totals["turns"], 1);

        // timestamps span the emitted messages
        assert_eq!(n.first_ts.as_deref(), Some("2026-07-04T07:13:08.969Z"));
        assert_eq!(n.last_ts.as_deref(), Some("2026-07-04T07:13:17.000Z"));
    }

    // Machine-data smoke: run explicitly with `cargo test --lib -- --ignored` on a machine that
    // has real Codex sessions. Verifies every real rollout sniffs + normalizes + shapes.
    #[test]
    #[ignore]
    fn real_codex_sessions_smoke() {
        if !root_exists() {
            eprintln!("no ~/.codex/sessions — skipping");
            return;
        }
        let mut n = 0;
        let label = codex_label();
        walk_sessions(&sessions_root(), |p| {
            let raw = fs::read_to_string(&p).unwrap_or_default();
            let recs = crate::history::parse_lines(&raw);
            assert!(looks_codex(&recs), "not sniffed as codex: {:?}", p);
            let norm = normalize(&recs);
            assert!(norm.session_id.is_some() || norm.messages.is_empty(), "no session id: {:?}", p);
            let sess = session_from_recs(&p.to_string_lossy(), &recs);
            assert_eq!(sess["meta"]["assistant"], "Codex");
            let listed = session_meta_from(&p, &recs, &label, &label).unwrap();
            assert_eq!(listed["source"], "codex");
            n += 1;
        });
        eprintln!("smoke-checked {} real codex sessions", n);
    }

    #[test]
    fn claude_records_do_not_sniff_as_codex() {
        let recs = vec![
            json!({ "type": "user", "message": { "role": "user", "content": "hi" }, "cwd": "/x", "sessionId": "s1" }),
            json!({ "type": "assistant", "message": { "role": "assistant", "content": [{ "type": "text", "text": "hello" }] } }),
            json!({ "type": "summary", "summary": "greeting" }),
        ];
        assert!(!looks_codex(&recs));
    }

    #[test]
    fn old_envelope_less_rollout_still_parses() {
        let recs = vec![
            json!({ "id": "old-1", "timestamp": "2025-05-01T00:00:00Z", "instructions": "x", "cwd": "/tmp/old" }),
            json!({ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "hello old codex" }] }),
            json!({ "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": "hi" }] }),
        ];
        assert!(looks_codex(&recs));
        let n = normalize(&recs);
        assert_eq!(n.session_id.as_deref(), Some("old-1"));
        assert_eq!(n.cwd.as_deref(), Some("/tmp/old"));
        assert_eq!(n.messages.len(), 2);
        assert_eq!(crate::history::first_user_text(&n.messages), "hello old codex");
    }

    #[test]
    fn turn_aborted_and_web_search_render() {
        let recs: Vec<Value> = vec![
            serde_json::from_str(&line("2026-01-01T00:00:00Z", "response_item", json!({
                "type": "web_search_call", "id": "ws_1", "action": { "type": "search", "query": "rust serde" }
            }))).unwrap(),
            serde_json::from_str(&line("2026-01-01T00:00:01Z", "event_msg", json!({ "type": "turn_aborted", "reason": "interrupted" }))).unwrap(),
        ];
        let n = normalize(&recs);
        assert_eq!(n.messages[0]["content"][0]["name"], "WebSearch");
        assert_eq!(n.messages[0]["content"][0]["input"]["query"], "rust serde");
        assert!(n.messages[1]["content"][0]["text"].as_str().unwrap().starts_with("[Request interrupted"));
    }
}
