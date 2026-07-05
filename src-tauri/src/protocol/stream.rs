// Incremental SSE transcoders (P2). Consume an upstream provider's streaming events line-by-line
// and emit the client protocol's SSE events as they arrive — true token-by-token streaming, not the
// buffer-then-synthesize first cut. Currently: OpenAI Chat `chat.completion.chunk` → Anthropic
// Messages events (`message_start` / `content_block_*` / `message_delta` / `message_stop`).

use serde_json::{json, Value};

fn ev(event: &str, data: Value) -> String {
    format!("event: {}\ndata: {}\n\n", event, serde_json::to_string(&data).unwrap_or_default())
}

fn map_stop(finish: Option<&str>, had_tool: bool) -> &'static str {
    match finish {
        Some("length") => "max_tokens",
        Some("tool_calls") | Some("function_call") => "tool_use",
        _ if had_tool => "tool_use",
        _ => "end_turn",
    }
}

/// Stateful OpenAI-Chat-stream → Anthropic-stream transcoder. Feed each raw upstream SSE line to
/// `push`; call `finish` at end. Anthropic requires an ordered `message_start`, then content blocks
/// (each `content_block_start`/`_delta`/`_stop`), then `message_delta` + `message_stop`. We open a
/// text block on the first text delta and one tool_use block per OpenAI tool_call index, assigning
/// Anthropic block indices in first-appearance order.
pub struct ChatToAnthropic {
    client_model: String,
    started: bool,
    next_index: usize,
    // text block
    text_index: Option<usize>,
    // openai tool_call index → (anthropic block index, open?)
    tools: Vec<ToolSlot>,
    input_tokens: i64,
    output_tokens: i64,
    finish_reason: Option<String>,
    stopped: bool,
}

struct ToolSlot {
    oa_index: u64,
    an_index: usize,
    open: bool,
}

impl ChatToAnthropic {
    pub fn new(client_model: &str) -> Self {
        Self {
            client_model: client_model.to_string(),
            started: false,
            next_index: 0,
            text_index: None,
            tools: vec![],
            input_tokens: 0,
            output_tokens: 0,
            finish_reason: None,
            stopped: false,
        }
    }

    fn ensure_started(&mut self, out: &mut String) {
        if self.started {
            return;
        }
        self.started = true;
        out.push_str(&ev(
            "message_start",
            json!({ "type": "message_start", "message": {
                "id": "msg_ccbud", "type": "message", "role": "assistant", "model": self.client_model,
                "content": [], "stop_reason": Value::Null, "stop_sequence": Value::Null,
                "usage": { "input_tokens": self.input_tokens.max(0), "output_tokens": 0 },
            }}),
        ));
    }

    fn open_text(&mut self, out: &mut String) -> usize {
        if let Some(i) = self.text_index {
            return i;
        }
        let idx = self.next_index;
        self.next_index += 1;
        self.text_index = Some(idx);
        out.push_str(&ev("content_block_start", json!({ "type": "content_block_start", "index": idx, "content_block": { "type": "text", "text": "" } })));
        idx
    }

    /// Feed one raw upstream SSE line (e.g. "data: {...}\n" or "data: [DONE]\n"). Returns the
    /// Anthropic SSE text to forward (possibly empty).
    pub fn push(&mut self, line: &str) -> String {
        let mut out = String::new();
        let t = line.trim();
        let payload = match t.strip_prefix("data:") {
            Some(p) => p.trim(),
            None => return out, // ignore "event:" lines / blanks; chat SSE carries data-only
        };
        if payload.is_empty() {
            return out;
        }
        if payload == "[DONE]" {
            out.push_str(&self.finish());
            return out;
        }
        let chunk: Value = match serde_json::from_str(payload) {
            Ok(v) => v,
            Err(_) => return out,
        };
        // usage may ride the final chunk (stream_options.include_usage)
        if let Some(u) = chunk.get("usage").filter(|u| !u.is_null()) {
            self.input_tokens = u.get("prompt_tokens").and_then(|v| v.as_i64()).unwrap_or(self.input_tokens);
            self.output_tokens = u.get("completion_tokens").and_then(|v| v.as_i64()).unwrap_or(self.output_tokens);
        }
        let choice = chunk.get("choices").and_then(|c| c.as_array()).and_then(|a| a.first());
        let choice = match choice {
            Some(c) => c,
            None => return out,
        };
        self.ensure_started(&mut out);
        let delta = choice.get("delta").cloned().unwrap_or(Value::Null);

        // text delta
        if let Some(txt) = delta.get("content").and_then(|v| v.as_str()) {
            if !txt.is_empty() {
                let idx = self.open_text(&mut out);
                out.push_str(&ev("content_block_delta", json!({ "type": "content_block_delta", "index": idx, "delta": { "type": "text_delta", "text": txt } })));
            }
        }

        // tool_call deltas (streamed in fragments, keyed by their OpenAI index)
        if let Some(tcs) = delta.get("tool_calls").and_then(|v| v.as_array()) {
            for tc in tcs {
                let oa_index = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                let name = tc.get("function").and_then(|f| f.get("name")).and_then(|v| v.as_str());
                let id = tc.get("id").and_then(|v| v.as_str());
                let args = tc.get("function").and_then(|f| f.get("arguments")).and_then(|v| v.as_str()).unwrap_or("");

                // find or create the slot for this OpenAI tool index
                let pos = self.tools.iter().position(|s| s.oa_index == oa_index);
                let slot_idx = match pos {
                    Some(i) => i,
                    None => {
                        let an_index = self.next_index;
                        self.next_index += 1;
                        self.tools.push(ToolSlot { oa_index, an_index, open: false });
                        self.tools.len() - 1
                    }
                };
                // open the block once we know its id+name (first fragment carries them)
                if !self.tools[slot_idx].open {
                    let an_index = self.tools[slot_idx].an_index;
                    out.push_str(&ev("content_block_start", json!({ "type": "content_block_start", "index": an_index, "content_block": {
                        "type": "tool_use", "id": id.unwrap_or(""), "name": name.unwrap_or(""), "input": {} } })));
                    self.tools[slot_idx].open = true;
                }
                if !args.is_empty() {
                    let an_index = self.tools[slot_idx].an_index;
                    out.push_str(&ev("content_block_delta", json!({ "type": "content_block_delta", "index": an_index, "delta": { "type": "input_json_delta", "partial_json": args } })));
                }
            }
        }

        if let Some(fr) = choice.get("finish_reason").and_then(|v| v.as_str()) {
            self.finish_reason = Some(fr.to_string());
        }
        out
    }

    /// Close any open blocks and emit message_delta + message_stop. Idempotent.
    pub fn finish(&mut self) -> String {
        if self.stopped {
            return String::new();
        }
        self.stopped = true;
        let mut out = String::new();
        self.ensure_started(&mut out);
        // close blocks in ascending Anthropic index order
        let mut closes: Vec<usize> = vec![];
        if let Some(i) = self.text_index {
            closes.push(i);
        }
        for s in &self.tools {
            if s.open {
                closes.push(s.an_index);
            }
        }
        closes.sort_unstable();
        for i in closes {
            out.push_str(&ev("content_block_stop", json!({ "type": "content_block_stop", "index": i })));
        }
        let had_tool = self.tools.iter().any(|s| s.open);
        out.push_str(&ev(
            "message_delta",
            json!({ "type": "message_delta",
                "delta": { "stop_reason": map_stop(self.finish_reason.as_deref(), had_tool), "stop_sequence": Value::Null },
                "usage": { "output_tokens": self.output_tokens.max(0) } }),
        ));
        out.push_str(&ev("message_stop", json!({ "type": "message_stop" })));
        out
    }

    pub fn input_tokens(&self) -> i64 {
        self.input_tokens
    }
    pub fn output_tokens(&self) -> i64 {
        self.output_tokens
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcodes_text_and_split_tool_call() {
        let mut tc = ChatToAnthropic::new("claude-x");
        let mut out = String::new();
        // role primer, then text, then a tool call split across two chunks, then finish + usage
        out.push_str(&tc.push("data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"}}]}"));
        out.push_str(&tc.push("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Let me \"}}]}"));
        out.push_str(&tc.push("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"check.\"}}]}"));
        out.push_str(&tc.push("data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"pa\"}}]}}]}"));
        out.push_str(&tc.push("data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"th\\\":\\\"a.txt\\\"}\"}}]}}]}"));
        out.push_str(&tc.push("data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":20,\"completion_tokens\":9}}"));
        out.push_str(&tc.push("data: [DONE]"));

        // ordered events present (serde_json sorts object keys, so assert on substrings, not key order)
        assert!(out.contains("event: message_start"));
        assert!(out.find("event: message_start").unwrap() < out.find("event: content_block_start").unwrap());
        assert!(out.find("event: content_block_start").unwrap() < out.find("event: message_delta").unwrap());
        assert!(out.find("event: message_delta").unwrap() < out.find("event: message_stop").unwrap());
        // text block: a text content_block_start + its two text deltas
        assert!(out.contains(r#""type":"text""#));
        assert!(out.contains("text_delta") && out.contains(r#""text":"Let me ""#));
        assert!(out.contains(r#""text":"check.""#));
        // tool block: tool_use start carries id+name; args reassembled across fragments
        assert!(out.contains(r#""type":"tool_use""#));
        assert!(out.contains(r#""id":"call_1""#) && out.contains(r#""name":"read_file""#));
        assert!(out.contains("input_json_delta") && out.contains(r#""partial_json":"{\"pa""#));
        assert!(out.contains(r#""partial_json":"th\":\"a.txt\"}""#));
        // closes both blocks (index 0 text, index 1 tool), tool_use stop, usage, terminal stop
        assert!(out.contains(r#""index":0,"type":"content_block_stop""#));
        assert!(out.contains(r#""index":1,"type":"content_block_stop""#));
        assert!(out.contains(r#""stop_reason":"tool_use""#));
        assert!(out.contains(r#""output_tokens":9"#));
        assert!(out.contains("event: message_stop"));
        assert_eq!(tc.input_tokens(), 20);
    }

    #[test]
    fn plain_text_only() {
        let mut tc = ChatToAnthropic::new("claude-x");
        let mut out = String::new();
        out.push_str(&tc.push("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello\"}}]}"));
        out.push_str(&tc.push("data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}"));
        out.push_str(&tc.push("data: [DONE]"));
        assert!(out.contains("text_delta") && out.contains(r#""text":"hello""#));
        assert!(out.contains(r#""stop_reason":"end_turn""#));
        assert!(out.contains("event: message_stop"));
    }
}
