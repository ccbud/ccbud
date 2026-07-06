// Incremental SSE transcoders (P2). Consume an upstream provider's streaming events line-by-line
// and emit the client protocol's SSE events as they arrive — true token-by-token streaming, not the
// buffer-then-synthesize first cut. Wired pairs (see `Transcoder`):
//   - OpenAI Chat `chat.completion.chunk` → Anthropic Messages events (Claude Code client)
//   - OpenAI Chat `chat.completion.chunk` → OpenAI Responses events   (Codex client)
//   - Anthropic Messages events           → OpenAI Responses events   (Codex client)

use super::Wire;
use serde_json::{json, Value};

fn ev(event: &str, data: Value) -> String {
    format!("event: {}\ndata: {}\n\n", event, serde_json::to_string(&data).unwrap_or_default())
}

/// Dispatcher over the wired (provider → client) incremental transcoders, so gateway.rs holds one
/// value regardless of the pair. `supports` is the single source of truth behind
/// `protocol::can_transcode_stream`.
pub enum Transcoder {
    ChatToAnthropic(ChatToAnthropic),
    ChatToResponses(ChatToResponses),
    AnthropicToResponses(AnthropicToResponses),
}

impl Transcoder {
    pub fn supports(provider: Wire, client: Wire) -> bool {
        matches!(
            (provider, client),
            (Wire::OpenAiChat, Wire::Anthropic)
                | (Wire::OpenAiChat, Wire::OpenAiResponses)
                | (Wire::Anthropic, Wire::OpenAiResponses)
        )
    }

    pub fn new(provider: Wire, client: Wire, client_model: &str) -> Option<Self> {
        match (provider, client) {
            (Wire::OpenAiChat, Wire::Anthropic) => Some(Self::ChatToAnthropic(ChatToAnthropic::new(client_model))),
            (Wire::OpenAiChat, Wire::OpenAiResponses) => Some(Self::ChatToResponses(ChatToResponses::new(client_model))),
            (Wire::Anthropic, Wire::OpenAiResponses) => {
                Some(Self::AnthropicToResponses(AnthropicToResponses::new(client_model)))
            }
            _ => None,
        }
    }

    pub fn push(&mut self, line: &str) -> String {
        match self {
            Self::ChatToAnthropic(t) => t.push(line),
            Self::ChatToResponses(t) => t.push(line),
            Self::AnthropicToResponses(t) => t.push(line),
        }
    }

    pub fn finish(&mut self) -> String {
        match self {
            Self::ChatToAnthropic(t) => t.finish(),
            Self::ChatToResponses(t) => t.finish(),
            Self::AnthropicToResponses(t) => t.finish(),
        }
    }

    pub fn input_tokens(&self) -> i64 {
        match self {
            Self::ChatToAnthropic(t) => t.input_tokens(),
            Self::ChatToResponses(t) => t.input_tokens(),
            Self::AnthropicToResponses(t) => t.input_tokens(),
        }
    }

    pub fn output_tokens(&self) -> i64 {
        match self {
            Self::ChatToAnthropic(t) => t.output_tokens(),
            Self::ChatToResponses(t) => t.output_tokens(),
            Self::AnthropicToResponses(t) => t.output_tokens(),
        }
    }
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

// ---- shared Responses-side item builders (final `output_item.done` / `completed` payloads) ----

fn resp_message_item(id: &str, text: &str) -> Value {
    json!({ "type": "message", "id": id, "status": "completed", "role": "assistant",
        "content": [{ "type": "output_text", "annotations": [], "text": text }] })
}

fn resp_function_call_item(id: &str, call_id: &str, name: &str, args: &str) -> Value {
    json!({ "type": "function_call", "id": id, "status": "completed", "call_id": call_id,
        "name": name, "arguments": if args.is_empty() { "{}" } else { args } })
}

fn resp_reasoning_item(id: &str, text: &str) -> Value {
    json!({ "type": "reasoning", "id": id, "summary": [{ "type": "summary_text", "text": text }] })
}

/// The terminal `response.completed` event. Codex parses `response.id` + `response.usage` from it
/// and treats a stream that closes without it as an error, so every Responses-emitting transcoder
/// must end with this exactly once.
fn resp_completed(id: &str, model: &str, output: Vec<Value>, input: i64, cached: i64, output_tokens: i64) -> String {
    ev(
        "response.completed",
        json!({ "type": "response.completed", "response": {
            "id": id, "object": "response", "status": "completed", "model": model,
            "output": output,
            "usage": {
                "input_tokens": input.max(0),
                "input_tokens_details": { "cached_tokens": cached.max(0) },
                "output_tokens": output_tokens.max(0),
                "output_tokens_details": { "reasoning_tokens": 0 },
                "total_tokens": (input + output_tokens).max(0),
            } } }),
    )
}

/// Stateful OpenAI-Chat-stream → OpenAI-Responses-stream transcoder (Codex client, chat upstream).
/// Text deltas stream through as `response.output_text.delta`; provider reasoning deltas
/// (`reasoning_content` / `reasoning`) as `response.reasoning_summary_text.delta`; tool-call
/// fragments accumulate per OpenAI index and surface whole in `response.output_item.done` — the
/// only place Codex materializes items from.
pub struct ChatToResponses {
    client_model: String,
    resp_id: String,
    created: bool,
    next_index: usize,
    reasoning: Option<TextItemAcc>,
    reasoning_open: bool,
    message: Option<TextItemAcc>,
    tools: Vec<RespToolAcc>,
    input_tokens: i64,
    output_tokens: i64,
    stopped: bool,
}

struct TextItemAcc {
    index: usize,
    id: String,
    acc: String,
}

struct RespToolAcc {
    oa_index: u64,
    index: usize,
    id: String,
    call_id: String,
    name: String,
    args: String,
}

impl ChatToResponses {
    pub fn new(client_model: &str) -> Self {
        Self {
            client_model: client_model.to_string(),
            resp_id: String::new(),
            created: false,
            next_index: 0,
            reasoning: None,
            reasoning_open: false,
            message: None,
            tools: vec![],
            input_tokens: 0,
            output_tokens: 0,
            stopped: false,
        }
    }

    fn rid(&self) -> String {
        if self.resp_id.is_empty() { "resp_ccbud".to_string() } else { self.resp_id.clone() }
    }

    fn ensure_created(&mut self, out: &mut String) {
        if self.created {
            return;
        }
        self.created = true;
        let id = self.rid();
        out.push_str(&ev(
            "response.created",
            json!({ "type": "response.created",
                "response": { "id": id, "object": "response", "status": "in_progress", "model": self.client_model } }),
        ));
    }

    fn close_reasoning(&mut self, out: &mut String) {
        if !self.reasoning_open {
            return;
        }
        self.reasoning_open = false;
        if let Some(r) = &self.reasoning {
            out.push_str(&ev(
                "response.output_item.done",
                json!({ "type": "response.output_item.done", "output_index": r.index,
                    "item": resp_reasoning_item(&r.id, &r.acc) }),
            ));
        }
    }

    /// Feed one raw upstream SSE line ("data: {...}" or "data: [DONE]"). Returns Responses SSE
    /// text to forward (possibly empty).
    pub fn push(&mut self, line: &str) -> String {
        let mut out = String::new();
        let t = line.trim();
        let payload = match t.strip_prefix("data:") {
            Some(p) => p.trim(),
            None => return out, // chat SSE is data-only; ignore blanks/event: lines
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
        if self.resp_id.is_empty() {
            if let Some(id) = chunk.get("id").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
                self.resp_id = format!("resp_{}", id);
            }
        }
        // usage rides the final chunk (stream_options.include_usage)
        if let Some(u) = chunk.get("usage").filter(|u| !u.is_null()) {
            self.input_tokens = u.get("prompt_tokens").and_then(|v| v.as_i64()).unwrap_or(self.input_tokens);
            self.output_tokens = u.get("completion_tokens").and_then(|v| v.as_i64()).unwrap_or(self.output_tokens);
        }
        let choice = match chunk.get("choices").and_then(|c| c.as_array()).and_then(|a| a.first()) {
            Some(c) => c,
            None => return out,
        };
        self.ensure_created(&mut out);
        let delta = choice.get("delta").cloned().unwrap_or(Value::Null);

        // provider reasoning stream (DeepSeek/GLM-style `reasoning_content`, or `reasoning`)
        let think = delta
            .get("reasoning_content")
            .and_then(|v| v.as_str())
            .or_else(|| delta.get("reasoning").and_then(|v| v.as_str()))
            .unwrap_or("");
        if !think.is_empty() {
            if self.reasoning.is_none() {
                let index = self.next_index;
                self.next_index += 1;
                let id = format!("rs_{}", index);
                out.push_str(&ev(
                    "response.output_item.added",
                    json!({ "type": "response.output_item.added", "output_index": index,
                        "item": { "type": "reasoning", "id": id, "summary": [] } }),
                ));
                self.reasoning = Some(TextItemAcc { index, id, acc: String::new() });
                self.reasoning_open = true;
            }
            let r = self.reasoning.as_mut().unwrap();
            r.acc.push_str(think);
            out.push_str(&ev(
                "response.reasoning_summary_text.delta",
                json!({ "type": "response.reasoning_summary_text.delta", "item_id": r.id,
                    "output_index": r.index, "summary_index": 0, "delta": think }),
            ));
        }

        // text delta
        if let Some(txt) = delta.get("content").and_then(|v| v.as_str()) {
            if !txt.is_empty() {
                self.close_reasoning(&mut out);
                if self.message.is_none() {
                    let index = self.next_index;
                    self.next_index += 1;
                    let id = format!("msg_{}", index);
                    out.push_str(&ev(
                        "response.output_item.added",
                        json!({ "type": "response.output_item.added", "output_index": index,
                            "item": { "type": "message", "id": id, "status": "in_progress", "role": "assistant", "content": [] } }),
                    ));
                    out.push_str(&ev(
                        "response.content_part.added",
                        json!({ "type": "response.content_part.added", "item_id": id, "output_index": index,
                            "content_index": 0, "part": { "type": "output_text", "annotations": [], "text": "" } }),
                    ));
                    self.message = Some(TextItemAcc { index, id, acc: String::new() });
                }
                let m = self.message.as_mut().unwrap();
                m.acc.push_str(txt);
                out.push_str(&ev(
                    "response.output_text.delta",
                    json!({ "type": "response.output_text.delta", "item_id": m.id, "output_index": m.index,
                        "content_index": 0, "delta": txt }),
                ));
            }
        }

        // tool_call deltas (streamed in fragments, keyed by their OpenAI index)
        if let Some(tcs) = delta.get("tool_calls").and_then(|v| v.as_array()) {
            if !tcs.is_empty() {
                self.close_reasoning(&mut out);
            }
            for tc in tcs {
                let oa_index = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                let frag_id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let frag_name = tc.get("function").and_then(|f| f.get("name")).and_then(|v| v.as_str()).unwrap_or("");
                let args = tc.get("function").and_then(|f| f.get("arguments")).and_then(|v| v.as_str()).unwrap_or("");

                let pos = match self.tools.iter().position(|s| s.oa_index == oa_index) {
                    Some(p) => p,
                    None => {
                        let index = self.next_index;
                        self.next_index += 1;
                        let slot = RespToolAcc {
                            oa_index,
                            index,
                            id: format!("fc_{}", index),
                            call_id: if frag_id.is_empty() { format!("call_{}", index) } else { frag_id.to_string() },
                            name: frag_name.to_string(),
                            args: String::new(),
                        };
                        out.push_str(&ev(
                            "response.output_item.added",
                            json!({ "type": "response.output_item.added", "output_index": slot.index,
                                "item": { "type": "function_call", "id": slot.id, "status": "in_progress",
                                    "call_id": slot.call_id, "name": slot.name, "arguments": "" } }),
                        ));
                        self.tools.push(slot);
                        self.tools.len() - 1
                    }
                };
                // stray late fragments may carry the id/name the opener lacked; the done item wins
                if !frag_id.is_empty() {
                    self.tools[pos].call_id = frag_id.to_string();
                }
                if !frag_name.is_empty() && self.tools[pos].name.is_empty() {
                    self.tools[pos].name = frag_name.to_string();
                }
                if !args.is_empty() {
                    let s = &mut self.tools[pos];
                    s.args.push_str(args);
                    out.push_str(&ev(
                        "response.function_call_arguments.delta",
                        json!({ "type": "response.function_call_arguments.delta", "item_id": s.id,
                            "output_index": s.index, "delta": args }),
                    ));
                }
            }
        }
        out
    }

    /// Close open items in index order and emit `response.completed`. Idempotent.
    pub fn finish(&mut self) -> String {
        if self.stopped {
            return String::new();
        }
        self.stopped = true;
        let mut out = String::new();
        self.ensure_created(&mut out);
        self.close_reasoning(&mut out);
        if let Some(m) = &self.message {
            out.push_str(&ev(
                "response.output_text.done",
                json!({ "type": "response.output_text.done", "item_id": m.id, "output_index": m.index,
                    "content_index": 0, "text": m.acc }),
            ));
            out.push_str(&ev(
                "response.content_part.done",
                json!({ "type": "response.content_part.done", "item_id": m.id, "output_index": m.index,
                    "content_index": 0, "part": { "type": "output_text", "annotations": [], "text": m.acc } }),
            ));
            out.push_str(&ev(
                "response.output_item.done",
                json!({ "type": "response.output_item.done", "output_index": m.index,
                    "item": resp_message_item(&m.id, &m.acc) }),
            ));
        }
        for s in &self.tools {
            out.push_str(&ev(
                "response.function_call_arguments.done",
                json!({ "type": "response.function_call_arguments.done", "item_id": s.id,
                    "output_index": s.index, "arguments": s.args }),
            ));
            out.push_str(&ev(
                "response.output_item.done",
                json!({ "type": "response.output_item.done", "output_index": s.index,
                    "item": resp_function_call_item(&s.id, &s.call_id, &s.name, &s.args) }),
            ));
        }
        let mut items: Vec<(usize, Value)> = vec![];
        if let Some(r) = &self.reasoning {
            items.push((r.index, resp_reasoning_item(&r.id, &r.acc)));
        }
        if let Some(m) = &self.message {
            items.push((m.index, resp_message_item(&m.id, &m.acc)));
        }
        for s in &self.tools {
            items.push((s.index, resp_function_call_item(&s.id, &s.call_id, &s.name, &s.args)));
        }
        items.sort_by_key(|(i, _)| *i);
        let output: Vec<Value> = items.into_iter().map(|(_, v)| v).collect();
        out.push_str(&resp_completed(&self.rid(), &self.client_model, output, self.input_tokens, 0, self.output_tokens));
        out
    }

    pub fn input_tokens(&self) -> i64 {
        self.input_tokens
    }
    pub fn output_tokens(&self) -> i64 {
        self.output_tokens
    }
}

/// Stateful Anthropic-Messages-stream → OpenAI-Responses-stream transcoder (Codex client,
/// Anthropic upstream). Anthropic blocks map 1:1 onto Responses output items: text →
/// message/output_text, tool_use → function_call (input_json_delta fragments accumulate into the
/// arguments string), thinking → reasoning summary. Upstream `error` events surface as
/// `response.failed` so Codex aborts cleanly instead of timing out.
pub struct AnthropicToResponses {
    client_model: String,
    resp_id: String,
    created: bool,
    next_index: usize,
    blocks: Vec<ABlock>,
    input_tokens: i64,
    cached_tokens: i64,
    output_tokens: i64,
    stopped: bool,
}

struct ABlock {
    a_index: u64,
    index: usize,
    id: String,
    kind: AKind,
    open: bool,
}

enum AKind {
    Text { acc: String },
    Tool { call_id: String, name: String, args: String },
    Think { acc: String },
}

/// The closing event sequence for one finished block (its `*.done` events + `output_item.done`).
fn close_ablock_events(b: &ABlock) -> String {
    let mut out = String::new();
    match &b.kind {
        AKind::Text { acc } => {
            out.push_str(&ev(
                "response.output_text.done",
                json!({ "type": "response.output_text.done", "item_id": b.id, "output_index": b.index,
                    "content_index": 0, "text": acc }),
            ));
            out.push_str(&ev(
                "response.content_part.done",
                json!({ "type": "response.content_part.done", "item_id": b.id, "output_index": b.index,
                    "content_index": 0, "part": { "type": "output_text", "annotations": [], "text": acc } }),
            ));
            out.push_str(&ev(
                "response.output_item.done",
                json!({ "type": "response.output_item.done", "output_index": b.index, "item": resp_message_item(&b.id, acc) }),
            ));
        }
        AKind::Tool { call_id, name, args } => {
            out.push_str(&ev(
                "response.function_call_arguments.done",
                json!({ "type": "response.function_call_arguments.done", "item_id": b.id,
                    "output_index": b.index, "arguments": args }),
            ));
            out.push_str(&ev(
                "response.output_item.done",
                json!({ "type": "response.output_item.done", "output_index": b.index,
                    "item": resp_function_call_item(&b.id, call_id, name, args) }),
            ));
        }
        AKind::Think { acc } => {
            out.push_str(&ev(
                "response.output_item.done",
                json!({ "type": "response.output_item.done", "output_index": b.index, "item": resp_reasoning_item(&b.id, acc) }),
            ));
        }
    }
    out
}

fn ablock_item(b: &ABlock) -> Value {
    match &b.kind {
        AKind::Text { acc } => resp_message_item(&b.id, acc),
        AKind::Tool { call_id, name, args } => resp_function_call_item(&b.id, call_id, name, args),
        AKind::Think { acc } => resp_reasoning_item(&b.id, acc),
    }
}

impl AnthropicToResponses {
    pub fn new(client_model: &str) -> Self {
        Self {
            client_model: client_model.to_string(),
            resp_id: String::new(),
            created: false,
            next_index: 0,
            blocks: vec![],
            input_tokens: 0,
            cached_tokens: 0,
            output_tokens: 0,
            stopped: false,
        }
    }

    fn rid(&self) -> String {
        if self.resp_id.is_empty() { "resp_ccbud".to_string() } else { self.resp_id.clone() }
    }

    fn ensure_created(&mut self, out: &mut String) {
        if self.created {
            return;
        }
        self.created = true;
        let id = self.rid();
        out.push_str(&ev(
            "response.created",
            json!({ "type": "response.created",
                "response": { "id": id, "object": "response", "status": "in_progress", "model": self.client_model } }),
        ));
    }

    /// Feed one raw upstream SSE line. Anthropic streams interleave `event:` and `data:` lines;
    /// the data JSON's `type` mirrors the event name, so data lines alone drive the state machine.
    pub fn push(&mut self, line: &str) -> String {
        let mut out = String::new();
        let t = line.trim();
        let payload = match t.strip_prefix("data:") {
            Some(p) => p.trim(),
            None => return out,
        };
        if payload.is_empty() {
            return out;
        }
        let evt: Value = match serde_json::from_str(payload) {
            Ok(v) => v,
            Err(_) => return out,
        };
        match evt.get("type").and_then(|v| v.as_str()) {
            Some("message_start") => {
                if let Some(m) = evt.get("message") {
                    if let Some(id) = m.get("id").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
                        self.resp_id = format!("resp_{}", id);
                    }
                    if let Some(u) = m.get("usage") {
                        // Responses-style input_tokens includes cached reads; Anthropic reports
                        // them separately.
                        let base = u.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                        let cr = u.get("cache_read_input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                        let cc = u.get("cache_creation_input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                        self.input_tokens = base + cr + cc;
                        self.cached_tokens = cr;
                    }
                }
                self.ensure_created(&mut out);
            }
            Some("content_block_start") => {
                self.ensure_created(&mut out);
                let a_index = evt.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                let cb = evt.get("content_block").cloned().unwrap_or(Value::Null);
                let index = self.next_index;
                match cb.get("type").and_then(|v| v.as_str()) {
                    Some("text") => {
                        self.next_index += 1;
                        let id = format!("msg_{}", index);
                        out.push_str(&ev(
                            "response.output_item.added",
                            json!({ "type": "response.output_item.added", "output_index": index,
                                "item": { "type": "message", "id": id, "status": "in_progress", "role": "assistant", "content": [] } }),
                        ));
                        out.push_str(&ev(
                            "response.content_part.added",
                            json!({ "type": "response.content_part.added", "item_id": id, "output_index": index,
                                "content_index": 0, "part": { "type": "output_text", "annotations": [], "text": "" } }),
                        ));
                        self.blocks.push(ABlock { a_index, index, id, kind: AKind::Text { acc: String::new() }, open: true });
                    }
                    Some("tool_use") => {
                        self.next_index += 1;
                        let id = format!("fc_{}", index);
                        let call_id = cb
                            .get("id")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| format!("call_{}", index));
                        let name = cb.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        out.push_str(&ev(
                            "response.output_item.added",
                            json!({ "type": "response.output_item.added", "output_index": index,
                                "item": { "type": "function_call", "id": id, "status": "in_progress",
                                    "call_id": call_id, "name": name, "arguments": "" } }),
                        ));
                        self.blocks.push(ABlock { a_index, index, id, kind: AKind::Tool { call_id, name, args: String::new() }, open: true });
                    }
                    Some("thinking") => {
                        self.next_index += 1;
                        let id = format!("rs_{}", index);
                        out.push_str(&ev(
                            "response.output_item.added",
                            json!({ "type": "response.output_item.added", "output_index": index,
                                "item": { "type": "reasoning", "id": id, "summary": [] } }),
                        ));
                        self.blocks.push(ABlock { a_index, index, id, kind: AKind::Think { acc: String::new() }, open: true });
                    }
                    // redacted_thinking / server_tool_use / … have no Responses equivalent; their
                    // deltas find no block below and drop.
                    _ => {}
                }
            }
            Some("content_block_delta") => {
                let a_index = evt.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                let delta = evt.get("delta").cloned().unwrap_or(Value::Null);
                if let Some(b) = self.blocks.iter_mut().find(|b| b.a_index == a_index && b.open) {
                    match (&mut b.kind, delta.get("type").and_then(|v| v.as_str())) {
                        (AKind::Text { acc }, Some("text_delta")) => {
                            if let Some(txt) = delta.get("text").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
                                acc.push_str(txt);
                                out.push_str(&ev(
                                    "response.output_text.delta",
                                    json!({ "type": "response.output_text.delta", "item_id": b.id,
                                        "output_index": b.index, "content_index": 0, "delta": txt }),
                                ));
                            }
                        }
                        (AKind::Tool { args, .. }, Some("input_json_delta")) => {
                            if let Some(pj) = delta.get("partial_json").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
                                args.push_str(pj);
                                out.push_str(&ev(
                                    "response.function_call_arguments.delta",
                                    json!({ "type": "response.function_call_arguments.delta", "item_id": b.id,
                                        "output_index": b.index, "delta": pj }),
                                ));
                            }
                        }
                        (AKind::Think { acc }, Some("thinking_delta")) => {
                            if let Some(th) = delta.get("thinking").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
                                acc.push_str(th);
                                out.push_str(&ev(
                                    "response.reasoning_summary_text.delta",
                                    json!({ "type": "response.reasoning_summary_text.delta", "item_id": b.id,
                                        "output_index": b.index, "summary_index": 0, "delta": th }),
                                ));
                            }
                        }
                        _ => {} // signature_delta etc.
                    }
                }
            }
            Some("content_block_stop") => {
                let a_index = evt.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                if let Some(b) = self.blocks.iter_mut().find(|b| b.a_index == a_index && b.open) {
                    b.open = false;
                    out.push_str(&close_ablock_events(b));
                }
            }
            Some("message_delta") => {
                if let Some(o) = evt.get("usage").and_then(|u| u.get("output_tokens")).and_then(|v| v.as_i64()) {
                    self.output_tokens = o;
                }
            }
            Some("message_stop") => {
                out.push_str(&self.finish());
            }
            Some("error") => {
                let msg = evt
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("upstream error");
                self.ensure_created(&mut out);
                let id = self.rid();
                out.push_str(&ev(
                    "response.failed",
                    json!({ "type": "response.failed", "response": { "id": id, "object": "response", "status": "failed",
                        "error": { "code": "upstream_error", "message": msg } } }),
                ));
                self.stopped = true; // failed is terminal — no completed after it
            }
            _ => {} // ping etc.
        }
        out
    }

    /// Close any still-open blocks and emit `response.completed`. Idempotent (and a no-op after a
    /// terminal `response.failed`).
    pub fn finish(&mut self) -> String {
        if self.stopped {
            return String::new();
        }
        self.stopped = true;
        let mut out = String::new();
        self.ensure_created(&mut out);
        self.blocks.sort_by_key(|b| b.index);
        for b in &mut self.blocks {
            if b.open {
                b.open = false;
                out.push_str(&close_ablock_events(b));
            }
        }
        let output: Vec<Value> = self.blocks.iter().map(ablock_item).collect();
        out.push_str(&resp_completed(
            &self.rid(),
            &self.client_model,
            output,
            self.input_tokens,
            self.cached_tokens,
            self.output_tokens,
        ));
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

    #[test]
    fn chat_to_responses_text_and_split_tool_call() {
        let mut tc = ChatToResponses::new("alias-x");
        let mut out = String::new();
        out.push_str(&tc.push("data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"}}]}"));
        out.push_str(&tc.push("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Let me \"}}]}"));
        out.push_str(&tc.push("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"check.\"}}]}"));
        out.push_str(&tc.push("data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"shell\",\"arguments\":\"{\\\"co\"}}]}}]}"));
        out.push_str(&tc.push("data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"mmand\\\":[\\\"ls\\\"]}\"}}]}}]}"));
        out.push_str(&tc.push("data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":20,\"completion_tokens\":9}}"));
        out.push_str(&tc.push("data: [DONE]"));

        // ordered: created → text deltas → item done events → completed
        let created = out.find(r#""type":"response.created""#).unwrap();
        let first_delta = out.find(r#""type":"response.output_text.delta""#).unwrap();
        let item_done = out.find(r#""type":"response.output_item.done""#).unwrap();
        let completed = out.find(r#""type":"response.completed""#).unwrap();
        assert!(created < first_delta && first_delta < item_done && item_done < completed);
        // token-by-token text deltas
        assert!(out.contains(r#""delta":"Let me ""#));
        assert!(out.contains(r#""delta":"check.""#));
        // Codex materializes items from output_item.done: full text + reassembled arguments
        assert!(out.contains(r#""text":"Let me check.""#));
        assert!(out.contains(r#""call_id":"call_1""#) && out.contains(r#""name":"shell""#));
        assert!(out.contains(r#""arguments":"{\"command\":[\"ls\"]}""#));
        // completed carries id + usage
        assert!(out.contains(r#""id":"resp_chatcmpl-1""#));
        assert!(out.contains(r#""input_tokens":20"#) && out.contains(r#""output_tokens":9"#));
        assert_eq!(tc.input_tokens(), 20);
        assert_eq!(tc.output_tokens(), 9);
        // finish is idempotent — [DONE] already closed the stream
        assert_eq!(tc.finish(), "");
    }

    #[test]
    fn anthropic_to_responses_text_tool_and_thinking() {
        let mut tc = AnthropicToResponses::new("alias-x");
        let mut out = String::new();
        out.push_str(&tc.push(r#"data: {"type":"message_start","message":{"id":"msg_9","usage":{"input_tokens":30,"cache_read_input_tokens":12,"cache_creation_input_tokens":0}}}"#));
        out.push_str(&tc.push(r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#));
        out.push_str(&tc.push(r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}"#));
        out.push_str(&tc.push(r#"data: {"type":"content_block_stop","index":0}"#));
        out.push_str(&tc.push(r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#));
        out.push_str(&tc.push(r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Run"}}"#));
        out.push_str(&tc.push(r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"ning."}}"#));
        out.push_str(&tc.push(r#"data: {"type":"content_block_stop","index":1}"#));
        out.push_str(&tc.push(r#"data: {"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_1","name":"shell","input":{}}}"#));
        out.push_str(&tc.push(r#"data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"q\":"}}"#));
        out.push_str(&tc.push(r#"data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"\"x\"}"}}"#));
        out.push_str(&tc.push(r#"data: {"type":"content_block_stop","index":2}"#));
        out.push_str(&tc.push(r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":11}}"#));
        out.push_str(&tc.push(r#"data: {"type":"message_stop"}"#));

        let created = out.find(r#""type":"response.created""#).unwrap();
        let completed = out.find(r#""type":"response.completed""#).unwrap();
        assert!(created < completed);
        // thinking → reasoning summary deltas + item
        assert!(out.contains(r#""type":"response.reasoning_summary_text.delta""#) && out.contains(r#""delta":"hmm""#));
        assert!(out.contains(r#""type":"summary_text""#));
        // text streams as deltas and closes with the full text
        assert!(out.contains(r#""delta":"Run""#) && out.contains(r#""delta":"ning.""#));
        assert!(out.contains(r#""text":"Running.""#));
        // tool_use → function_call item with the reassembled arguments string
        assert!(out.contains(r#""call_id":"toolu_1""#) && out.contains(r#""name":"shell""#));
        assert!(out.contains(r#""arguments":"{\"q\":\"x\"}""#));
        // usage: cached reads fold into input_tokens, detail carries them
        assert!(out.contains(r#""input_tokens":42"#));
        assert!(out.contains(r#""cached_tokens":12"#));
        assert!(out.contains(r#""output_tokens":11"#));
        assert!(out.contains(r#""id":"resp_msg_9""#));
        assert_eq!(tc.input_tokens(), 42);
        assert_eq!(tc.output_tokens(), 11);
        // message_stop already completed the stream
        assert_eq!(tc.finish(), "");
    }

    #[test]
    fn anthropic_to_responses_error_is_terminal() {
        let mut tc = AnthropicToResponses::new("alias-x");
        let mut out = String::new();
        out.push_str(&tc.push(r#"data: {"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":5}}}"#));
        out.push_str(&tc.push(r#"data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#));
        out.push_str(&tc.finish());
        assert!(out.contains(r#""type":"response.failed""#) && out.contains("Overloaded"));
        // failed is terminal: no completed after it
        assert!(!out.contains(r#""type":"response.completed""#));
    }
}
