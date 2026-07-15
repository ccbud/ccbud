// Incremental SSE transcoders (P2). Consume an upstream provider's streaming events line-by-line
// and emit the client protocol's SSE events as they arrive — true token-by-token streaming, not the
// buffer-then-synthesize first cut. Wired pairs (see `Transcoder`):
//   - OpenAI Chat `chat.completion.chunk` → Anthropic Messages events (Claude Code client)
//   - OpenAI Chat `chat.completion.chunk` → OpenAI Responses events   (Codex client)
//   - Anthropic Messages events           → OpenAI Responses events   (Codex client)

use super::openai_responses::{
    custom_tool_input_from_chat_arguments, response_scoped_call_id, CodexToolContext, CodexToolKind,
};
use super::Wire;
use serde_json::{json, Value};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedToolCall {
    pub call_id: String,
    pub name: String,
    pub arguments: String,
    pub thought_signature: Option<String>,
}

fn ev(event: &str, data: Value) -> String {
    format!(
        "event: {}\ndata: {}\n\n",
        event,
        serde_json::to_string(&data).unwrap_or_default()
    )
}

fn upstream_error_message(event: &Value) -> Option<&str> {
    let error = event.get("error").filter(|value| !value.is_null());
    let is_error = error.is_some() || event.get("type").and_then(Value::as_str) == Some("error");
    if !is_error {
        return None;
    }
    error
        .and_then(|value| value.get("message").and_then(Value::as_str))
        .or_else(|| error.and_then(Value::as_str))
        .or_else(|| event.get("message").and_then(Value::as_str))
        .or(Some("upstream error"))
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
        Self::new_with_context(provider, client, client_model, CodexToolContext::default())
    }

    pub fn new_with_context(
        provider: Wire,
        client: Wire,
        client_model: &str,
        tool_context: CodexToolContext,
    ) -> Option<Self> {
        match (provider, client) {
            (Wire::OpenAiChat, Wire::Anthropic) => {
                Some(Self::ChatToAnthropic(ChatToAnthropic::new(client_model)))
            }
            (Wire::OpenAiChat, Wire::OpenAiResponses) => Some(Self::ChatToResponses(
                ChatToResponses::new_with_context(client_model, tool_context),
            )),
            (Wire::Anthropic, Wire::OpenAiResponses) => Some(Self::AnthropicToResponses(
                AnthropicToResponses::new_with_context(client_model, tool_context),
            )),
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

    /// Terminate a translated stream without allowing EOF finalization to synthesize success.
    pub fn fail(&mut self, message: &str) -> String {
        match self {
            Self::ChatToAnthropic(t) => t.fail(message),
            Self::ChatToResponses(t) => t.fail(message),
            Self::AnthropicToResponses(t) => t.fail(message),
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

    pub fn captured_tool_calls(&self) -> Vec<CapturedToolCall> {
        match self {
            Self::ChatToAnthropic(t) => t.captured_tool_calls(),
            Self::ChatToResponses(t) => t.captured_tool_calls(),
            _ => vec![],
        }
    }

    /// True once the terminal client event (`message_stop` / `response.completed` /
    /// `response.incomplete` / `response.failed`) has been emitted: the turn is semantically
    /// complete even though the upstream socket may not have hit EOF yet — Responses clients
    /// (Codex) hang up exactly at this point, so the gateway must not treat that disconnect as an
    /// abort.
    pub fn done(&self) -> bool {
        match self {
            Self::ChatToAnthropic(t) => t.stopped,
            Self::ChatToResponses(t) => t.stopped,
            Self::AnthropicToResponses(t) => t.stopped,
        }
    }

    pub fn succeeded(&self) -> bool {
        match self {
            Self::ChatToAnthropic(t) => t.stopped && !t.failed,
            Self::ChatToResponses(t) => t.stopped && !t.failed,
            Self::AnthropicToResponses(t) => t.stopped && !t.failed,
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
    // message id sent in message_start — from the upstream chunk id when it has one, else a
    // generated unique id. Clients persist this id; it must never repeat across turns (usage
    // analytics de-dupes assistant messages by id).
    msg_id: Option<String>,
    next_index: usize,
    // text block
    text_index: Option<usize>,
    // openai tool_call index → (anthropic block index, open?)
    tools: Vec<ToolSlot>,
    input_tokens: i64,
    output_tokens: i64,
    finish_reason: Option<String>,
    stopped: bool,
    failed: bool,
}

struct ToolSlot {
    oa_index: u64,
    an_index: usize,
    open: bool,
    id: String,
    name: String,
    thought_signature: Option<String>,
    arguments: String,
}

impl ChatToAnthropic {
    pub fn new(client_model: &str) -> Self {
        Self {
            client_model: client_model.to_string(),
            started: false,
            msg_id: None,
            next_index: 0,
            text_index: None,
            tools: vec![],
            input_tokens: 0,
            output_tokens: 0,
            finish_reason: None,
            stopped: false,
            failed: false,
        }
    }

    fn ensure_started(&mut self, out: &mut String) {
        if self.started {
            return;
        }
        self.started = true;
        let id = self
            .msg_id
            .get_or_insert_with(|| super::uid("msg_ccbud"))
            .clone();
        out.push_str(&ev(
            "message_start",
            json!({ "type": "message_start", "message": {
                "id": id, "type": "message", "role": "assistant", "model": self.client_model,
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

    fn captured_tool_calls(&self) -> Vec<CapturedToolCall> {
        self.tools
            .iter()
            .map(|slot| CapturedToolCall {
                call_id: slot.id.clone(),
                name: slot.name.clone(),
                arguments: slot.arguments.clone(),
                thought_signature: slot.thought_signature.clone(),
            })
            .collect()
    }

    /// Feed one raw upstream SSE line (e.g. "data: {...}\n" or "data: [DONE]\n"). Returns the
    /// Anthropic SSE text to forward (possibly empty).
    pub fn push(&mut self, line: &str) -> String {
        let mut out = String::new();
        if self.stopped {
            return out;
        }
        let t = line.trim();
        let payload = match t.strip_prefix("data:") {
            Some(p) => p.trim(),
            None => return out, // ignore "event:" lines / blanks; chat SSE carries data-only
        };
        if payload.is_empty() {
            return out;
        }
        if payload == "[DONE]" {
            out.push_str(&self.complete());
            return out;
        }
        let chunk: Value = match serde_json::from_str(payload) {
            Ok(v) => v,
            Err(_) => return out,
        };
        if let Some(message) = upstream_error_message(&chunk) {
            return self.fail(message);
        }
        if self.msg_id.is_none() {
            if let Some(id) = chunk
                .get("id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                self.msg_id = Some(format!("msg_{}", id));
            }
        }
        // usage may ride the final chunk (stream_options.include_usage)
        if let Some(u) = chunk.get("usage").filter(|u| !u.is_null()) {
            self.input_tokens = u
                .get("prompt_tokens")
                .and_then(|v| v.as_i64())
                .unwrap_or(self.input_tokens);
            self.output_tokens = u
                .get("completion_tokens")
                .and_then(|v| v.as_i64())
                .unwrap_or(self.output_tokens);
        }
        let choice = chunk
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first());
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
            // A no-index Gemini chunk can contain multiple parallel calls. Even if a provider
            // repeats the same id, each array item in this delta must claim a distinct slot.
            let mut claimed_slots: Vec<usize> = vec![];
            for (fallback_index, tc) in tcs.iter().enumerate() {
                let explicit_index = tc.get("index").and_then(|v| v.as_u64());
                let oa_index = explicit_index.unwrap_or(fallback_index as u64);
                let name = tc
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let args = tc
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let thought_signature = super::json_thought_signature(tc);

                // Standard OpenAI chunks carry `index`; Gemini-compatible streams may omit it.
                // In that case prefer the stable call id, then the call's position in this delta.
                let pos = explicit_index
                    .and_then(|_| {
                        self.tools
                            .iter()
                            .enumerate()
                            .find(|(index, slot)| {
                                slot.oa_index == oa_index && !claimed_slots.contains(index)
                            })
                            .map(|(index, _)| index)
                    })
                    .or_else(|| {
                        (!id.is_empty())
                            .then(|| {
                                self.tools
                                    .iter()
                                    .enumerate()
                                    .find(|(index, slot)| {
                                        slot.id == id && !claimed_slots.contains(index)
                                    })
                                    .map(|(index, _)| index)
                            })
                            .flatten()
                    })
                    .or_else(|| {
                        self.tools
                            .iter()
                            .enumerate()
                            .find(|(index, slot)| {
                                slot.oa_index == oa_index && !claimed_slots.contains(index)
                            })
                            .map(|(index, _)| index)
                    });
                let slot_idx = match pos {
                    Some(i) => i,
                    None => {
                        let an_index = self.next_index;
                        self.next_index += 1;
                        self.tools.push(ToolSlot {
                            oa_index,
                            an_index,
                            open: false,
                            id: String::new(),
                            name: String::new(),
                            thought_signature: None,
                            arguments: String::new(),
                        });
                        self.tools.len() - 1
                    }
                };
                claimed_slots.push(slot_idx);
                let (an_index, should_open, open_id, open_name) = {
                    let slot = &mut self.tools[slot_idx];
                    if !id.is_empty() {
                        slot.id = id.to_string();
                    }
                    if !name.is_empty() {
                        slot.name = name.to_string();
                    }
                    if thought_signature.is_some() {
                        slot.thought_signature = thought_signature;
                    }
                    if !args.is_empty() {
                        slot.arguments.push_str(args);
                    }
                    let should_open = !slot.open;
                    if should_open {
                        slot.open = true;
                    }
                    (
                        slot.an_index,
                        should_open,
                        slot.id.clone(),
                        slot.name.clone(),
                    )
                };
                if should_open {
                    out.push_str(&ev(
                        "content_block_start",
                        json!({ "type": "content_block_start",
                        "index": an_index, "content_block": { "type": "tool_use",
                            "id": open_id, "name": open_name, "input": {} } }),
                    ));
                }
                if !args.is_empty() {
                    out.push_str(&ev("content_block_delta", json!({ "type": "content_block_delta",
                        "index": an_index, "delta": { "type": "input_json_delta", "partial_json": args } })));
                }
            }
        }

        if let Some(fr) = choice.get("finish_reason").and_then(|v| v.as_str()) {
            self.finish_reason = Some(fr.to_string());
        }
        out
    }

    /// Close any open blocks and emit message_delta + message_stop. Idempotent.
    fn complete(&mut self) -> String {
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
            out.push_str(&ev(
                "content_block_stop",
                json!({ "type": "content_block_stop", "index": i }),
            ));
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

    /// Finalize a clean upstream EOF only when a Chat finish reason was observed. `[DONE]` calls
    /// `complete` directly; an EOF without either signal is a truncated stream.
    pub fn finish(&mut self) -> String {
        if self.stopped {
            return String::new();
        }
        if self.finish_reason.is_some() {
            self.complete()
        } else {
            self.fail("upstream stream ended before [DONE] or a finish reason")
        }
    }

    fn fail(&mut self, message: &str) -> String {
        if self.stopped {
            return String::new();
        }
        self.stopped = true;
        self.failed = true;
        ev(
            "error",
            json!({ "type": "error", "error": { "type": "api_error", "message": message } }),
        )
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

fn resp_in_progress_tool_item(
    context: &CodexToolContext,
    id: &str,
    call_id: &str,
    name: &str,
    reasoning: Option<&str>,
) -> Value {
    let mut item =
        context.response_tool_item_with_reasoning(id, "in_progress", call_id, name, "", reasoning);
    if item.get("type").and_then(Value::as_str) == Some("function_call") {
        item["arguments"] = json!("");
    }
    item
}

fn normalized_tool_arguments(arguments: &str) -> &str {
    if arguments.trim().is_empty() {
        "{}"
    } else {
        arguments
    }
}

fn resp_reasoning_item(id: &str, text: &str) -> Value {
    json!({ "type": "reasoning", "id": id, "summary": [{ "type": "summary_text", "text": text }] })
}

fn response_scoped_item_id(prefix: &str, response_id: &str, index: usize) -> String {
    format!(
        "{}_{}_{}",
        prefix,
        response_id.trim_start_matches("resp_"),
        index
    )
}

/// The terminal `response.completed` event. Codex parses `response.id` + `response.usage` from it
/// and treats a stream that closes without it as an error, so every Responses-emitting transcoder
/// must end with this exactly once.
fn resp_completed(
    id: &str,
    model: &str,
    output: Vec<Value>,
    input: i64,
    cached: i64,
    output_tokens: i64,
) -> String {
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

fn resp_incomplete(
    id: &str,
    model: &str,
    output: Vec<Value>,
    input: i64,
    cached: i64,
    output_tokens: i64,
    reason: &str,
) -> String {
    ev(
        "response.incomplete",
        json!({ "type": "response.incomplete", "response": {
            "id": id, "object": "response", "status": "incomplete", "model": model,
            "output": output,
            "incomplete_details": { "reason": reason },
            "usage": {
                "input_tokens": input.max(0),
                "input_tokens_details": { "cached_tokens": cached.max(0) },
                "output_tokens": output_tokens.max(0),
                "output_tokens_details": { "reasoning_tokens": 0 },
                "total_tokens": (input + output_tokens).max(0),
            } } }),
    )
}

fn incomplete_reason(stop_reason: Option<&str>) -> Option<&'static str> {
    match stop_reason {
        Some("length" | "max_tokens" | "model_context_window_exceeded") => {
            Some("max_output_tokens")
        }
        Some("content_filter") => Some("content_filter"),
        _ => None,
    }
}

fn resp_failed(id: &str, message: &str) -> String {
    ev(
        "response.failed",
        json!({ "type": "response.failed", "response": {
            "id": id, "object": "response", "status": "failed",
            "error": { "code": "upstream_error", "message": message }
        } }),
    )
}

/// Stateful OpenAI-Chat-stream → OpenAI-Responses-stream transcoder (Codex client, chat upstream).
/// Text deltas stream through as `response.output_text.delta`; provider reasoning deltas
/// (`reasoning_content` / `reasoning`) as `response.reasoning_summary_text.delta`; tool-call
/// fragments accumulate per OpenAI index (with the same no-index Gemini slot handling as
/// ChatToAnthropic, including thought-signature capture) and surface whole in
/// `response.output_item.done` — the only place Codex materializes items from.
pub struct ChatToResponses {
    client_model: String,
    tool_context: CodexToolContext,
    // Construction-time fallback. An upstream id may replace it only until response.created is
    // emitted; afterward this id is immutable so every event in the response agrees.
    resp_id: String,
    created: bool,
    next_index: usize,
    reasoning: Option<TextItemAcc>,
    reasoning_open: bool,
    message: Option<TextItemAcc>,
    tools: Vec<RespToolAcc>,
    input_tokens: i64,
    cached_tokens: i64,
    output_tokens: i64,
    finish_reason: Option<String>,
    stopped: bool,
    failed: bool,
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
    upstream_call_id: String,
    call_id: String,
    name: String,
    args: String,
    thought_signature: Option<String>,
    announced: bool,
    emitted_args_len: usize,
}

impl ChatToResponses {
    pub fn new(client_model: &str) -> Self {
        Self::new_with_context(client_model, CodexToolContext::default())
    }

    pub fn new_with_context(client_model: &str, tool_context: CodexToolContext) -> Self {
        Self {
            client_model: client_model.to_string(),
            tool_context,
            resp_id: super::uid("resp_ccbud"),
            created: false,
            next_index: 0,
            reasoning: None,
            reasoning_open: false,
            message: None,
            tools: vec![],
            input_tokens: 0,
            cached_tokens: 0,
            output_tokens: 0,
            finish_reason: None,
            stopped: false,
            failed: false,
        }
    }

    fn rid(&self) -> String {
        self.resp_id.clone()
    }

    /// The turn's tool calls (with any Gemini thought signatures sniffed from the chat stream),
    /// keyed by the call_id the Responses client will echo back — feeds the gateway's
    /// session-scoped signature cache exactly like ChatToAnthropic. Nameless slots are excluded,
    /// matching what finish() emits (and therefore what the client can echo).
    pub fn captured_tool_calls(&self) -> Vec<CapturedToolCall> {
        self.tools
            .iter()
            .filter(|slot| !slot.name.is_empty())
            .map(|slot| CapturedToolCall {
                call_id: slot.call_id.clone(),
                name: slot.name.clone(),
                arguments: slot.args.clone(),
                thought_signature: slot.thought_signature.clone(),
            })
            .collect()
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

    fn announce_tool_if_ready(&mut self, pos: usize, out: &mut String) {
        let Some(slot) = self.tools.get(pos) else {
            return;
        };
        if slot.announced || slot.name.is_empty() {
            return;
        }

        let id = self
            .tool_context
            .response_item_id(&slot.name, &self.rid(), slot.index);
        let item = resp_in_progress_tool_item(
            &self.tool_context,
            &id,
            &slot.call_id,
            &slot.name,
            self.reasoning
                .as_ref()
                .map(|reasoning| reasoning.acc.as_str()),
        );
        let index = slot.index;
        out.push_str(&ev(
            "response.output_item.added",
            json!({ "type": "response.output_item.added", "output_index": index, "item": item }),
        ));

        let slot = &mut self.tools[pos];
        slot.id = id;
        slot.announced = true;
        self.emit_pending_tool_arguments(pos, out);
    }

    fn emit_pending_tool_arguments(&mut self, pos: usize, out: &mut String) {
        let Some(slot) = self.tools.get_mut(pos) else {
            return;
        };
        if !slot.announced
            || slot.name.is_empty()
            || self.tool_context.kind_for_chat_name(&slot.name) == CodexToolKind::Custom
            || slot.emitted_args_len >= slot.args.len()
        {
            return;
        }
        let delta = slot.args[slot.emitted_args_len..].to_string();
        slot.emitted_args_len = slot.args.len();
        out.push_str(&ev(
            "response.function_call_arguments.delta",
            json!({ "type": "response.function_call_arguments.delta", "item_id": slot.id,
                "output_index": slot.index, "delta": delta }),
        ));
    }

    fn close_tool_events(&self, slot: &RespToolAcc) -> String {
        let mut out = String::new();
        let arguments = normalized_tool_arguments(&slot.args);
        let item = self.tool_context.response_tool_item_with_reasoning(
            &slot.id,
            "completed",
            &slot.call_id,
            &slot.name,
            arguments,
            self.reasoning
                .as_ref()
                .map(|reasoning| reasoning.acc.as_str()),
        );
        match self.tool_context.kind_for_chat_name(&slot.name) {
            CodexToolKind::Custom => {
                let input = custom_tool_input_from_chat_arguments(arguments);
                if !input.is_empty() {
                    out.push_str(&ev(
                        "response.custom_tool_call_input.delta",
                        json!({ "type": "response.custom_tool_call_input.delta", "item_id": slot.id,
                            "call_id": slot.call_id, "output_index": slot.index, "delta": input }),
                    ));
                }
                out.push_str(&ev(
                    "response.custom_tool_call_input.done",
                    json!({ "type": "response.custom_tool_call_input.done", "item_id": slot.id,
                        "call_id": slot.call_id, "output_index": slot.index, "input": input }),
                ));
            }
            CodexToolKind::Function | CodexToolKind::Namespace | CodexToolKind::ToolSearch => {
                out.push_str(&ev(
                    "response.function_call_arguments.done",
                    json!({ "type": "response.function_call_arguments.done", "item_id": slot.id,
                        "output_index": slot.index, "arguments": arguments }),
                ));
            }
        }
        out.push_str(&ev(
            "response.output_item.done",
            json!({ "type": "response.output_item.done", "output_index": slot.index, "item": item }),
        ));
        out
    }

    /// Feed one raw upstream SSE line ("data: {...}" or "data: [DONE]"). Returns Responses SSE
    /// text to forward (possibly empty).
    pub fn push(&mut self, line: &str) -> String {
        let mut out = String::new();
        if self.stopped {
            return out;
        }
        let t = line.trim();
        let payload = match t.strip_prefix("data:") {
            Some(p) => p.trim(),
            None => return out, // chat SSE is data-only; ignore blanks/event: lines
        };
        if payload.is_empty() {
            return out;
        }
        if payload == "[DONE]" {
            out.push_str(&self.complete());
            return out;
        }
        let chunk: Value = match serde_json::from_str(payload) {
            Ok(v) => v,
            Err(_) => return out,
        };
        if let Some(message) = upstream_error_message(&chunk) {
            return self.fail(message);
        }
        if !self.created {
            if let Some(id) = chunk
                .get("id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                self.resp_id = format!("resp_{}", id);
            }
        }
        // usage rides the final chunk (stream_options.include_usage)
        if let Some(u) = chunk.get("usage").filter(|u| !u.is_null()) {
            self.input_tokens = u
                .get("prompt_tokens")
                .and_then(|v| v.as_i64())
                .unwrap_or(self.input_tokens);
            self.output_tokens = u
                .get("completion_tokens")
                .and_then(|v| v.as_i64())
                .unwrap_or(self.output_tokens);
            if let Some(c) = u
                .pointer("/prompt_tokens_details/cached_tokens")
                .and_then(|v| v.as_i64())
            {
                self.cached_tokens = c;
            }
        }
        let choice = match chunk
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
        {
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
                let id = response_scoped_item_id("rs", &self.rid(), index);
                out.push_str(&ev(
                    "response.output_item.added",
                    json!({ "type": "response.output_item.added", "output_index": index,
                        "item": { "type": "reasoning", "id": id, "summary": [] } }),
                ));
                self.reasoning = Some(TextItemAcc {
                    index,
                    id,
                    acc: String::new(),
                });
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
                    let id = response_scoped_item_id("msg", &self.rid(), index);
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
                    self.message = Some(TextItemAcc {
                        index,
                        id,
                        acc: String::new(),
                    });
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
            // A no-index Gemini chunk can contain multiple parallel calls. Even if a provider
            // repeats the same id, each array item in this delta must claim a distinct slot
            // (mirrors ChatToAnthropic).
            let mut claimed_slots: Vec<usize> = vec![];
            for (fallback_index, tc) in tcs.iter().enumerate() {
                let explicit_index = tc.get("index").and_then(|v| v.as_u64());
                let oa_index = explicit_index.unwrap_or(fallback_index as u64);
                let frag_id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let frag_name = tc
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let args = tc
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let thought_signature = super::json_thought_signature(tc);

                // Standard OpenAI chunks carry `index`; Gemini-compatible streams may omit it.
                // In that case prefer the stable call id, then the call's position in this delta.
                let pos = explicit_index
                    .and_then(|_| {
                        self.tools
                            .iter()
                            .enumerate()
                            .find(|(index, slot)| {
                                slot.oa_index == oa_index && !claimed_slots.contains(index)
                            })
                            .map(|(index, _)| index)
                    })
                    .or_else(|| {
                        (!frag_id.is_empty())
                            .then(|| {
                                self.tools
                                    .iter()
                                    .enumerate()
                                    .find(|(index, slot)| {
                                        slot.upstream_call_id == frag_id
                                            && !claimed_slots.contains(index)
                                    })
                                    .map(|(index, _)| index)
                            })
                            .flatten()
                    })
                    .or_else(|| {
                        self.tools
                            .iter()
                            .enumerate()
                            .find(|(index, slot)| {
                                slot.oa_index == oa_index && !claimed_slots.contains(index)
                            })
                            .map(|(index, _)| index)
                    });
                let pos = match pos {
                    Some(p) => p,
                    None => {
                        let index = self.next_index;
                        self.next_index += 1;
                        let call_id = response_scoped_call_id(&self.rid(), index);
                        let slot = RespToolAcc {
                            oa_index,
                            index,
                            id: String::new(),
                            upstream_call_id: frag_id.to_string(),
                            call_id,
                            name: frag_name.to_string(),
                            args: String::new(),
                            thought_signature: None,
                            announced: false,
                            emitted_args_len: 0,
                        };
                        self.tools.push(slot);
                        self.tools.len() - 1
                    }
                };
                claimed_slots.push(pos);
                // stray late fragments may carry the id/name the opener lacked; the done item wins
                if !frag_id.is_empty() {
                    self.tools[pos].upstream_call_id = frag_id.to_string();
                }
                if !frag_name.is_empty() && self.tools[pos].name.is_empty() {
                    self.tools[pos].name = frag_name.to_string();
                }
                if thought_signature.is_some() {
                    self.tools[pos].thought_signature = thought_signature;
                }
                if !args.is_empty() {
                    self.tools[pos].args.push_str(args);
                }
                self.announce_tool_if_ready(pos, &mut out);
                self.emit_pending_tool_arguments(pos, &mut out);
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(reason.to_string());
        }
        out
    }

    /// Close open items in index order and emit the appropriate terminal Responses event.
    fn complete(&mut self) -> String {
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
        for pos in 0..self.tools.len() {
            self.announce_tool_if_ready(pos, &mut out);
        }
        // A slot whose name never arrived is model garbage the client cannot execute — and a
        // nameless function_call echoed into the next request is rejected upstream. Skip it.
        for slot in self
            .tools
            .iter()
            .filter(|slot| slot.announced && !slot.name.is_empty())
        {
            out.push_str(&self.close_tool_events(slot));
        }
        let mut items: Vec<(usize, Value)> = vec![];
        if let Some(r) = &self.reasoning {
            items.push((r.index, resp_reasoning_item(&r.id, &r.acc)));
        }
        if let Some(m) = &self.message {
            items.push((m.index, resp_message_item(&m.id, &m.acc)));
        }
        for slot in self
            .tools
            .iter()
            .filter(|slot| slot.announced && !slot.name.is_empty())
        {
            items.push((
                slot.index,
                self.tool_context.response_tool_item_with_reasoning(
                    &slot.id,
                    "completed",
                    &slot.call_id,
                    &slot.name,
                    normalized_tool_arguments(&slot.args),
                    self.reasoning
                        .as_ref()
                        .map(|reasoning| reasoning.acc.as_str()),
                ),
            ));
        }
        items.sort_by_key(|(i, _)| *i);
        let output: Vec<Value> = items.into_iter().map(|(_, v)| v).collect();
        if let Some(reason) = incomplete_reason(self.finish_reason.as_deref()) {
            self.failed = true;
            out.push_str(&resp_incomplete(
                &self.rid(),
                &self.client_model,
                output,
                self.input_tokens,
                self.cached_tokens,
                self.output_tokens,
                reason,
            ));
        } else {
            out.push_str(&resp_completed(
                &self.rid(),
                &self.client_model,
                output,
                self.input_tokens,
                self.cached_tokens,
                self.output_tokens,
            ));
        }
        out
    }

    /// Finalize a clean upstream EOF only when a Chat finish reason was observed. `[DONE]` calls
    /// `complete` directly; an EOF without either signal is a truncated stream.
    pub fn finish(&mut self) -> String {
        if self.stopped {
            return String::new();
        }
        if self.finish_reason.is_some() {
            self.complete()
        } else {
            self.fail("upstream stream ended before [DONE] or a finish reason")
        }
    }

    fn fail(&mut self, message: &str) -> String {
        if self.stopped {
            return String::new();
        }
        let mut out = String::new();
        self.ensure_created(&mut out);
        let id = self.rid();
        out.push_str(&resp_failed(&id, message));
        self.stopped = true;
        self.failed = true;
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
    tool_context: CodexToolContext,
    // Construction-time fallback. An upstream id may replace it only until response.created is
    // emitted; afterward this id is immutable so every event in the response agrees.
    resp_id: String,
    created: bool,
    next_index: usize,
    blocks: Vec<ABlock>,
    input_tokens: i64,
    cached_tokens: i64,
    output_tokens: i64,
    stop_reason: Option<String>,
    stopped: bool,
    failed: bool,
}

struct ABlock {
    a_index: u64,
    index: usize,
    id: String,
    kind: AKind,
    open: bool,
}

enum AKind {
    Text {
        acc: String,
    },
    Tool {
        call_id: String,
        name: String,
        args: String,
        start_args: String,
    },
    Think {
        acc: String,
    },
}

fn ablock_tool_arguments(args: &str, start_args: &str) -> String {
    if !args.trim().is_empty() {
        args.to_string()
    } else if !start_args.trim().is_empty() {
        start_args.to_string()
    } else {
        "{}".to_string()
    }
}

/// The closing event sequence for one finished block (its `*.done` events + `output_item.done`).
fn close_ablock_events(
    b: &ABlock,
    tool_context: &CodexToolContext,
    reasoning: Option<&str>,
) -> String {
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
        AKind::Tool {
            call_id,
            name,
            args,
            start_args,
        } => {
            let arguments = ablock_tool_arguments(args, start_args);
            let item = tool_context.response_tool_item_with_reasoning(
                &b.id,
                "completed",
                call_id,
                name,
                &arguments,
                reasoning,
            );
            match tool_context.kind_for_chat_name(name) {
                CodexToolKind::Custom => {
                    let input = custom_tool_input_from_chat_arguments(&arguments);
                    if !input.is_empty() {
                        out.push_str(&ev(
                            "response.custom_tool_call_input.delta",
                            json!({ "type": "response.custom_tool_call_input.delta", "item_id": b.id,
                                "call_id": call_id, "output_index": b.index, "delta": input }),
                        ));
                    }
                    out.push_str(&ev(
                        "response.custom_tool_call_input.done",
                        json!({ "type": "response.custom_tool_call_input.done", "item_id": b.id,
                            "call_id": call_id, "output_index": b.index, "input": input }),
                    ));
                }
                CodexToolKind::Function | CodexToolKind::Namespace | CodexToolKind::ToolSearch => {
                    out.push_str(&ev(
                        "response.function_call_arguments.done",
                        json!({ "type": "response.function_call_arguments.done", "item_id": b.id,
                            "output_index": b.index, "arguments": arguments }),
                    ));
                }
            }
            out.push_str(&ev(
                "response.output_item.done",
                json!({ "type": "response.output_item.done", "output_index": b.index,
                    "item": item }),
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

fn ablock_item(b: &ABlock, tool_context: &CodexToolContext, reasoning: Option<&str>) -> Value {
    match &b.kind {
        AKind::Text { acc } => resp_message_item(&b.id, acc),
        AKind::Tool {
            call_id,
            name,
            args,
            start_args,
        } => tool_context.response_tool_item_with_reasoning(
            &b.id,
            "completed",
            call_id,
            name,
            &ablock_tool_arguments(args, start_args),
            reasoning,
        ),
        AKind::Think { acc } => resp_reasoning_item(&b.id, acc),
    }
}

impl AnthropicToResponses {
    pub fn new(client_model: &str) -> Self {
        Self::new_with_context(client_model, CodexToolContext::default())
    }

    pub fn new_with_context(client_model: &str, tool_context: CodexToolContext) -> Self {
        Self {
            client_model: client_model.to_string(),
            tool_context,
            resp_id: super::uid("resp_ccbud"),
            created: false,
            next_index: 0,
            blocks: vec![],
            input_tokens: 0,
            cached_tokens: 0,
            output_tokens: 0,
            stop_reason: None,
            stopped: false,
            failed: false,
        }
    }

    fn rid(&self) -> String {
        self.resp_id.clone()
    }

    fn reasoning_text(&self) -> Option<String> {
        let text = self
            .blocks
            .iter()
            .filter_map(|block| match &block.kind {
                AKind::Think { acc } if !acc.trim().is_empty() => Some(acc.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        (!text.is_empty()).then_some(text)
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
        if self.stopped {
            return out;
        }
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
                    if !self.created {
                        if let Some(id) = m
                            .get("id")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                        {
                            self.resp_id = format!("resp_{}", id);
                        }
                    }
                    if let Some(u) = m.get("usage") {
                        // Responses-style input_tokens includes cached reads; Anthropic reports
                        // them separately.
                        let base = u.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                        let cr = u
                            .get("cache_read_input_tokens")
                            .and_then(|v| v.as_i64())
                            .unwrap_or(0);
                        let cc = u
                            .get("cache_creation_input_tokens")
                            .and_then(|v| v.as_i64())
                            .unwrap_or(0);
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
                        let id = response_scoped_item_id("msg", &self.rid(), index);
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
                        self.blocks.push(ABlock {
                            a_index,
                            index,
                            id,
                            kind: AKind::Text { acc: String::new() },
                            open: true,
                        });
                    }
                    Some("tool_use") => {
                        self.next_index += 1;
                        let call_id = response_scoped_call_id(&self.rid(), index);
                        let name = cb
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let id = self
                            .tool_context
                            .response_item_id(&name, &self.rid(), index);
                        let start_args = cb
                            .get("input")
                            .filter(|value| {
                                value.as_object().is_some_and(|object| !object.is_empty())
                            })
                            .map(Value::to_string)
                            .unwrap_or_default();
                        let reasoning = self.reasoning_text();
                        let item = self.tool_context.response_tool_item_with_reasoning(
                            &id,
                            "in_progress",
                            &call_id,
                            &name,
                            "",
                            reasoning.as_deref(),
                        );
                        let mut item = item;
                        if item.get("type").and_then(Value::as_str) == Some("function_call") {
                            item["arguments"] = json!("");
                        }
                        out.push_str(&ev(
                            "response.output_item.added",
                            json!({ "type": "response.output_item.added", "output_index": index,
                                "item": item }),
                        ));
                        self.blocks.push(ABlock {
                            a_index,
                            index,
                            id,
                            kind: AKind::Tool {
                                call_id,
                                name,
                                args: String::new(),
                                start_args,
                            },
                            open: true,
                        });
                    }
                    Some("thinking") => {
                        self.next_index += 1;
                        let id = response_scoped_item_id("rs", &self.rid(), index);
                        out.push_str(&ev(
                            "response.output_item.added",
                            json!({ "type": "response.output_item.added", "output_index": index,
                                "item": { "type": "reasoning", "id": id, "summary": [] } }),
                        ));
                        self.blocks.push(ABlock {
                            a_index,
                            index,
                            id,
                            kind: AKind::Think { acc: String::new() },
                            open: true,
                        });
                    }
                    // redacted_thinking / server_tool_use / … have no Responses equivalent; their
                    // deltas find no block below and drop.
                    _ => {}
                }
            }
            Some("content_block_delta") => {
                let a_index = evt.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                let delta = evt.get("delta").cloned().unwrap_or(Value::Null);
                if let Some(b) = self
                    .blocks
                    .iter_mut()
                    .find(|b| b.a_index == a_index && b.open)
                {
                    match (&mut b.kind, delta.get("type").and_then(|v| v.as_str())) {
                        (AKind::Text { acc }, Some("text_delta")) => {
                            if let Some(txt) = delta
                                .get("text")
                                .and_then(|v| v.as_str())
                                .filter(|s| !s.is_empty())
                            {
                                acc.push_str(txt);
                                out.push_str(&ev(
                                    "response.output_text.delta",
                                    json!({ "type": "response.output_text.delta", "item_id": b.id,
                                        "output_index": b.index, "content_index": 0, "delta": txt }),
                                ));
                            }
                        }
                        (AKind::Tool { name, args, .. }, Some("input_json_delta")) => {
                            if let Some(pj) = delta
                                .get("partial_json")
                                .and_then(|v| v.as_str())
                                .filter(|s| !s.is_empty())
                            {
                                args.push_str(pj);
                                if self.tool_context.kind_for_chat_name(name)
                                    != CodexToolKind::Custom
                                {
                                    out.push_str(&ev(
                                        "response.function_call_arguments.delta",
                                        json!({ "type": "response.function_call_arguments.delta", "item_id": b.id,
                                            "output_index": b.index, "delta": pj }),
                                    ));
                                }
                            }
                        }
                        (AKind::Think { acc }, Some("thinking_delta")) => {
                            if let Some(th) = delta
                                .get("thinking")
                                .and_then(|v| v.as_str())
                                .filter(|s| !s.is_empty())
                            {
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
                let reasoning = self.reasoning_text();
                if let Some(b) = self
                    .blocks
                    .iter_mut()
                    .find(|b| b.a_index == a_index && b.open)
                {
                    b.open = false;
                    out.push_str(&close_ablock_events(
                        b,
                        &self.tool_context,
                        reasoning.as_deref(),
                    ));
                }
            }
            Some("message_delta") => {
                if let Some(reason) = evt
                    .get("delta")
                    .and_then(|delta| delta.get("stop_reason"))
                    .and_then(Value::as_str)
                {
                    self.stop_reason = Some(reason.to_string());
                }
                if let Some(o) = evt
                    .get("usage")
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(|v| v.as_i64())
                {
                    self.output_tokens = o;
                }
            }
            Some("message_stop") => {
                out.push_str(&self.complete());
            }
            Some("error") => {
                let msg = evt
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("upstream error");
                out.push_str(&self.fail(msg));
            }
            _ => {} // ping etc.
        }
        out
    }

    /// Close any still-open blocks and emit the appropriate terminal Responses event.
    fn complete(&mut self) -> String {
        if self.stopped {
            return String::new();
        }
        self.stopped = true;
        let mut out = String::new();
        self.ensure_created(&mut out);
        let reasoning = self.reasoning_text();
        self.blocks.sort_by_key(|b| b.index);
        for b in &mut self.blocks {
            if b.open {
                b.open = false;
                out.push_str(&close_ablock_events(
                    b,
                    &self.tool_context,
                    reasoning.as_deref(),
                ));
            }
        }
        let output: Vec<Value> = self
            .blocks
            .iter()
            .map(|block| ablock_item(block, &self.tool_context, reasoning.as_deref()))
            .collect();
        if let Some(reason) = incomplete_reason(self.stop_reason.as_deref()) {
            self.failed = true;
            out.push_str(&resp_incomplete(
                &self.rid(),
                &self.client_model,
                output,
                self.input_tokens,
                self.cached_tokens,
                self.output_tokens,
                reason,
            ));
        } else {
            out.push_str(&resp_completed(
                &self.rid(),
                &self.client_model,
                output,
                self.input_tokens,
                self.cached_tokens,
                self.output_tokens,
            ));
        }
        out
    }

    /// Finalize a clean upstream EOF only after Anthropic reported a stop reason. A normal
    /// `message_stop` calls `complete` directly; an EOF before both signals is truncated.
    pub fn finish(&mut self) -> String {
        if self.stopped {
            return String::new();
        }
        if self.stop_reason.is_some() {
            self.complete()
        } else {
            self.fail("upstream stream ended before message_stop or a stop reason")
        }
    }

    fn fail(&mut self, message: &str) -> String {
        if self.stopped {
            return String::new();
        }
        let mut out = String::new();
        self.ensure_created(&mut out);
        let id = self.rid();
        out.push_str(&resp_failed(&id, message));
        self.stopped = true;
        self.failed = true;
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
        out.push_str(
            &tc.push("data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"}}]}"),
        );
        out.push_str(
            &tc.push("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Let me \"}}]}"),
        );
        out.push_str(
            &tc.push("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"check.\"}}]}"),
        );
        out.push_str(&tc.push("data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"pa\"},\"extra_content\":{\"google\":{\"thought_signature\":\"sig-stream-abc\"}}}]}}]}"));
        out.push_str(&tc.push("data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"th\\\":\\\"a.txt\\\"}\"}}]}}]}"));
        out.push_str(&tc.push("data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":20,\"completion_tokens\":9}}"));
        out.push_str(&tc.push("data: [DONE]"));

        // ordered events present (serde_json sorts object keys, so assert on substrings, not key order)
        assert!(out.contains("event: message_start"));
        assert!(
            out.find("event: message_start").unwrap()
                < out.find("event: content_block_start").unwrap()
        );
        assert!(
            out.find("event: content_block_start").unwrap()
                < out.find("event: message_delta").unwrap()
        );
        assert!(
            out.find("event: message_delta").unwrap() < out.find("event: message_stop").unwrap()
        );
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
        let captured = tc.captured_tool_calls();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].call_id, "call_1");
        assert_eq!(captured[0].arguments, r#"{"path":"a.txt"}"#);
        assert_eq!(
            captured[0].thought_signature.as_deref(),
            Some("sig-stream-abc")
        );
    }

    #[test]
    fn keeps_no_index_parallel_calls_with_the_same_id_distinct() {
        let mut tc = ChatToAnthropic::new("claude-x");
        let mut out = String::new();
        out.push_str(&tc.push("data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"id\":\"same-call\",\"type\":\"function\",\"function\":{\"name\":\"lookup\",\"arguments\":\"{\\\"query\\\":\\\"same\\\"}\"},\"extra_content\":{\"google\":{\"thought_signature\":\"sig-same-id\"}}},{\"id\":\"same-call\",\"type\":\"function\",\"function\":{\"name\":\"lookup\",\"arguments\":\"{\\\"query\\\":\\\"same\\\"}\"}}]}}]}\n"));
        out.push_str(&tc.push(
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n",
        ));
        out.push_str(&tc.push("data: [DONE]\n"));

        let captured = tc.captured_tool_calls();
        assert_eq!(captured.len(), 2);
        assert_eq!(captured[0].arguments, r#"{"query":"same"}"#);
        assert_eq!(captured[1].arguments, r#"{"query":"same"}"#);
        assert_eq!(
            captured[0].thought_signature.as_deref(),
            Some("sig-same-id")
        );
        assert!(captured[1].thought_signature.is_none());
        assert!(out.contains(r#""index":0,"type":"content_block_start""#));
        assert!(out.contains(r#""index":1,"type":"content_block_start""#));
    }

    #[test]
    fn plain_text_only() {
        let mut tc = ChatToAnthropic::new("claude-x");
        let mut out = String::new();
        out.push_str(
            &tc.push("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello\"}}]}"),
        );
        out.push_str(
            &tc.push("data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}"),
        );
        out.push_str(&tc.push("data: [DONE]"));
        assert!(out.contains("text_delta") && out.contains(r#""text":"hello""#));
        assert!(out.contains(r#""stop_reason":"end_turn""#));
        assert!(out.contains("event: message_stop"));
    }

    // The gateway's abort guard relies on done() flipping as soon as push() emits the terminal
    // client event — that is the moment Responses clients (Codex) hang up, before upstream EOF.
    #[test]
    fn done_flips_on_terminal_event_before_eof() {
        let mut tc = Transcoder::new(Wire::Anthropic, Wire::OpenAiResponses, "alias-x").unwrap();
        tc.push("data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":3}}}\n");
        tc.push("data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\"}}\n");
        tc.push("data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n");
        assert!(!tc.done());
        let out = tc.push("data: {\"type\":\"message_stop\"}\n");
        assert!(out.contains("response.completed"));
        assert!(tc.done());

        let mut tc = Transcoder::new(Wire::OpenAiChat, Wire::Anthropic, "claude-x").unwrap();
        tc.push("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello\"}}]}");
        assert!(!tc.done());
        let out = tc.push("data: [DONE]");
        assert!(out.contains("event: message_stop"));
        assert!(tc.done());
    }

    fn response_for_event(output: &str, event: &str) -> Value {
        let event_line = format!("event: {}", event);
        let frame = output
            .split("\n\n")
            .find(|frame| frame.lines().next() == Some(event_line.as_str()))
            .unwrap_or_else(|| panic!("missing {event} event in {output}"));
        let data = frame
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .unwrap();
        serde_json::from_str::<Value>(data).unwrap()["response"].clone()
    }

    fn response_id_for_event(output: &str, event: &str) -> String {
        response_for_event(output, event)["id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn chat_to_responses_response_ids_are_unique_and_stable() {
        let mut first = ChatToResponses::new("alias-x");
        let mut second = ChatToResponses::new("alias-x");
        let first_fallback = first.resp_id.clone();
        let second_fallback = second.resp_id.clone();
        assert!(first_fallback.starts_with("resp_ccbud_"));
        assert!(second_fallback.starts_with("resp_ccbud_"));
        assert_ne!(first_fallback, second_fallback);

        let mut first_out =
            first.push(r#"data: {"choices":[{"index":0,"delta":{"role":"assistant"}}]}"#);
        first_out.push_str(&first.push(
            r#"data: {"id":"chatcmpl-too-late","choices":[{"index":0,"delta":{"content":"hi"}}]}"#,
        ));
        first_out.push_str(&first.push("data: [DONE]"));
        assert_eq!(first.resp_id, first_fallback);
        assert_eq!(
            response_id_for_event(&first_out, "response.created"),
            first_fallback
        );
        assert_eq!(
            response_id_for_event(&first_out, "response.completed"),
            first_fallback
        );

        let second_out = second.push("data: [DONE]");
        assert_eq!(
            response_id_for_event(&second_out, "response.created"),
            second_fallback
        );
        assert_eq!(
            response_id_for_event(&second_out, "response.completed"),
            second_fallback
        );

        let mut upstream = ChatToResponses::new("alias-x");
        let upstream_fallback = upstream.resp_id.clone();
        let mut upstream_out = upstream.push(
            r#"data: {"id":"chatcmpl-early","choices":[{"index":0,"delta":{"role":"assistant"}}]}"#,
        );
        upstream_out.push_str(&upstream.push("data: [DONE]"));
        assert_ne!(upstream.resp_id, upstream_fallback);
        assert_eq!(upstream.resp_id, "resp_chatcmpl-early");
        assert_eq!(
            response_id_for_event(&upstream_out, "response.created"),
            "resp_chatcmpl-early"
        );
        assert_eq!(
            response_id_for_event(&upstream_out, "response.completed"),
            "resp_chatcmpl-early"
        );
    }

    #[test]
    fn anthropic_to_responses_response_ids_are_unique_and_stable() {
        let mut first = AnthropicToResponses::new("alias-x");
        let mut second = AnthropicToResponses::new("alias-x");
        let first_fallback = first.resp_id.clone();
        let second_fallback = second.resp_id.clone();
        assert!(first_fallback.starts_with("resp_ccbud_"));
        assert!(second_fallback.starts_with("resp_ccbud_"));
        assert_ne!(first_fallback, second_fallback);

        let mut first_out = first.push(
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        );
        first_out.push_str(&first.push(
            r#"data: {"type":"message_start","message":{"id":"msg_too_late","usage":{"input_tokens":1}}}"#,
        ));
        first_out.push_str(&first.push(r#"data: {"type":"message_stop"}"#));
        assert_eq!(first.resp_id, first_fallback);
        assert_eq!(
            response_id_for_event(&first_out, "response.created"),
            first_fallback
        );
        assert_eq!(
            response_id_for_event(&first_out, "response.completed"),
            first_fallback
        );

        let second_out = second.push(r#"data: {"type":"message_stop"}"#);
        assert_eq!(
            response_id_for_event(&second_out, "response.created"),
            second_fallback
        );
        assert_eq!(
            response_id_for_event(&second_out, "response.completed"),
            second_fallback
        );

        let mut upstream = AnthropicToResponses::new("alias-x");
        let upstream_fallback = upstream.resp_id.clone();
        let mut upstream_out = upstream.push(
            r#"data: {"type":"message_start","message":{"id":"msg_early","usage":{"input_tokens":1}}}"#,
        );
        upstream_out.push_str(&upstream.push(r#"data: {"type":"message_stop"}"#));
        assert_ne!(upstream.resp_id, upstream_fallback);
        assert_eq!(upstream.resp_id, "resp_msg_early");
        assert_eq!(
            response_id_for_event(&upstream_out, "response.created"),
            "resp_msg_early"
        );
        assert_eq!(
            response_id_for_event(&upstream_out, "response.completed"),
            "resp_msg_early"
        );
    }

    #[test]
    fn streaming_response_item_ids_are_scoped_to_the_response() {
        let chat = |upstream_id: &str| {
            let mut tc = ChatToResponses::new("alias-x");
            let mut out = tc.push(&format!(
                r#"data: {{"id":"{upstream_id}","choices":[{{"index":0,"delta":{{"reasoning_content":"think","content":"answer"}}}}]}}"#
            ));
            out.push_str(&tc.push("data: [DONE]"));
            response_for_event(&out, "response.completed")["output"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|item| item.get("id").and_then(Value::as_str))
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        };
        let first_chat = chat("chatcmpl-first");
        let second_chat = chat("chatcmpl-second");
        assert_eq!(first_chat.len(), 2);
        assert!(first_chat.iter().all(|id| id.contains("chatcmpl-first")));
        assert!(second_chat.iter().all(|id| id.contains("chatcmpl-second")));
        assert!(first_chat.iter().all(|id| !second_chat.contains(id)));

        let anthropic = |message_id: &str| {
            let mut tc = AnthropicToResponses::new("alias-x");
            let mut out = tc.push(&format!(
                r#"data: {{"type":"message_start","message":{{"id":"{message_id}","usage":{{"input_tokens":1}}}}}}"#
            ));
            out.push_str(&tc.push(
                r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            ));
            out.push_str(&tc.push(
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"think"}}"#,
            ));
            out.push_str(&tc.push(
                r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            ));
            out.push_str(&tc.push(
                r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"answer"}}"#,
            ));
            out.push_str(&tc.push(r#"data: {"type":"message_stop"}"#));
            response_for_event(&out, "response.completed")["output"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|item| item.get("id").and_then(Value::as_str))
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        };
        let first_anthropic = anthropic("msg-first");
        let second_anthropic = anthropic("msg-second");
        assert_eq!(first_anthropic.len(), 2);
        assert!(first_anthropic.iter().all(|id| id.contains("msg-first")));
        assert!(second_anthropic.iter().all(|id| id.contains("msg-second")));
        assert!(first_anthropic
            .iter()
            .all(|id| !second_anthropic.contains(id)));
    }

    #[test]
    fn chat_to_responses_text_and_split_tool_call() {
        let mut tc = ChatToResponses::new("alias-x");
        let mut out = String::new();
        out.push_str(&tc.push("data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"}}]}"));
        out.push_str(
            &tc.push("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Let me \"}}]}"),
        );
        out.push_str(
            &tc.push("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"check.\"}}]}"),
        );
        out.push_str(&tc.push("data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"shell\",\"arguments\":\"{\\\"co\"}}]}}]}"));
        out.push_str(&tc.push("data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"mmand\\\":[\\\"ls\\\"]}\"}}]}}]}"));
        out.push_str(&tc.push("data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":20,\"completion_tokens\":9,\"prompt_tokens_details\":{\"cached_tokens\":7}}}"));
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
        let client_call_id = response_scoped_call_id("resp_chatcmpl-1", 1);
        assert!(
            out.contains(&format!(r#""call_id":"{}""#, client_call_id))
                && out.contains(r#""name":"shell""#)
        );
        assert!(out.contains(r#""arguments":"{\"command\":[\"ls\"]}""#));
        // completed carries id + usage, incl. the prompt cache detail Codex reports
        assert!(out.contains(r#""id":"resp_chatcmpl-1""#));
        assert!(out.contains(r#""input_tokens":20"#) && out.contains(r#""output_tokens":9"#));
        assert!(out.contains(r#""cached_tokens":7"#));
        assert_eq!(tc.input_tokens(), 20);
        assert_eq!(tc.output_tokens(), 9);
        // captured for the gateway's signature cache, keyed by the call_id Codex echoes back
        let captured = tc.captured_tool_calls();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].call_id, client_call_id);
        assert_eq!(captured[0].arguments, r#"{"command":["ls"]}"#);
        // finish is idempotent — [DONE] already closed the stream
        assert_eq!(tc.finish(), "");
    }

    // Gemini's OpenAI-compatible stream omits `index` and can repeat ids across parallel calls;
    // before the fix every no-index fragment collapsed into slot 0 (one garbled call), so Codex
    // never received usable tool calls from a Gemini chat upstream.
    #[test]
    fn chat_to_responses_keeps_no_index_parallel_calls_distinct() {
        let mut tc = ChatToResponses::new("alias-x");
        let mut out = String::new();
        out.push_str(&tc.push("data: {\"id\":\"chatcmpl-2\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"id\":\"same-call\",\"type\":\"function\",\"function\":{\"name\":\"lookup\",\"arguments\":\"{\\\"query\\\":\\\"a\\\"}\"},\"extra_content\":{\"google\":{\"thought_signature\":\"sig-parallel\"}}},{\"id\":\"same-call\",\"type\":\"function\",\"function\":{\"name\":\"lookup\",\"arguments\":\"{\\\"query\\\":\\\"b\\\"}\"}}]}}]}\n"));
        out.push_str(&tc.push(
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n",
        ));
        out.push_str(&tc.push("data: [DONE]\n"));

        // two distinct function_call items, each with its own arguments
        assert!(out.contains(r#""output_index":0"#) && out.contains(r#""output_index":1"#));
        assert!(out.contains(r#""arguments":"{\"query\":\"a\"}""#));
        assert!(out.contains(r#""arguments":"{\"query\":\"b\"}""#));
        let captured = tc.captured_tool_calls();
        assert_eq!(captured.len(), 2);
        assert_ne!(captured[0].call_id, captured[1].call_id);
        assert_eq!(
            captured[0].call_id,
            response_scoped_call_id("resp_chatcmpl-2", 0)
        );
        assert_eq!(
            captured[1].call_id,
            response_scoped_call_id("resp_chatcmpl-2", 1)
        );
        assert!(out.contains(&format!(r#""call_id":"{}""#, captured[0].call_id)));
        assert!(out.contains(&format!(r#""call_id":"{}""#, captured[1].call_id)));
        assert_eq!(captured[0].arguments, r#"{"query":"a"}"#);
        assert_eq!(captured[1].arguments, r#"{"query":"b"}"#);
        // the Gemini thought signature is captured for the session cache (restore next turn)
        assert_eq!(
            captured[0].thought_signature.as_deref(),
            Some("sig-parallel")
        );
        assert!(captured[1].thought_signature.is_none());
    }

    // Some models emit tool-call fragments that never carry a function name; forwarding them
    // gives Codex an unexecutable call whose echo the upstream then rejects — drop them instead.
    #[test]
    fn chat_to_responses_skips_nameless_tool_calls() {
        let mut tc = ChatToResponses::new("alias-x");
        let mut out = String::new();
        out.push_str(&tc.push("data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{}\"}}]}}]}"));
        out.push_str(&tc.push(
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}",
        ));
        out.push_str(&tc.push("data: [DONE]"));
        assert!(!out.contains("response.function_call_arguments.done"));
        assert!(
            out.contains(r#""output":[]"#),
            "completed output stays empty: {}",
            out
        );
        assert!(out.contains(r#""type":"response.completed""#));
        assert!(tc.captured_tool_calls().is_empty());
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
        assert!(
            out.contains(r#""type":"response.reasoning_summary_text.delta""#)
                && out.contains(r#""delta":"hmm""#)
        );
        assert!(out.contains(r#""type":"summary_text""#));
        // text streams as deltas and closes with the full text
        assert!(out.contains(r#""delta":"Run""#) && out.contains(r#""delta":"ning.""#));
        assert!(out.contains(r#""text":"Running.""#));
        // tool_use → function_call item with the reassembled arguments string
        assert!(
            out.contains(&format!(
                r#""call_id":"{}""#,
                response_scoped_call_id("resp_msg_9", 2)
            )) && out.contains(r#""name":"shell""#)
        );
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
        out.push_str(&tc.push(
            r#"data: {"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":5}}}"#,
        ));
        out.push_str(&tc.push(
            r#"data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        ));
        out.push_str(&tc.finish());
        assert!(out.contains(r#""type":"response.failed""#) && out.contains("Overloaded"));
        // failed is terminal: no completed after it
        assert!(!out.contains(r#""type":"response.completed""#));
    }

    #[test]
    fn chat_error_events_are_terminal_for_translated_clients() {
        let mut responses =
            Transcoder::new(Wire::OpenAiChat, Wire::OpenAiResponses, "alias-x").unwrap();
        let mut responses_out =
            responses.push(r#"data: {"choices":[{"index":0,"delta":{"content":"partial"}}]}"#);
        responses_out.push_str(
            &responses
                .push(r#"data: {"error":{"type":"server_error","message":"upstream exploded"}}"#),
        );
        responses_out.push_str(&responses.finish());
        assert!(responses_out.contains(r#""type":"response.failed""#));
        assert!(responses_out.contains("upstream exploded"));
        assert!(!responses_out.contains(r#""type":"response.completed""#));
        assert!(responses.done());
        assert!(!responses.succeeded());

        let mut anthropic = Transcoder::new(Wire::OpenAiChat, Wire::Anthropic, "claude-x").unwrap();
        let mut anthropic_out =
            anthropic.push(r#"data: {"choices":[{"index":0,"delta":{"content":"partial"}}]}"#);
        anthropic_out.push_str(
            &anthropic
                .push(r#"data: {"error":{"type":"server_error","message":"upstream exploded"}}"#),
        );
        anthropic_out.push_str(&anthropic.finish());
        assert!(anthropic_out.contains("event: error"));
        assert!(anthropic_out.contains("upstream exploded"));
        assert!(!anthropic_out.contains("event: message_stop"));
        assert!(anthropic.done());
        assert!(!anthropic.succeeded());
    }

    #[test]
    fn transport_failure_cannot_be_finalized_as_success() {
        for (provider, client) in [
            (Wire::OpenAiChat, Wire::Anthropic),
            (Wire::OpenAiChat, Wire::OpenAiResponses),
            (Wire::Anthropic, Wire::OpenAiResponses),
        ] {
            let mut tc = Transcoder::new(provider, client, "alias-x").unwrap();
            let mut out = tc.fail("upstream stream transport error");
            out.push_str(&tc.finish());
            assert!(tc.done());
            assert!(!tc.succeeded());
            assert!(out.contains("upstream stream transport error"));
            assert!(!out.contains("response.completed"));
            assert!(!out.contains("event: message_stop"));
        }
    }

    #[test]
    fn premature_clean_eof_fails_but_reported_stop_reasons_can_finalize() {
        for (provider, client) in [
            (Wire::OpenAiChat, Wire::Anthropic),
            (Wire::OpenAiChat, Wire::OpenAiResponses),
            (Wire::Anthropic, Wire::OpenAiResponses),
        ] {
            let mut tc = Transcoder::new(provider, client, "alias-x").unwrap();
            tc.push(match provider {
                Wire::Anthropic => {
                    r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#
                }
                _ => r#"data: {"choices":[{"index":0,"delta":{"content":"partial"}}]}"#,
            });
            let out = tc.finish();
            assert!(tc.done());
            assert!(!tc.succeeded());
            assert!(out.contains("upstream stream ended before"));
            assert!(!out.contains("response.completed"));
            assert!(!out.contains("event: message_stop"));
        }

        let mut chat_responses =
            Transcoder::new(Wire::OpenAiChat, Wire::OpenAiResponses, "alias-x").unwrap();
        chat_responses.push(
            r#"data: {"choices":[{"index":0,"delta":{"content":"done"},"finish_reason":"stop"}]}"#,
        );
        let out = chat_responses.finish();
        assert!(out.contains("response.completed"));
        assert!(chat_responses.succeeded());

        let mut chat_anthropic =
            Transcoder::new(Wire::OpenAiChat, Wire::Anthropic, "claude-x").unwrap();
        chat_anthropic.push(
            r#"data: {"choices":[{"index":0,"delta":{"content":"done"},"finish_reason":"stop"}]}"#,
        );
        let out = chat_anthropic.finish();
        assert!(out.contains("event: message_stop"));
        assert!(chat_anthropic.succeeded());

        let mut anthropic =
            Transcoder::new(Wire::Anthropic, Wire::OpenAiResponses, "alias-x").unwrap();
        anthropic.push(
            r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#,
        );
        let out = anthropic.finish();
        assert!(out.contains("response.completed"));
        assert!(anthropic.succeeded());
    }

    #[test]
    fn max_token_truncation_emits_incomplete_instead_of_completed() {
        let mut chat = Transcoder::new(Wire::OpenAiChat, Wire::OpenAiResponses, "alias-x").unwrap();
        let mut chat_out = chat.push(
            r#"data: {"choices":[{"index":0,"delta":{"content":"partial"},"finish_reason":"length"}]}"#,
        );
        chat_out.push_str(&chat.push("data: [DONE]"));
        assert!(chat_out.contains("response.incomplete"));
        assert!(chat_out.contains(r#""reason":"max_output_tokens""#));
        assert!(!chat_out.contains("response.completed"));
        assert!(chat.done());
        assert!(!chat.succeeded());

        let mut anthropic =
            Transcoder::new(Wire::Anthropic, Wire::OpenAiResponses, "alias-x").unwrap();
        let mut anthropic_out = anthropic.push(
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        );
        anthropic_out.push_str(&anthropic.push(
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}}"#,
        ));
        anthropic_out.push_str(&anthropic.push(
            r#"data: {"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":8}}"#,
        ));
        anthropic_out.push_str(&anthropic.push(r#"data: {"type":"message_stop"}"#));
        assert!(anthropic_out.contains("response.incomplete"));
        assert!(anthropic_out.contains(r#""reason":"max_output_tokens""#));
        assert!(!anthropic_out.contains("response.completed"));
        assert!(anthropic.done());
        assert!(!anthropic.succeeded());
    }

    fn extended_tool_context() -> CodexToolContext {
        CodexToolContext::from_request(&json!({
            "tools": [
                { "type": "custom", "name": "apply_patch", "description": "Apply a patch" },
                { "type": "namespace", "name": "multi_agent_v1", "tools": [
                    { "type": "function", "name": "spawn_agent", "description": "Spawn",
                      "parameters": { "type": "object", "properties": {
                          "task_name": { "type": "string" }
                      }, "required": ["task_name"] } }
                ] },
                { "type": "tool_search", "execution": "client",
                  "description": "Search deferred tools.",
                  "parameters": { "type": "object", "properties": {
                      "query": { "type": "string" }
                  }, "required": ["query"] } }
            ]
        }))
    }

    #[test]
    fn chat_stream_restores_custom_and_tool_search_calls() {
        let mut tc = ChatToResponses::new_with_context("alias-x", extended_tool_context());
        let mut out = String::new();
        out.push_str(&tc.push(
            r#"data: {"id":"chatcmpl-tools","choices":[{"index":0,"delta":{"reasoning_content":"choose tools"}}]}"#,
        ));
        out.push_str(&tc.push(
            r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_patch","type":"function","function":{"name":"apply_patch","arguments":"{\"input\":\"*** Begin"}},{"index":1,"id":"call_search","type":"function","function":{"name":"tool_search","arguments":"{\"query\":\"browser\"}"}}]}}]}"#,
        ));
        out.push_str(&tc.push(
            r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":" Patch\"}"}}]},"finish_reason":"tool_calls"}]}"#,
        ));
        out.push_str(&tc.push("data: [DONE]"));

        assert!(out.contains("event: response.custom_tool_call_input.delta"));
        assert!(out.contains("event: response.custom_tool_call_input.done"));
        assert!(out.contains(r#""type":"custom_tool_call""#));
        assert!(out.contains(r#""input":"*** Begin Patch""#));
        assert!(out.contains(r#""type":"tool_search_call""#));
        assert!(out.contains(r#""arguments":{"query":"browser"}"#));
        assert!(out.contains(r#""reasoning_content":"choose tools""#));
        assert!(out.contains(r#""type":"response.completed""#));
    }

    #[test]
    fn anthropic_stream_restores_namespace_and_custom_calls() {
        let mut tc = AnthropicToResponses::new_with_context("alias-x", extended_tool_context());
        let mut out = String::new();
        out.push_str(&tc.push(
            r#"data: {"type":"message_start","message":{"id":"msg_tools","usage":{"input_tokens":3}}}"#,
        ));
        out.push_str(&tc.push(
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
        ));
        out.push_str(&tc.push(
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"delegate"}}"#,
        ));
        out.push_str(&tc.push(r#"data: {"type":"content_block_stop","index":0}"#));
        out.push_str(&tc.push(
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_spawn","name":"multi_agent_v1__spawn_agent","input":{}}}"#,
        ));
        out.push_str(&tc.push(
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"task_name\":\"audit\"}"}}"#,
        ));
        out.push_str(&tc.push(r#"data: {"type":"content_block_stop","index":1}"#));
        out.push_str(&tc.push(
            r#"data: {"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_patch","name":"apply_patch","input":{"input":"*** Begin Patch"}}}"#,
        ));
        out.push_str(&tc.push(r#"data: {"type":"content_block_stop","index":2}"#));
        out.push_str(&tc.push(
            r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5}}"#,
        ));
        out.push_str(&tc.push(r#"data: {"type":"message_stop"}"#));

        assert!(out.contains(r#""type":"function_call""#));
        assert!(out.contains(r#""name":"spawn_agent""#));
        assert!(out.contains(r#""namespace":"multi_agent_v1""#));
        assert!(out.contains(r#""reasoning_content":"delegate""#));
        assert!(out.contains("event: response.custom_tool_call_input.delta"));
        assert!(out.contains("event: response.custom_tool_call_input.done"));
        assert!(out.contains(r#""type":"custom_tool_call""#));
        assert!(out.contains(r#""input":"*** Begin Patch""#));
        assert!(out.contains(r#""type":"response.completed""#));
    }
}
