// OpenAI Responses (/v1/responses) codec — BOTH halves:
//
//   provider-side (gateway → a Responses upstream):
//     encode_request:  IR → Responses REQUEST body
//     decode_response: Responses RESPONSE → IR
//
//   client-side (a Responses client, i.e. Codex with wire_api="responses", → gateway):
//     decode_request:  Responses REQUEST → IR
//     encode_response / encode_response_sse: IR → Responses RESPONSE (json / synthesized SSE)
//
// The Responses API uses an item-based `input` array (role messages + function_call /
// function_call_output items), `instructions` for the system prompt, `max_output_tokens`, and a
// `reasoning.effort` knob. Its response is an `output` array of items. Tool definitions are
// FLATTENED at the item level (`{"type":"function","name",...}`), unlike Chat Completions.
//
// The client-side halves are hand-rolled rather than reusing llm-connector's
// responses_request_to_chat_request / chat_response_to_responses_response: the crate's versions
// silently DROP function_call / function_call_output / assistant output_text history items and
// tool_calls in responses, and reject the flattened tool form — all fatal for Codex, whose agent
// loop is tool calls end-to-end.
//
// Codex reads the turn's items ONLY from `response.output_item.done` SSE events (text deltas are
// cosmetic; the stream MUST end with `response.completed` carrying id + usage), so the synthesized
// stream emits the full added → delta → done sequence per item.

use llm_connector::core::Protocol;
use llm_connector::protocols::adapters::openai::OpenAIProtocol;
use llm_connector::types::{
    ChatRequest, ChatResponse, FunctionCall, Message, MessageBlock, Role, Tool, ToolCall, ToolChoice,
};
use serde_json::{json, Value};

/// Map a thinking budget (tokens) to a Responses reasoning effort tier.
fn budget_to_effort(budget: Option<u32>) -> &'static str {
    match budget {
        Some(b) if b >= 8192 => "high",
        Some(b) if b >= 2048 => "medium",
        _ => "low",
    }
}

/// Reverse of budget_to_effort: a Responses reasoning effort tier → a thinking budget (tokens).
fn effort_to_budget(effort: &str) -> u32 {
    match effort {
        "high" => 16384,
        "medium" => 4096,
        _ => 1024, // "low" / "minimal"
    }
}

/// IR (ChatRequest) → OpenAI Responses request BODY. `outgoing_model` is the provider's real model.
pub fn encode_request(ir: &ChatRequest, outgoing_model: &str, stream: bool) -> Value {
    let mut instructions: Option<String> = None;
    let mut input: Vec<Value> = vec![];

    for m in &ir.messages {
        match m.role {
            Role::System => {
                // Responses carries the system prompt in `instructions`, not the input array.
                let t = m.content_as_text();
                if !t.trim().is_empty() {
                    instructions = Some(match instructions.take() {
                        Some(prev) => format!("{}\n{}", prev, t),
                        None => t,
                    });
                }
            }
            Role::Tool => {
                // a tool result → function_call_output item
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": m.tool_call_id.clone().unwrap_or_default(),
                    "output": m.content_as_text(),
                }));
            }
            Role::User => {
                let text = m.content_as_text();
                let mut content: Vec<Value> = vec![];
                if !text.is_empty() {
                    content.push(json!({ "type": "input_text", "text": text }));
                }
                for b64 in m.content_as_images_base64() {
                    content.push(json!({ "type": "input_image", "image_url": format!("data:image/png;base64,{}", b64) }));
                }
                if !content.is_empty() {
                    input.push(json!({ "type": "message", "role": "user", "content": content }));
                }
            }
            Role::Assistant => {
                let text = m.content_as_text();
                if !text.is_empty() {
                    input.push(json!({ "type": "message", "role": "assistant",
                        "content": [{ "type": "output_text", "text": text }] }));
                }
                if let Some(calls) = &m.tool_calls {
                    for tc in calls {
                        input.push(json!({
                            "type": "function_call",
                            "call_id": tc.id,
                            "name": tc.function.name,
                            "arguments": tc.function.arguments,
                        }));
                    }
                }
            }
        }
    }

    let mut body = json!({
        "model": outgoing_model,
        "input": input,
        "stream": stream,
    });
    if let Some(instr) = instructions {
        body["instructions"] = json!(instr);
    }
    if let Some(mt) = ir.max_tokens {
        body["max_output_tokens"] = json!(mt);
    }
    if let Some(t) = ir.temperature {
        body["temperature"] = json!(t);
    }
    if let Some(p) = ir.top_p {
        body["top_p"] = json!(p);
    }
    // tools → Responses function tools (fields flattened at the item level, not nested under
    // "function" like Chat Completions).
    if let Some(tools) = &ir.tools {
        let arr: Vec<Value> = tools
            .iter()
            .map(|t| json!({
                "type": "function",
                "name": t.function.name,
                "description": t.function.description,
                "parameters": t.function.parameters,
            }))
            .collect();
        if !arr.is_empty() {
            body["tools"] = json!(arr);
        }
    }
    // Anthropic extended thinking → Responses reasoning effort.
    if ir.enable_thinking == Some(true) {
        body["reasoning"] = json!({ "effort": budget_to_effort(ir.thinking_budget) });
    }
    body
}

/// OpenAI Responses RESPONSE (buffered) → IR. We reshape the Responses reply into an OpenAI Chat
/// completion and let the crate's parse_response build the IR — reusing its battle-tested mapping.
pub fn decode_response(text: &str) -> Result<ChatResponse, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("responses parse: {}", e))?;
    let output = v.get("output").and_then(|o| o.as_array()).cloned().unwrap_or_default();

    let mut content = String::new();
    let mut tool_calls: Vec<Value> = vec![];
    let mut had_tool = false;
    for item in &output {
        match item.get("type").and_then(|t| t.as_str()) {
            Some("message") => {
                if let Some(cs) = item.get("content").and_then(|c| c.as_array()) {
                    for c in cs {
                        if let Some(t) = c.get("text").and_then(|v| v.as_str()) {
                            content.push_str(t);
                        }
                    }
                }
            }
            Some("function_call") => {
                had_tool = true;
                tool_calls.push(json!({
                    "id": item.get("call_id").or_else(|| item.get("id")).cloned().unwrap_or(json!("")),
                    "type": "function",
                    "function": {
                        "name": item.get("name").cloned().unwrap_or(json!("")),
                        "arguments": item.get("arguments").and_then(|v| v.as_str()).unwrap_or("{}"),
                    }
                }));
            }
            _ => {}
        }
    }
    // fall back to the flattened output_text if no message items carried content
    if content.is_empty() {
        if let Some(t) = v.get("output_text").and_then(|v| v.as_str()) {
            content = t.to_string();
        }
    }

    let usage = v.get("usage").cloned().unwrap_or(json!({}));
    let mut message = json!({ "role": "assistant", "content": content });
    if !tool_calls.is_empty() {
        message["tool_calls"] = json!(tool_calls);
    }
    let chat = json!({
        "id": v.get("id").cloned().unwrap_or(json!("resp")),
        "object": "chat.completion",
        "created": 0,
        "model": v.get("model").cloned().unwrap_or(json!("")),
        "choices": [{ "index": 0, "finish_reason": if had_tool { "tool_calls" } else { "stop" }, "message": message }],
        "usage": {
            "prompt_tokens": usage.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0),
            "completion_tokens": usage.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0),
            "total_tokens": usage.get("total_tokens").and_then(|v| v.as_i64()).unwrap_or(0),
        }
    });
    OpenAIProtocol::new("")
        .parse_response(&chat.to_string())
        .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// client-side half: a Responses client (Codex) in front of the gateway
// ---------------------------------------------------------------------------

/// Pull text out of a Responses content value (string, or array of typed parts). Accepts the
/// input_text / output_text / text / summary_text part flavors.
fn parts_text(content: &Value) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    let arr = match content.as_array() {
        Some(a) => a,
        None => return String::new(),
    };
    let mut out: Vec<String> = vec![];
    for p in arr {
        match p.get("type").and_then(|t| t.as_str()) {
            Some("input_text") | Some("output_text") | Some("text") | Some("summary_text") => {
                if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
                    out.push(t.to_string());
                }
            }
            _ => {}
        }
    }
    out.join("\n")
}

/// input_image parts → IR image blocks. Codex sends `image_url` as a data URI (screenshots /
/// attached images); a plain URL is also accepted per the OpenAI spec.
fn parts_images(content: &Value) -> Vec<MessageBlock> {
    let arr = match content.as_array() {
        Some(a) => a,
        None => return vec![],
    };
    let mut out = vec![];
    for p in arr {
        if p.get("type").and_then(|t| t.as_str()) != Some("input_image") {
            continue;
        }
        let url = p
            .get("image_url")
            .and_then(|v| v.as_str())
            .or_else(|| p.get("image_url").and_then(|v| v.get("url")).and_then(|v| v.as_str()));
        let Some(u) = url else { continue };
        if let Some(rest) = u.strip_prefix("data:") {
            if let Some((meta, data)) = rest.split_once(";base64,") {
                if !data.is_empty() {
                    out.push(MessageBlock::image_base64(if meta.is_empty() { "image/png" } else { meta }, data));
                }
                continue;
            }
        }
        out.push(MessageBlock::image_url(u));
    }
    out
}

/// Decode an OpenAI Responses REQUEST json (what Codex sends with wire_api="responses") into the
/// IR. Handles the full item vocabulary of an agentic history: message items (user input_text /
/// input_image, assistant output_text), function_call, function_call_output. `reasoning` items
/// (another model's chain-of-thought) and OpenAI-native tool items (local_shell_call,
/// web_search_call, custom_tool_call…) have no cross-provider equivalent and are dropped.
pub fn decode_request(req: &Value) -> Result<ChatRequest, String> {
    let model = req.get("model").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let mut messages: Vec<Message> = vec![];

    if let Some(instr) = req.get("instructions").and_then(|v| v.as_str()) {
        if !instr.trim().is_empty() {
            messages.push(Message::text(Role::System, instr));
        }
    }

    match req.get("input") {
        Some(Value::String(s)) => {
            if !s.is_empty() {
                messages.push(Message::text(Role::User, s.clone()));
            }
        }
        Some(Value::Array(items)) => {
            for item in items {
                // Bare `{role, content}` items (no "type") are legal Responses input; treat them
                // as message items.
                let ty = item
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or(if item.get("role").is_some() { "message" } else { "" });
                match ty {
                    "message" => {
                        let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("user");
                        let content = item.get("content").cloned().unwrap_or(Value::Null);
                        let text = parts_text(&content);
                        match role {
                            "assistant" => {
                                if !text.is_empty() {
                                    messages.push(Message::text(Role::Assistant, text));
                                }
                            }
                            "system" | "developer" => {
                                if !text.is_empty() {
                                    messages.push(Message::text(Role::System, text));
                                }
                            }
                            _ => {
                                let mut blocks: Vec<MessageBlock> = vec![];
                                if !text.is_empty() {
                                    blocks.push(MessageBlock::text(text));
                                }
                                blocks.extend(parts_images(&content));
                                if !blocks.is_empty() {
                                    messages.push(Message::new(Role::User, blocks));
                                }
                            }
                        }
                    }
                    "function_call" => {
                        let call = ToolCall {
                            id: item
                                .get("call_id")
                                .or_else(|| item.get("id"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            call_type: "function".to_string(),
                            function: FunctionCall {
                                name: item.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                                arguments: match item.get("arguments") {
                                    Some(Value::String(s)) => s.clone(),
                                    Some(v) if !v.is_null() => v.to_string(),
                                    _ => "{}".to_string(),
                                },
                                thought_signature: None,
                            },
                            index: None,
                            thought_signature: None,
                        };
                        // Codex emits a turn's prose (`message` item) and its tool calls as
                        // sibling items — fold calls into the trailing assistant message so the
                        // IR carries one assistant turn, mirroring the Chat/Anthropic shape.
                        match messages.last_mut() {
                            Some(m) if m.role == Role::Assistant => {
                                m.tool_calls.get_or_insert_with(Vec::new).push(call);
                            }
                            _ => {
                                let mut m = Message::new(Role::Assistant, vec![]);
                                m.tool_calls = Some(vec![call]);
                                messages.push(m);
                            }
                        }
                    }
                    "function_call_output" => {
                        let id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let text = match item.get("output") {
                            Some(Value::String(s)) => s.clone(),
                            Some(v @ Value::Array(_)) => parts_text(v),
                            Some(Value::Object(o)) => o
                                .get("content")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string())
                                .unwrap_or_else(|| Value::Object(o.clone()).to_string()),
                            _ => String::new(),
                        };
                        messages.push(Message::tool(text, id));
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }

    let mut cr = ChatRequest::new(model).with_messages(messages);
    if let Some(mt) = req.get("max_output_tokens").and_then(|v| v.as_u64()) {
        cr = cr.with_max_tokens(mt as u32);
    }
    if let Some(t) = req.get("temperature").and_then(|v| v.as_f64()) {
        cr = cr.with_temperature(t as f32);
    }
    if let Some(p) = req.get("top_p").and_then(|v| v.as_f64()) {
        cr = cr.with_top_p(p as f32);
    }
    if req.get("stream").and_then(|v| v.as_bool()).unwrap_or(false) {
        cr = cr.with_stream(true);
    }
    // tools: the Responses form flattens function fields at the item level; accept the nested
    // Chat shape too. Non-function tools (local_shell, web_search, custom…) have no chat/messages
    // equivalent and are dropped.
    if let Some(tools) = req.get("tools").and_then(|v| v.as_array()) {
        let mut ts: Vec<Tool> = vec![];
        for t in tools {
            if t.get("type").and_then(|v| v.as_str()).unwrap_or("function") != "function" {
                continue;
            }
            let f = t.get("function").filter(|f| f.is_object()).unwrap_or(t);
            let Some(name) = f.get("name").and_then(|v| v.as_str()) else { continue };
            let desc = f.get("description").and_then(|v| v.as_str()).map(|s| s.to_string());
            let params = f
                .get("parameters")
                .cloned()
                .filter(|p| !p.is_null())
                .unwrap_or_else(|| json!({ "type": "object" }));
            ts.push(Tool::function(name, desc, params));
        }
        if !ts.is_empty() {
            cr = cr.with_tools(ts);
        }
    }
    // tool_choice: mode strings pass through; both the flattened Responses object form
    // ({type:"function",name}) and the nested Chat form pin a specific function.
    if let Some(tc) = req.get("tool_choice") {
        if let Some(mode) = tc.as_str() {
            if matches!(mode, "auto" | "none" | "required") {
                cr.tool_choice = Some(ToolChoice::Mode(mode.to_string()));
            }
        } else if tc.get("type").and_then(|v| v.as_str()) == Some("function") {
            if let Some(name) = tc
                .get("name")
                .and_then(|v| v.as_str())
                .or_else(|| tc.get("function").and_then(|f| f.get("name")).and_then(|v| v.as_str()))
            {
                cr.tool_choice = Some(ToolChoice::function(name));
            }
        }
    }
    // reasoning.effort → IR thinking (an Anthropic upstream turns this into a thinking budget;
    // chat upstreams ignore it).
    if let Some(effort) = req.get("reasoning").and_then(|r| r.get("effort")).and_then(|v| v.as_str()) {
        cr = cr.with_enable_thinking(true).with_thinking_budget(effort_to_budget(effort));
    }
    Ok(cr)
}

/// Encode the IR response back into an OpenAI Responses RESPONSE json. `client_model` is the name
/// the client asked for (so Codex sees its own model, not the upstream's). Unlike the crate's
/// chat_response_to_responses_response this maps tool_calls → function_call items and provider
/// reasoning → a reasoning item — both load-bearing for Codex's agent loop.
pub fn encode_response(resp: &ChatResponse, client_model: &str) -> Value {
    let choice = resp.choices.first();
    let msg = choice.map(|c| &c.message);
    // Same fallback as anthropic.rs: when a turn has tool_calls the crate parks the prose only in
    // the top-level ChatResponse.content.
    let text = {
        let t = msg.map(|m| m.content_as_text()).unwrap_or_default();
        if t.is_empty() { resp.content.clone() } else { t }
    };
    let rid = if resp.id.is_empty() { "ccbud".to_string() } else { resp.id.clone() };

    let mut output: Vec<Value> = vec![];
    if let Some(reasoning) = msg.and_then(|m| m.reasoning_any()) {
        if !reasoning.trim().is_empty() {
            output.push(json!({ "type": "reasoning", "id": format!("rs_{}", rid),
                "summary": [{ "type": "summary_text", "text": reasoning }] }));
        }
    }
    if !text.is_empty() {
        output.push(json!({ "type": "message", "id": format!("msg_{}", rid), "status": "completed",
            "role": "assistant",
            "content": [{ "type": "output_text", "annotations": [], "text": text }] }));
    }
    if let Some(m) = msg {
        if let Some(calls) = &m.tool_calls {
            for (i, tc) in calls.iter().enumerate() {
                output.push(json!({ "type": "function_call", "id": format!("fc_{}_{}", rid, i),
                    "status": "completed",
                    "call_id": if tc.id.is_empty() { format!("call_{}", i) } else { tc.id.clone() },
                    "name": tc.function.name,
                    "arguments": if tc.function.arguments.is_empty() { "{}".to_string() } else { tc.function.arguments.clone() } }));
            }
        }
    }
    if output.is_empty() {
        // Codex builds the turn from output items; an empty message beats an empty array.
        output.push(json!({ "type": "message", "id": format!("msg_{}", rid), "status": "completed",
            "role": "assistant", "content": [{ "type": "output_text", "annotations": [], "text": "" }] }));
    }

    let usage = resp.usage.as_ref();
    let input_tokens = usage.map(|u| u.prompt_tokens).unwrap_or(0) as i64;
    let output_tokens = usage.map(|u| u.completion_tokens).unwrap_or(0) as i64;
    let total = (usage.map(|u| u.total_tokens).unwrap_or(0) as i64).max(input_tokens + output_tokens);
    json!({
        "id": format!("resp_{}", rid),
        "object": "response",
        "created_at": resp.created,
        "status": "completed",
        "model": client_model,
        "output": output,
        "output_text": text,
        "usage": {
            "input_tokens": input_tokens,
            "input_tokens_details": { "cached_tokens": 0 },
            "output_tokens": output_tokens,
            "output_tokens_details": { "reasoning_tokens": 0 },
            "total_tokens": total,
        }
    })
}

fn sse_ev(data: &Value) -> String {
    let t = data.get("type").and_then(|v| v.as_str()).unwrap_or("message");
    format!("event: {}\ndata: {}\n\n", t, serde_json::to_string(data).unwrap_or_default())
}

/// Synthesize a complete OpenAI Responses SSE event sequence from a finished IR response. Used when
/// the client (Codex) asked to stream but the upstream was translated buffered — the client still
/// gets a valid `response.created → output_item.added/delta/done per item → response.completed`
/// stream, just delivered at once. Codex materializes items only from `response.output_item.done`
/// and errors if the stream ends without `response.completed`, so both are non-negotiable.
pub fn encode_response_sse(resp: &ChatResponse, client_model: &str) -> String {
    let full = encode_response(resp, client_model);
    let rid = full.get("id").and_then(|v| v.as_str()).unwrap_or("resp_ccbud").to_string();
    let mut out = String::new();
    out.push_str(&sse_ev(&json!({ "type": "response.created",
        "response": { "id": rid, "object": "response", "status": "in_progress", "model": client_model } })));

    let items = full.get("output").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    for (idx, item) in items.iter().enumerate() {
        let item_id = item.get("id").and_then(|v| v.as_str()).unwrap_or("item").to_string();
        match item.get("type").and_then(|v| v.as_str()).unwrap_or("") {
            "message" => {
                let text = item["content"][0]["text"].as_str().unwrap_or("");
                out.push_str(&sse_ev(&json!({ "type": "response.output_item.added", "output_index": idx,
                    "item": { "type": "message", "id": item_id, "status": "in_progress", "role": "assistant", "content": [] } })));
                out.push_str(&sse_ev(&json!({ "type": "response.content_part.added", "item_id": item_id,
                    "output_index": idx, "content_index": 0,
                    "part": { "type": "output_text", "annotations": [], "text": "" } })));
                if !text.is_empty() {
                    out.push_str(&sse_ev(&json!({ "type": "response.output_text.delta", "item_id": item_id,
                        "output_index": idx, "content_index": 0, "delta": text })));
                }
                out.push_str(&sse_ev(&json!({ "type": "response.output_text.done", "item_id": item_id,
                    "output_index": idx, "content_index": 0, "text": text })));
                out.push_str(&sse_ev(&json!({ "type": "response.content_part.done", "item_id": item_id,
                    "output_index": idx, "content_index": 0,
                    "part": { "type": "output_text", "annotations": [], "text": text } })));
                out.push_str(&sse_ev(&json!({ "type": "response.output_item.done", "output_index": idx, "item": item })));
            }
            "function_call" => {
                let args = item.get("arguments").and_then(|v| v.as_str()).unwrap_or("{}");
                let mut added = item.clone();
                added["status"] = json!("in_progress");
                added["arguments"] = json!("");
                out.push_str(&sse_ev(&json!({ "type": "response.output_item.added", "output_index": idx, "item": added })));
                out.push_str(&sse_ev(&json!({ "type": "response.function_call_arguments.delta", "item_id": item_id,
                    "output_index": idx, "delta": args })));
                out.push_str(&sse_ev(&json!({ "type": "response.function_call_arguments.done", "item_id": item_id,
                    "output_index": idx, "arguments": args })));
                out.push_str(&sse_ev(&json!({ "type": "response.output_item.done", "output_index": idx, "item": item })));
            }
            "reasoning" => {
                let think = item["summary"][0]["text"].as_str().unwrap_or("");
                out.push_str(&sse_ev(&json!({ "type": "response.output_item.added", "output_index": idx,
                    "item": { "type": "reasoning", "id": item_id, "summary": [] } })));
                if !think.is_empty() {
                    out.push_str(&sse_ev(&json!({ "type": "response.reasoning_summary_text.delta", "item_id": item_id,
                        "output_index": idx, "summary_index": 0, "delta": think })));
                }
                out.push_str(&sse_ev(&json!({ "type": "response.output_item.done", "output_index": idx, "item": item })));
            }
            _ => {
                out.push_str(&sse_ev(&json!({ "type": "response.output_item.done", "output_index": idx, "item": item })));
            }
        }
    }

    out.push_str(&sse_ev(&json!({ "type": "response.completed", "response": full })));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_ir_to_responses_request() {
        let anthropic = json!({
            "model": "claude-x", "max_tokens": 500,
            "system": "be terse",
            "tools": [{ "name": "grep", "description": "search", "input_schema": { "type": "object" } }],
            "thinking": { "type": "enabled", "budget_tokens": 4096 },
            "messages": [
                { "role": "user", "content": "find foo" },
                { "role": "assistant", "content": [{ "type": "tool_use", "id": "c1", "name": "grep", "input": { "q": "foo" } }] },
                { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "c1", "content": "found" }] }
            ]
        });
        let ir = crate::protocol::anthropic::decode_request(&anthropic).unwrap();
        let body = encode_request(&ir, "gpt-5.5", false);

        assert_eq!(body["model"], "gpt-5.5");
        assert_eq!(body["instructions"], "be terse");
        assert_eq!(body["max_output_tokens"], 500);
        assert_eq!(body["reasoning"]["effort"], "medium"); // 4096 → medium
        // tools flattened (name at item level, not under "function")
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "grep");
        // input items: user message, function_call, function_call_output
        let input = body["input"].as_array().unwrap();
        assert!(input.iter().any(|i| i["type"] == "message" && i["role"] == "user"
            && i["content"][0]["type"] == "input_text" && i["content"][0]["text"] == "find foo"));
        let fc = input.iter().find(|i| i["type"] == "function_call").unwrap();
        assert_eq!(fc["name"], "grep");
        assert_eq!(fc["call_id"], "c1");
        let fco = input.iter().find(|i| i["type"] == "function_call_output").unwrap();
        assert_eq!(fco["call_id"], "c1");
        assert_eq!(fco["output"], "found");
    }

    #[test]
    fn decodes_responses_reply_to_ir_then_anthropic() {
        // A Responses reply with an assistant message + a function_call output item.
        let resp = json!({
            "id": "resp_1", "object": "response", "created_at": 1, "model": "gpt-5.5", "status": "completed",
            "output": [
                { "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": "Working on it." }] },
                { "type": "function_call", "call_id": "call_7", "name": "grep", "arguments": "{\"q\":\"foo\"}" }
            ],
            "usage": { "input_tokens": 15, "output_tokens": 8, "total_tokens": 23 }
        });
        let ir = decode_response(&resp.to_string()).unwrap();
        // reuse the Anthropic response encoder → verify the round-trip surfaces text + tool_use + usage
        let out = crate::protocol::anthropic::encode_response(&ir, "claude-x");
        assert_eq!(out["stop_reason"], "tool_use");
        assert_eq!(out["usage"]["input_tokens"], 15);
        assert_eq!(out["usage"]["output_tokens"], 8);
        let content = out["content"].as_array().unwrap();
        assert!(content.iter().any(|b| b["type"] == "text" && b["text"] == "Working on it."));
        let tu = content.iter().find(|b| b["type"] == "tool_use").unwrap();
        assert_eq!(tu["name"], "grep");
        assert_eq!(tu["input"]["q"], "foo");
    }

    // A representative Codex request (wire_api="responses"): instructions, flattened function
    // tools, and an agentic history — user message, assistant prose + function_call, its
    // function_call_output, and a reasoning item that must be dropped.
    fn codex_request() -> Value {
        json!({
            "model": "z-ai/glm-5.2",
            "instructions": "You are Codex.",
            "input": [
                { "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "list files" }] },
                { "type": "reasoning", "id": "rs_x", "summary": [{ "type": "summary_text", "text": "thinking…" }] },
                { "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": "Running ls." }] },
                { "type": "function_call", "call_id": "call_1", "name": "shell", "arguments": "{\"command\":[\"ls\"]}" },
                { "type": "function_call_output", "call_id": "call_1", "output": "a.txt\nb.txt" },
                { "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "read a.txt" }] }
            ],
            "tools": [
                { "type": "function", "name": "shell", "description": "run a command", "strict": false,
                  "parameters": { "type": "object", "properties": { "command": { "type": "array" } } } },
                { "type": "web_search" }
            ],
            "tool_choice": "auto",
            "parallel_tool_calls": false,
            "reasoning": { "effort": "medium", "summary": "auto" },
            "store": false,
            "stream": true
        })
    }

    #[test]
    fn decodes_codex_responses_request_to_ir() {
        let ir = decode_request(&codex_request()).unwrap();
        let roles: Vec<_> = ir.messages.iter().map(|m| format!("{:?}", m.role)).collect();
        // instructions → System; assistant prose + function_call folded into ONE assistant turn;
        // function_call_output → Tool; reasoning item dropped.
        assert_eq!(roles, vec!["System", "User", "Assistant", "Tool", "User"]);
        assert_eq!(ir.messages[0].content_as_text(), "You are Codex.");
        assert_eq!(ir.messages[1].content_as_text(), "list files");
        assert_eq!(ir.messages[2].content_as_text(), "Running ls.");
        let calls = ir.messages[2].tool_calls.as_ref().unwrap();
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].function.name, "shell");
        assert!(calls[0].function.arguments.contains("ls"));
        assert_eq!(ir.messages[3].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(ir.messages[3].content_as_text(), "a.txt\nb.txt");
        // flattened function tool recognized, non-function web_search dropped
        let tools = ir.tools.as_ref().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].function.name, "shell");
        assert_eq!(ir.stream, Some(true));
        // reasoning.effort medium → thinking budget for an Anthropic upstream
        assert_eq!(ir.enable_thinking, Some(true));
        assert_eq!(ir.thinking_budget, Some(4096));

        // The crate encodes the IR to a real Anthropic Messages body — proves the reused half
        // works end-to-end (responses client → anthropic upstream).
        use llm_connector::core::Protocol;
        use llm_connector::protocols::adapters::anthropic::AnthropicProtocol;
        let body = AnthropicProtocol::new("").build_chat_request_body(&ir).unwrap();
        let msgs = body.get("messages").and_then(|v| v.as_array()).unwrap();
        // assistant turn carries a tool_use block; tool output became a user tool_result turn
        assert!(msgs.iter().any(|m| m["role"] == "assistant"
            && m["content"].as_array().unwrap().iter().any(|b| b["type"] == "tool_use" && b["id"] == "call_1")));
        assert!(msgs.iter().any(|m| m["role"] == "user"
            && m["content"].as_array().unwrap().iter().any(|b| b["type"] == "tool_result" && b["tool_use_id"] == "call_1")));
        assert_eq!(body["system"], "You are Codex.");
    }

    #[test]
    fn encodes_ir_to_responses_response_with_tool_calls() {
        // A chat upstream reply with prose + a tool call → the Responses body Codex consumes.
        use llm_connector::core::Protocol;
        let chat = r#"{
            "id":"chatcmpl-9","object":"chat.completion","created":1,"model":"gpt-4o",
            "choices":[{"index":0,"finish_reason":"tool_calls","message":{
                "role":"assistant","content":"Checking.",
                "tool_calls":[{"id":"call_9","type":"function",
                    "function":{"name":"shell","arguments":"{\"command\":[\"ls\"]}"}}]}}],
            "usage":{"prompt_tokens":11,"completion_tokens":7,"total_tokens":18}
        }"#;
        let ir = OpenAIProtocol::new("").parse_response(chat).unwrap();
        let out = encode_response(&ir, "z-ai/glm-5.2");
        assert_eq!(out["object"], "response");
        assert_eq!(out["status"], "completed");
        assert_eq!(out["model"], "z-ai/glm-5.2");
        assert_eq!(out["usage"]["input_tokens"], 11);
        assert_eq!(out["usage"]["output_tokens"], 7);
        assert_eq!(out["usage"]["total_tokens"], 18);
        let output = out["output"].as_array().unwrap();
        let m = output.iter().find(|i| i["type"] == "message").unwrap();
        assert_eq!(m["content"][0]["type"], "output_text");
        assert_eq!(m["content"][0]["text"], "Checking.");
        let fc = output.iter().find(|i| i["type"] == "function_call").unwrap();
        assert_eq!(fc["call_id"], "call_9");
        assert_eq!(fc["name"], "shell");
        assert_eq!(fc["arguments"], "{\"command\":[\"ls\"]}");
    }

    #[test]
    fn synthesized_responses_sse_carries_items_and_completed() {
        use llm_connector::core::Protocol;
        let chat = r#"{
            "id":"c1","object":"chat.completion","created":1,"model":"up",
            "choices":[{"index":0,"finish_reason":"tool_calls","message":{
                "role":"assistant","content":"On it.",
                "tool_calls":[{"id":"call_2","type":"function",
                    "function":{"name":"apply_patch","arguments":"{\"p\":1}"}}]}}],
            "usage":{"prompt_tokens":5,"completion_tokens":3,"total_tokens":8}
        }"#;
        let ir = OpenAIProtocol::new("").parse_response(chat).unwrap();
        let sse = encode_response_sse(&ir, "alias-model");
        // ordered: created → message item events → function_call item events → completed
        let created = sse.find("\"type\":\"response.created\"").unwrap();
        let item_done = sse.find("response.output_item.done").unwrap();
        let completed = sse.find("\"type\":\"response.completed\"").unwrap();
        assert!(created < item_done && item_done < completed);
        // Codex reads items exclusively from output_item.done: both items must appear there.
        assert!(sse.contains(r#""delta":"On it.""#));
        assert!(sse.contains(r#""call_id":"call_2""#));
        assert!(sse.contains(r#""name":"apply_patch""#));
        assert!(sse.contains(r#""arguments":"{\"p\":1}""#));
        // completed carries id + usage (codex errors without them)
        assert!(sse.contains(r#""input_tokens":5"#));
        assert!(sse.contains(r#""output_tokens":3"#));
        assert!(sse.contains(r#""id":"resp_c1""#));
    }
}
