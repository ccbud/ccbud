// Local token estimator for the POST /v1/messages/count_tokens fallback — Rust port of
// countTokens.js. o200k_base tokenizer (closest public vocab to Claude 3/4) for text, plus a
// calibrated structural overhead for message framing / system / tools. Rounds up a touch so a
// slight over-count makes Claude Code compact early rather than overflow the real upstream limit.

#![allow(dead_code)]

use serde_json::Value;
use std::sync::OnceLock;

const BASE: i64 = 5; // per-request framing
const PER_MSG: i64 = 4; // per-message wrapper
const SYS: i64 = 4; // system-prompt framing
const TOOLS: i64 = 15; // fixed tools→system injection framing (not per-tool)
const IMAGE: i64 = 1600; // images are size-priced; flat conservative estimate
const SAFETY: f64 = 1.06; // round a little high, never under-count

fn enc() -> Option<&'static tiktoken_rs::CoreBPE> {
    static E: OnceLock<Option<tiktoken_rs::CoreBPE>> = OnceLock::new();
    E.get_or_init(|| tiktoken_rs::o200k_base().ok()).as_ref()
}

fn count(s: &str) -> i64 {
    if s.is_empty() {
        return 0;
    }
    match enc() {
        Some(e) => e.encode_ordinary(s).len() as i64,
        None => ((s.len() as f64) / 4.0).ceil() as i64, // crude fallback if tokenizer missing
    }
}

fn safe_json(v: &Value) -> String {
    if v.is_null() {
        String::new()
    } else {
        serde_json::to_string(v).unwrap_or_default()
    }
}

fn str_at<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("")
}

/// Estimate input_tokens for an Anthropic Messages request body. Mirrors what count_tokens
/// charges: system + every message's text/tool_use/tool_result + tool definitions + overhead.
pub fn estimate_input_tokens(body: &Value) -> i64 {
    let mut t = 0i64;

    match body.get("system") {
        Some(Value::String(s)) => t += count(s),
        Some(Value::Array(arr)) => {
            for b in arr {
                if b.get("type").and_then(|x| x.as_str()) == Some("text") {
                    t += count(str_at(b, "text"));
                }
            }
        }
        _ => {}
    }
    let has_sys = body.get("system").map(|v| !v.is_null()).unwrap_or(false);

    let msgs = body.get("messages").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    for m in &msgs {
        match m.get("content") {
            Some(Value::String(s)) => t += count(s),
            Some(Value::Array(arr)) => {
                for b in arr {
                    match b.get("type").and_then(|x| x.as_str()) {
                        Some("text") => t += count(str_at(b, "text")),
                        Some("tool_use") => {
                            t += count(str_at(b, "name")) + count(&safe_json(b.get("input").unwrap_or(&Value::Null)))
                        }
                        Some("tool_result") => match b.get("content") {
                            Some(Value::String(s)) => t += count(s),
                            Some(Value::Array(ca)) => {
                                for x in ca {
                                    if x.get("type").and_then(|y| y.as_str()) == Some("text") {
                                        t += count(str_at(x, "text"));
                                    }
                                }
                            }
                            _ => {}
                        },
                        Some("image") => t += IMAGE,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    let tools = body.get("tools").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    for tool in &tools {
        t += count(str_at(tool, "name"))
            + count(str_at(tool, "description"))
            + count(&safe_json(tool.get("input_schema").unwrap_or(&Value::Null)));
    }

    let overhead = BASE
        + PER_MSG * msgs.len() as i64
        + if has_sys { SYS } else { 0 }
        + if !tools.is_empty() { TOOLS } else { 0 };
    std::cmp::max(1, ((t + overhead) as f64 * SAFETY).ceil() as i64)
}

/// Is the real tokenizer loaded (vs the crude char fallback)? For diagnostics.
pub fn tokenizer_ready() -> bool {
    enc().is_some()
}
