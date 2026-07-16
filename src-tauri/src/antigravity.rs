// Google Antigravity CLI (`agy`) session support — reads its per-conversation SQLite stores
// (`~/.gemini/antigravity-cli/conversations/<uuid>.db`, `steps` table) plus the sibling
// `conversation_summaries.db` (title / preview / workspace uris — plain text), and normalizes
// them into the SAME session/message shape the renderer consumes (history::Norm).
//
// A step's `step_payload` is a protobuf blob with no published schema. A minimal wire-format
// walker recovers the stable fields (reverse-engineered against real conversations):
//   #1  step type enum        #4 status
//   #5  metadata: #5.1 {sec,nanos} created · #5.4 tool call {#1 id, #2 name, #3 args-JSON,
//       #7 result (opaque/encrypted — not recoverable)} · #5.9 generation stats
//       {#2 input tokens, #3 output tokens}
//   #19 user input: #19.2 text · #19.9 attachments {#1 mime, #2 bytes, #5 path}
//   #20 model turn: #20.1 assistant text
// Steps whose payload drifts from this map degrade to being skipped (never crash) — the
// summaries DB alone still lists the conversation. Tool RESULTS are stored in a non-readable
// encoding, so tool cards show name/args and the renderer's "no result" marker.
//
// DBs may be WAL-journaled and open in a live agy process: connections are read-only with a
// short busy timeout, and freshness checks use max(mtime(db), mtime(db-wal)).
//
// Title/tags/soft-delete live in the shared foreign-CLI sidecar (~/.ccbud/agent-meta.json)
// keyed `antigravity:<uuid>` — the DBs belong to another tool and are never written.

#![allow(dead_code)]

use crate::history::Norm;
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

fn home() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("."))
}

/// Antigravity CLI's data dir as a history-dir entry string (`~/.gemini/antigravity-cli`).
pub fn default_root() -> PathBuf {
    home().join(".gemini").join("antigravity-cli")
}

pub fn agy_label() -> String {
    crate::store::collapse_home(&default_root().to_string_lossy())
}

pub fn root_exists() -> bool {
    default_root().join("conversations").is_dir()
}

/// Walk every conversation DB under a `conversations/` dir.
pub fn walk<F: FnMut(PathBuf)>(conversations_dir: &Path, cb: &mut F) {
    let entries = match fs::read_dir(conversations_dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for ent in entries.flatten() {
        let p = ent.path();
        if p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("db") {
            cb(p);
        }
    }
}

/// Container-shape test for detail/edit routing: `…/conversations/<uuid>.db`.
pub fn looks_agy_path(file: &Path) -> bool {
    file.extension().and_then(|e| e.to_str()) == Some("db")
        && file
            .parent()
            .and_then(|d| d.file_name())
            .and_then(|n| n.to_str())
            .map(|n| n == "conversations")
            .unwrap_or(false)
}

fn session_uuid(file: &Path) -> String {
    file.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string()
}

fn sidecar_key(file: &Path) -> String {
    format!("antigravity:{}", session_uuid(file))
}

fn sidecar_meta(file: &Path) -> (Option<String>, Vec<String>, bool) {
    crate::sidecar::meta(&crate::sidecar::agent_file(), &sidecar_key(file))
}

pub fn is_deleted(file: &Path) -> bool {
    sidecar_meta(file).2
}

pub fn set_meta(file: &str, patch: &Value) -> Value {
    let key = sidecar_key(Path::new(file));
    if key == "antigravity:" {
        return json!({ "ok": false, "reason": "empty" });
    }
    crate::sidecar::set_meta(&crate::sidecar::agent_file(), &key, patch)
}

/// WAL-aware freshness stamp: a live agy writes into `<db>-wal` without touching the main
/// file's mtime, so cache keys must take the max of both.
pub fn wal_mtime_ms(file: &Path) -> f64 {
    let m = |p: &Path| {
        fs::metadata(p)
            .and_then(|md| md.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as f64)
            .unwrap_or(0.0)
    };
    let mut wal = file.as_os_str().to_os_string();
    wal.push("-wal");
    m(file).max(m(Path::new(&wal)))
}

fn open_ro(path: &Path) -> Option<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    let _ = conn.busy_timeout(std::time::Duration::from_millis(400));
    Some(conn)
}

// ---- protobuf wire walker (schema-less) ----

enum Wire {
    Varint(u64),
    Bytes(Vec<u8>),
    Fixed,
}

/// One message level → (field number, value) pairs. None when the buffer isn't a valid message.
fn wire_fields(buf: &[u8]) -> Option<Vec<(u32, Wire)>> {
    let mut out = vec![];
    let mut i = 0usize;
    fn varint(buf: &[u8], i: &mut usize) -> Option<u64> {
        let mut v: u64 = 0;
        let mut shift = 0u32;
        loop {
            let b = *buf.get(*i)?;
            *i += 1;
            v |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Some(v);
            }
            shift += 7;
            if shift > 63 {
                return None;
            }
        }
    }
    while i < buf.len() {
        let tag = varint(buf, &mut i)?;
        let (field, wt) = ((tag >> 3) as u32, tag & 7);
        if field == 0 {
            return None;
        }
        match wt {
            0 => out.push((field, Wire::Varint(varint(buf, &mut i)?))),
            2 => {
                let len = varint(buf, &mut i)? as usize;
                if i + len > buf.len() {
                    return None;
                }
                out.push((field, Wire::Bytes(buf[i..i + len].to_vec())));
                i += len;
            }
            5 => {
                if i + 4 > buf.len() {
                    return None;
                }
                i += 4;
                out.push((field, Wire::Fixed));
            }
            1 => {
                if i + 8 > buf.len() {
                    return None;
                }
                i += 8;
                out.push((field, Wire::Fixed));
            }
            _ => return None,
        }
    }
    Some(out)
}

fn field_bytes<'a>(fields: &'a [(u32, Wire)], no: u32) -> Option<&'a [u8]> {
    fields.iter().find_map(|(f, w)| match w {
        Wire::Bytes(b) if *f == no => Some(b.as_slice()),
        _ => None,
    })
}

fn field_msg(fields: &[(u32, Wire)], no: u32) -> Option<Vec<(u32, Wire)>> {
    wire_fields(field_bytes(fields, no)?)
}

fn field_str(fields: &[(u32, Wire)], no: u32) -> Option<String> {
    let b = field_bytes(fields, no)?;
    let s = std::str::from_utf8(b).ok()?;
    Some(s.to_string())
}

fn field_varint(fields: &[(u32, Wire)], no: u32) -> Option<u64> {
    fields.iter().find_map(|(f, w)| match w {
        Wire::Varint(v) if *f == no => Some(*v),
        _ => None,
    })
}

/// `{#1 seconds, #2 nanos}` timestamp message → RFC3339 (ms precision).
fn ts_of(fields: &[(u32, Wire)], no: u32) -> Option<String> {
    let m = field_msg(fields, no)?;
    let secs = field_varint(&m, 1)? as i64;
    let nanos = field_varint(&m, 2).unwrap_or(0) as u32;
    let dt = chrono::DateTime::from_timestamp(secs, nanos)?;
    Some(dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

fn ts_ms_of(fields: &[(u32, Wire)], no: u32) -> Option<f64> {
    let m = field_msg(fields, no)?;
    let secs = field_varint(&m, 1)? as f64;
    let nanos = field_varint(&m, 2).unwrap_or(0) as f64;
    Some(secs * 1000.0 + (nanos / 1_000_000.0).floor())
}

// ---- content mapping ----

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    out
}

/// Antigravity tool name + parsed arguments → (renderer tool name, renderer input). The args
/// JSON carries display strings (toolAction/toolSummary) alongside the real params — dropped
/// from generic passthrough to keep cards clean.
fn map_tool(name: &str, args: &Value) -> (String, Value) {
    let s = |k: &str| args.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    match name {
        "run_command" => {
            let mut input = json!({ "command": s("CommandLine") });
            if !s("Cwd").is_empty() {
                input["description"] = json!(s("Cwd"));
            }
            ("Bash".into(), input)
        }
        "view_file" => ("Read".into(), json!({ "file_path": s("AbsolutePath") })),
        "list_dir" => ("LS".into(), json!({ "path": s("DirectoryPath") })),
        "grep_search" => {
            let mut input = json!({ "pattern": s("Query") });
            if !s("SearchPath").is_empty() {
                input["path"] = json!(s("SearchPath"));
            }
            ("Grep".into(), input)
        }
        "find_by_name" => ("Glob".into(), json!({ "pattern": s("Pattern"), "path": s("SearchDirectory") })),
        "replace_file_content" => (
            "Edit".into(),
            json!({ "file_path": s("TargetFile"), "old_string": s("TargetContent"), "new_string": s("ReplacementContent") }),
        ),
        "write_to_file" => (
            "Write".into(),
            json!({ "file_path": s("TargetFile"), "content": s("CodeContent") }),
        ),
        "read_url_content" => ("WebFetch".into(), json!({ "url": s("Url") })),
        "search_web" => ("WebSearch".into(), json!({ "query": s("query") })),
        _ => {
            let mut input = args.clone();
            if let Some(o) = input.as_object_mut() {
                o.remove("toolAction");
                o.remove("toolSummary");
            }
            (name.to_string(), if input.is_object() { input } else { json!({}) })
        }
    }
}

/// Decode one step row into zero or more renderer messages, accumulating usage into `n`.
fn push_step(n: &mut Norm, payload: &[u8]) {
    let fields = match wire_fields(payload) {
        Some(f) => f,
        None => return,
    };
    let meta5 = field_msg(&fields, 5).unwrap_or_default();
    let ts = ts_of(&meta5, 1);
    let with_ts = |mut m: Value| {
        if let Some(t) = &ts {
            m["ts"] = json!(t);
        }
        m
    };

    // user turn: #19 {2: text, 9: attachments {1 mime, 2 bytes, 5 path}}
    if let Some(user) = field_msg(&fields, 19) {
        let mut blocks: Vec<Value> = vec![];
        if let Some(text) = field_str(&user, 2) {
            if !text.trim().is_empty() {
                blocks.push(json!({ "type": "text", "text": text }));
            }
        }
        for (f, w) in &user {
            if *f != 9 {
                continue;
            }
            if let Wire::Bytes(b) = w {
                if let Some(att) = wire_fields(b) {
                    let mime = field_str(&att, 1).unwrap_or_default();
                    let data = field_bytes(&att, 2);
                    match data {
                        // cap embedded images at 8 MB raw — larger ones degrade to a path note
                        Some(bytes) if mime.starts_with("image/") && bytes.len() <= 8_000_000 => {
                            blocks.push(json!({
                                "type": "image",
                                "source": { "type": "base64", "media_type": mime, "data": b64_encode(bytes) }
                            }));
                        }
                        _ => {
                            if let Some(p) = field_str(&att, 5) {
                                blocks.push(json!({ "type": "text", "text": format!("[attachment: {}]", p) }));
                            }
                        }
                    }
                }
            }
        }
        if !blocks.is_empty() {
            n.messages.push(with_ts(json!({ "role": "user", "content": blocks })));
        }
        return;
    }

    // tool call: #5.4 {1 id, 2 name, 3 args-json} (results are stored opaquely — omitted)
    if let Some(call) = field_msg(&meta5, 4) {
        let name = field_str(&call, 2).unwrap_or_else(|| "tool".into());
        let args: Value = field_str(&call, 3)
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(json!({}));
        let (tname, input) = map_tool(&name, &args);
        let id = field_str(&call, 1).unwrap_or_default();
        n.messages.push(with_ts(json!({
            "role": "assistant",
            "content": [{ "type": "tool_use", "id": id, "name": tname, "input": input }],
        })));
        return;
    }

    // model turn: #20.1 assistant text; #5.9 {2 input, 3 output} token stats
    let turn20 = field_msg(&fields, 20);
    let text = turn20.as_ref().and_then(|t| field_str(t, 1).or_else(|| field_str(t, 8)));
    let stats = field_msg(&meta5, 9);
    let usage = stats.as_ref().map(|st| {
        let input = field_varint(st, 2).unwrap_or(0) as i64;
        let output = field_varint(st, 3).unwrap_or(0) as i64;
        json!({ "inputTokens": input, "outputTokens": output, "cacheRead": 0, "cacheCreation": 0 })
    });
    if let Some(text) = text {
        if !text.trim().is_empty() {
            let mut m = json!({ "role": "assistant", "content": [{ "type": "text", "text": text }] });
            if let Some(u) = &usage {
                let input = u.get("inputTokens").and_then(|v| v.as_i64()).unwrap_or(0);
                let output = u.get("outputTokens").and_then(|v| v.as_i64()).unwrap_or(0);
                if input + output > 0 {
                    m["usage"] = u.clone();
                    let t = n.totals.as_object_mut().unwrap();
                    t["in"] = json!(t["in"].as_i64().unwrap_or(0) + input);
                    t["out"] = json!(t["out"].as_i64().unwrap_or(0) + output);
                    t["turns"] = json!(t["turns"].as_i64().unwrap_or(0) + 1);
                }
            }
            n.messages.push(with_ts(m));
        }
    }
}

/// Depth-first search for the first utf8 string field with `prefix` anywhere in a message tree.
fn find_str_with_prefix(buf: &[u8], prefix: &str, depth: u8) -> Option<String> {
    let fields = wire_fields(buf)?;
    for (_, w) in &fields {
        if let Wire::Bytes(b) = w {
            if let Ok(s) = std::str::from_utf8(b) {
                if s.starts_with(prefix) {
                    return Some(s.to_string());
                }
            }
            if depth < 6 {
                if let Some(found) = find_str_with_prefix(b, prefix, depth + 1) {
                    return Some(found);
                }
            }
        }
    }
    None
}

fn uri_to_path(uri: &str) -> String {
    crate::grok::percent_decode(uri.strip_prefix("file://").unwrap_or(uri))
}

/// Workspace cwd for conversations the summaries DB hasn't indexed (a few percent of real
/// stores): the per-conversation trajectory_metadata_blob embeds the workspace file:// uri.
fn fallback_cwd(file: &Path) -> Option<String> {
    let conn = open_ro(file)?;
    let blob: Vec<u8> = conn
        .query_row("SELECT data FROM trajectory_metadata_blob LIMIT 1", [], |r| r.get(0))
        .ok()?;
    find_str_with_prefix(&blob, "file://", 0).map(|u| uri_to_path(&u))
}

/// Title-of-last-resort for un-indexed conversations: the first user step's prose.
fn first_user_step_text(file: &Path) -> Option<String> {
    let conn = open_ro(file)?;
    let payload: Vec<u8> = conn
        .query_row("SELECT step_payload FROM steps WHERE step_type = 14 ORDER BY idx LIMIT 1", [], |r| r.get(0))
        .ok()?;
    let fields = wire_fields(&payload)?;
    let user = field_msg(&fields, 19)?;
    let text = field_str(&user, 2)?;
    let t: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.is_empty() {
        None
    } else {
        Some(t.chars().take(90).collect())
    }
}

/// One conversation's summaries-DB row: (title, preview, first workspace path, step_count).
fn summaries_row(file: &Path) -> Option<(String, String, Option<String>, i64)> {
    let root = file.parent()?.parent()?;
    let conn = open_ro(&root.join("conversation_summaries.db"))?;
    let uuid = session_uuid(file);
    conn.query_row(
        "SELECT title, preview, workspace_uris, step_count FROM conversation_summaries WHERE conversation_id = ?1",
        [&uuid],
        |row| {
            let title: String = row.get(0).unwrap_or_default();
            let preview: String = row.get(1).unwrap_or_default();
            let uris: String = row.get(2).unwrap_or_default();
            let steps: i64 = row.get(3).unwrap_or(0);
            Ok((title, preview, uris, steps))
        },
    )
    .ok()
    .map(|(title, preview, uris, steps)| {
        let cwd = serde_json::from_str::<Value>(&uris)
            .ok()
            .and_then(|v| v.as_array().and_then(|a| a.first().cloned()))
            .and_then(|u| u.as_str().map(|s| s.to_string()))
            .map(|u| uri_to_path(&u));
        (title, preview, cwd, steps)
    })
}

/// Read + normalize a conversation DB into the renderer's message model.
pub fn normalize_db(file: &Path) -> Norm {
    let mut n = Norm::default();
    if let Some((_, _, cwd, _)) = summaries_row(file) {
        n.cwd = cwd;
    }
    if n.cwd.is_none() {
        n.cwd = fallback_cwd(file);
    }
    n.session_id = Some(session_uuid(file));
    let conn = match open_ro(file) {
        Some(c) => c,
        None => return n,
    };
    let mut stmt = match conn.prepare("SELECT step_payload FROM steps ORDER BY idx") {
        Ok(s) => s,
        Err(_) => return n,
    };
    let rows = stmt.query_map([], |row| row.get::<_, Vec<u8>>(0));
    if let Ok(rows) = rows {
        for payload in rows.flatten() {
            push_step(&mut n, &payload);
        }
    }
    n.first_ts = n.messages.first().and_then(|m| m.get("ts")).and_then(|v| v.as_str()).map(|s| s.to_string());
    n.last_ts = n.messages.last().and_then(|m| m.get("ts")).and_then(|v| v.as_str()).map(|s| s.to_string());
    n
}

/// Creation stamp (ms) from the first step's timestamp — content-derived, immune to file
/// rewrites, matching record_created_ms semantics for jsonl sources.
fn first_step_ms(file: &Path) -> Option<f64> {
    let conn = open_ro(file)?;
    let payload: Vec<u8> = conn
        .query_row("SELECT step_payload FROM steps ORDER BY idx LIMIT 1", [], |r| r.get(0))
        .ok()?;
    let fields = wire_fields(&payload)?;
    let meta5 = field_msg(&fields, 5)?;
    ts_ms_of(&meta5, 1)
}

/// List-row meta — summaries DB + first-step timestamp; never parses the full step log.
pub fn session_meta_from(file: &Path, dir_id: &str, dir_label: &str) -> Option<Value> {
    let meta = fs::metadata(file).ok()?;
    let uuid = session_uuid(file);
    let (cc_title, cc_tags, cc_deleted) = sidecar_meta(file);
    let sum = summaries_row(file);
    let (sum_title, preview, cwd) = match &sum {
        Some((t, p, c, _)) => (t.trim().to_string(), p.trim().to_string(), c.clone()),
        None => (String::new(), String::new(), None),
    };
    let cwd = cwd.or_else(|| fallback_cwd(file));
    let auto_title: String = if !sum_title.is_empty() {
        sum_title
    } else if !preview.is_empty() {
        preview.chars().take(90).collect()
    } else {
        first_user_step_text(file).unwrap_or_default()
    };
    let created = first_step_ms(file).unwrap_or_else(|| crate::history::created_ms(file));
    Some(json!({
        "id": format!("antigravity:{}", uuid),
        "file": file.to_string_lossy(),
        "source": "antigravity",
        "dirId": dir_id,
        "dirLabel": dir_label,
        "sessionId": uuid,
        "cwd": cwd.clone(),
        "project": cwd.as_deref().map(crate::history::base_name).unwrap_or_default(),
        "gitBranch": Value::Null,
        "title": cc_title.clone().unwrap_or_else(|| auto_title.clone()),
        "autoTitle": auto_title,
        "tags": cc_tags,
        "model": Value::Null,
        "isSubagent": false,
        "imported": false,
        "deleted": cc_deleted,
        "createdAt": created,
        "lastActivity": wal_mtime_ms(file),
        "sizeKB": (meta.len() as f64 / 1024.0).round() as i64,
    }))
}

/// Full-detail shape (history.rs get_session routes here — the source is SQLite, not jsonl).
pub fn session_from(file: &str) -> Value {
    let path = Path::new(file);
    let n = normalize_db(path);
    let (cc_title, cc_tags, cc_deleted) = sidecar_meta(path);
    let sum = summaries_row(path);
    let sum_title = sum
        .as_ref()
        .map(|(t, _, _, _)| t.trim().to_string())
        .filter(|s| !s.is_empty());
    let auto_title = sum_title.unwrap_or_else(|| crate::history::first_user_text(&n.messages));
    let uuid = session_uuid(path);
    json!({
        "meta": {
            "id": format!("antigravity:{}", uuid),
            "file": file,
            "source": "antigravity",
            "assistant": "Antigravity",
            "title": cc_title.clone().unwrap_or_else(|| auto_title.clone()),
            "autoTitle": auto_title,
            "tags": cc_tags,
            "summary": Value::Null,
            "sessionId": uuid,
            "cwd": n.cwd.clone(),
            "project": n.cwd.as_deref().map(crate::history::base_name).unwrap_or_default(),
            "gitBranch": Value::Null,
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

    // hand-rolled wire encoding helpers (tests only)
    fn enc_varint(mut v: u64, out: &mut Vec<u8>) {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                break;
            }
            out.push(b | 0x80);
        }
    }
    fn tag(field: u32, wt: u8, out: &mut Vec<u8>) {
        enc_varint(((field as u64) << 3) | wt as u64, out);
    }
    fn put_varint(field: u32, v: u64, out: &mut Vec<u8>) {
        tag(field, 0, out);
        enc_varint(v, out);
    }
    fn put_bytes(field: u32, data: &[u8], out: &mut Vec<u8>) {
        tag(field, 2, out);
        enc_varint(data.len() as u64, out);
        out.extend_from_slice(data);
    }
    fn put_str(field: u32, s: &str, out: &mut Vec<u8>) {
        put_bytes(field, s.as_bytes(), out);
    }
    fn ts_msg(secs: u64) -> Vec<u8> {
        let mut m = vec![];
        put_varint(1, secs, &mut m);
        put_varint(2, 500_000_000, &mut m);
        m
    }

    fn user_step(text: &str) -> Vec<u8> {
        let mut meta5 = vec![];
        put_bytes(1, &ts_msg(1_783_811_237), &mut meta5);
        let mut u19 = vec![];
        put_str(2, text, &mut u19);
        let mut att = vec![];
        put_str(1, "image/png", &mut att);
        put_bytes(2, b"ABC", &mut att);
        put_str(5, "/tmp/x.png", &mut att);
        put_bytes(9, &att, &mut u19);
        let mut step = vec![];
        put_varint(1, 14, &mut step);
        put_varint(4, 3, &mut step);
        put_bytes(5, &meta5, &mut step);
        put_bytes(19, &u19, &mut step);
        step
    }

    fn tool_step() -> Vec<u8> {
        let mut call = vec![];
        put_str(1, "call-9", &mut call);
        put_str(2, "run_command", &mut call);
        put_str(3, "{\"CommandLine\":\"ls -la\",\"Cwd\":\"/tmp\",\"toolSummary\":\"Run\"}", &mut call);
        let mut meta5 = vec![];
        put_bytes(1, &ts_msg(1_783_811_240), &mut meta5);
        put_bytes(4, &call, &mut meta5);
        let mut step = vec![];
        put_varint(1, 21, &mut step);
        put_varint(4, 3, &mut step);
        put_bytes(5, &meta5, &mut step);
        step
    }

    fn gen_step(text: &str) -> Vec<u8> {
        let mut stats = vec![];
        put_varint(1, 1132, &mut stats);
        put_varint(2, 20245, &mut stats);
        put_varint(3, 346, &mut stats);
        let mut meta5 = vec![];
        put_bytes(1, &ts_msg(1_783_811_242), &mut meta5);
        put_bytes(9, &stats, &mut meta5);
        let mut t20 = vec![];
        put_str(1, text, &mut t20);
        let mut step = vec![];
        put_varint(1, 15, &mut step);
        put_varint(4, 3, &mut step);
        put_bytes(5, &meta5, &mut step);
        put_bytes(20, &t20, &mut step);
        step
    }

    #[test]
    fn decodes_steps() {
        let mut n = Norm::default();
        push_step(&mut n, &user_step("修复登录"));
        push_step(&mut n, &tool_step());
        push_step(&mut n, &gen_step("已修复。"));
        assert_eq!(n.messages.len(), 3);
        assert_eq!(n.messages[0]["role"], "user");
        assert_eq!(n.messages[0]["content"][0]["text"], "修复登录");
        assert_eq!(n.messages[0]["content"][1]["type"], "image");
        assert_eq!(n.messages[0]["content"][1]["source"]["data"], "QUJD");
        let tool = &n.messages[1]["content"][0];
        assert_eq!(tool["name"], "Bash");
        assert_eq!(tool["input"]["command"], "ls -la");
        assert_eq!(tool["id"], "call-9");
        assert_eq!(n.messages[2]["content"][0]["text"], "已修复。");
        assert_eq!(n.messages[2]["usage"]["inputTokens"], 20245);
        assert_eq!(n.messages[2]["usage"]["outputTokens"], 346);
        assert_eq!(n.totals["in"], 20245);
        assert_eq!(n.totals["turns"], 1);
        assert!(n.messages[0]["ts"].as_str().unwrap().starts_with("2026-"));
    }

    #[test]
    fn garbage_payload_is_skipped() {
        let mut n = Norm::default();
        push_step(&mut n, &[0xff, 0x00, 0x13, 0x37]);
        push_step(&mut n, b"");
        assert!(n.messages.is_empty());
    }

    #[test]
    fn b64_matches_reference() {
        assert_eq!(b64_encode(b"ABC"), "QUJD");
        assert_eq!(b64_encode(b"AB"), "QUI=");
        assert_eq!(b64_encode(b"A"), "QQ==");
        assert_eq!(b64_encode(b""), "");
    }

    #[test]
    fn detects_paths() {
        assert!(looks_agy_path(Path::new("/x/antigravity-cli/conversations/ab-1.db")));
        assert!(!looks_agy_path(Path::new("/x/antigravity-cli/conversation_summaries.db")));
        assert!(!looks_agy_path(Path::new("/x/conversations/notes.txt")));
    }
}
