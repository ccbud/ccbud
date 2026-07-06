// The "Anthropic server-side" halves that llm-connector (a client library) doesn't provide:
//   - decode_request:  Anthropic Messages REQUEST json  → llm-connector ChatRequest IR
//   - encode_response: llm-connector ChatResponse IR     → Anthropic Messages RESPONSE json
//
// Mapping follows the same shape LiteLLM / musistudio use: Anthropic content blocks are flattened
// into the OpenAI-style IR — `tool_use` blocks become assistant `Message.tool_calls`, `tool_result`
// blocks become separate `role:tool` messages, `system` becomes a leading system message. The IR is
// then encoded to OpenAI Chat (or Responses) by the crate. The reverse rebuilds Anthropic content
// blocks from the IR's tool_calls + text.
//
// Claude Code footguns handled explicitly (LiteLLM shipped bugs on these): user/system content
// blocks arrive as `{"type":"input_text"}` (not `text`) and MUST be recognized, else content is
// silently dropped → upstream 422.

use llm_connector::types::{
    ChatRequest, ChatResponse, FunctionCall, Message, MessageBlock, Role, Tool, ToolCall,
};
use serde_json::{json, Value};

/// Pull plain text out of an Anthropic content value (string, or array of text/input_text blocks).
fn blocks_text(content: &Value) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    let arr = match content.as_array() {
        Some(a) => a,
        None => return String::new(),
    };
    let mut out: Vec<String> = vec![];
    for b in arr {
        match b.get("type").and_then(|t| t.as_str()) {
            // Claude Code sends `input_text`; the Anthropic API also uses `text`. Accept both.
            Some("text") | Some("input_text") => {
                if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                    out.push(t.to_string());
                }
            }
            _ => {}
        }
    }
    out.join("\n")
}

/// Image blocks in an Anthropic content array → IR image blocks (base64 or url).
fn image_blocks(content: &Value) -> Vec<MessageBlock> {
    let arr = match content.as_array() {
        Some(a) => a,
        None => return vec![],
    };
    let mut out = vec![];
    for b in arr {
        if b.get("type").and_then(|t| t.as_str()) != Some("image") {
            continue;
        }
        let src = b.get("source").cloned().unwrap_or(Value::Null);
        match src.get("type").and_then(|t| t.as_str()) {
            Some("base64") => {
                let mt = src.get("media_type").and_then(|v| v.as_str()).unwrap_or("image/png");
                let data = src.get("data").and_then(|v| v.as_str()).unwrap_or("");
                if !data.is_empty() {
                    out.push(MessageBlock::image_base64(mt, data));
                }
            }
            Some("url") => {
                if let Some(u) = src.get("url").and_then(|v| v.as_str()) {
                    out.push(MessageBlock::image_url_anthropic(u));
                }
            }
            _ => {}
        }
    }
    out
}

/// tool_result blocks in a user turn → their own `role:tool` IR messages (OpenAI shape). Anthropic
/// nests tool results inside a user message; OpenAI wants each as a standalone tool message.
fn tool_result_messages(content: &Value) -> Vec<Message> {
    let arr = match content.as_array() {
        Some(a) => a,
        None => return vec![],
    };
    let mut out = vec![];
    for b in arr {
        if b.get("type").and_then(|t| t.as_str()) != Some("tool_result") {
            continue;
        }
        let id = b.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        // tool_result content is a string or an array of text blocks.
        let text = match b.get("content") {
            Some(Value::String(s)) => s.clone(),
            Some(c @ Value::Array(_)) => blocks_text(c),
            _ => String::new(),
        };
        out.push(Message::tool(text, id));
    }
    out
}

/// tool_use blocks in an assistant turn → IR ToolCalls (OpenAI function-call shape).
fn tool_use_calls(content: &Value) -> Vec<ToolCall> {
    let arr = match content.as_array() {
        Some(a) => a,
        None => return vec![],
    };
    let mut out = vec![];
    for b in arr {
        if b.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
            continue;
        }
        let id = b.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let name = b.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let args = b.get("input").cloned().unwrap_or_else(|| json!({}));
        out.push(ToolCall {
            id,
            call_type: "function".to_string(),
            function: FunctionCall {
                name,
                arguments: serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string()),
                thought_signature: None,
            },
            index: None,
            thought_signature: None,
        });
    }
    out
}

/// Anthropic `system` (string or array of text blocks) → a leading system Message.
fn system_message(req: &Value) -> Option<Message> {
    let sys = req.get("system")?;
    let text = if sys.is_string() { sys.as_str().unwrap_or("").to_string() } else { blocks_text(sys) };
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(Message::text(Role::System, text))
    }
}

/// Anthropic `tools` → IR function tools. `input_schema` maps to the function `parameters`.
fn tools(req: &Value) -> Option<Vec<Tool>> {
    let arr = req.get("tools").and_then(|v| v.as_array())?;
    let mut out = vec![];
    for t in arr {
        let name = t.get("name").and_then(|v| v.as_str())?;
        let desc = t.get("description").and_then(|v| v.as_str()).map(|s| s.to_string());
        let params = t.get("input_schema").cloned().unwrap_or_else(|| json!({ "type": "object" }));
        out.push(Tool::function(name, desc, params));
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Decode an Anthropic Messages REQUEST json into the llm-connector IR. `model` is left as the
/// request's model (gateway.rs already rewrote it to the provider's outgoing model before this).
pub fn decode_request(req: &Value) -> Result<ChatRequest, String> {
    let model = req.get("model").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let mut messages: Vec<Message> = vec![];

    if let Some(sys) = system_message(req) {
        messages.push(sys);
    }

    let turns = req.get("messages").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    for m in &turns {
        let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("user");
        let content = m.get("content").cloned().unwrap_or(Value::Null);
        match role {
            "assistant" => {
                // assistant turn: text (+ optional thinking) + tool_use → tool_calls
                let mut blocks: Vec<MessageBlock> = vec![];
                let text = blocks_text(&content);
                if !text.is_empty() {
                    blocks.push(MessageBlock::text(text));
                }
                let calls = tool_use_calls(&content);
                let mut msg = Message::new(Role::Assistant, blocks);
                if !calls.is_empty() {
                    msg.tool_calls = Some(calls);
                }
                messages.push(msg);
            }
            _ => {
                // user turn: tool_result blocks split off into their own tool messages FIRST
                // (they answer the prior assistant tool_calls), then any remaining text/images.
                for tm in tool_result_messages(&content) {
                    messages.push(tm);
                }
                let mut blocks: Vec<MessageBlock> = vec![];
                let text = blocks_text(&content);
                if !text.is_empty() {
                    blocks.push(MessageBlock::text(text));
                }
                blocks.extend(image_blocks(&content));
                if !blocks.is_empty() {
                    messages.push(Message::new(Role::User, blocks));
                }
            }
        }
    }

    let mut cr = ChatRequest::new(model).with_messages(messages);
    if let Some(mt) = req.get("max_tokens").and_then(|v| v.as_u64()) {
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
    if let Some(stop) = req.get("stop_sequences").and_then(|v| v.as_array()) {
        let v: Vec<String> = stop.iter().filter_map(|s| s.as_str().map(|x| x.to_string())).collect();
        if !v.is_empty() {
            cr = cr.with_stop(v);
        }
    }
    if let Some(ts) = tools(req) {
        cr = cr.with_tools(ts);
    }
    // Anthropic extended thinking → IR thinking budget (+ enable). Downstream OpenAI-chat drops it;
    // Responses maps the budget to reasoning.effort (handled in the responses codec).
    if let Some(th) = req.get("thinking") {
        let enabled = th.get("type").and_then(|v| v.as_str()) == Some("enabled");
        if enabled {
            cr = cr.with_enable_thinking(true);
            if let Some(b) = th.get("budget_tokens").and_then(|v| v.as_u64()) {
                cr = cr.with_thinking_budget(b as u32);
            }
        }
    }

    Ok(cr)
}

/// Map an OpenAI/IR finish_reason to an Anthropic stop_reason.
fn stop_reason(finish: Option<&str>, had_tool_calls: bool) -> &'static str {
    match finish {
        Some("length") => "max_tokens",
        Some("tool_calls") | Some("function_call") => "tool_use",
        Some("content_filter") => "end_turn",
        _ if had_tool_calls => "tool_use",
        _ => "end_turn",
    }
}

/// Encode the IR response back into an Anthropic Messages RESPONSE json. `client_model` is the name
/// the client asked for (so Claude Code sees its own model, not the upstream's).
pub fn encode_response(resp: &ChatResponse, client_model: &str) -> Value {
    let choice = resp.choices.first();
    let msg = choice.map(|c| &c.message);

    let mut content: Vec<Value> = vec![];
    // assistant thinking (if the provider surfaced reasoning) → an Anthropic thinking block first.
    if let Some(m) = msg {
        if let Some(reasoning) = m.reasoning_any() {
            if !reasoning.trim().is_empty() {
                content.push(json!({ "type": "thinking", "thinking": reasoning }));
            }
        }
    }
    // assistant text. The crate parks text in choices[].message.content normally, but when a turn
    // ALSO has tool_calls it keeps the text only in the top-level ChatResponse.content — so fall
    // back to that (else assistant prose is dropped whenever a tool is called in the same turn).
    let text = {
        let t = msg.map(|m| m.content_as_text()).unwrap_or_default();
        if t.is_empty() { resp.content.clone() } else { t }
    };
    if !text.is_empty() {
        content.push(json!({ "type": "text", "text": text }));
    }
    // tool calls → tool_use blocks
    let mut had_tool_calls = false;
    if let Some(m) = msg {
        if let Some(calls) = &m.tool_calls {
            for tc in calls {
                had_tool_calls = true;
                let input: Value = tc.arguments_value().unwrap_or_else(|_| json!({}));
                content.push(json!({
                    "type": "tool_use",
                    "id": if tc.id.is_empty() { format!("toolu_{}", content.len()) } else { tc.id.clone() },
                    "name": tc.function.name,
                    "input": input,
                }));
            }
        }
    }
    if content.is_empty() {
        content.push(json!({ "type": "text", "text": "" }));
    }

    let finish = choice.and_then(|c| c.finish_reason.as_deref());
    let usage = resp.usage.as_ref();
    let input_tokens = usage.map(|u| u.prompt_tokens).unwrap_or(0);
    let output_tokens = usage.map(|u| u.completion_tokens).unwrap_or(0);

    json!({
        // never a constant fallback — clients persist this id and usage de-dupes by it
        "id": if resp.id.is_empty() { super::uid("msg_ccbud") } else { resp.id.clone() },
        "type": "message",
        "role": "assistant",
        "model": client_model,
        "content": content,
        "stop_reason": stop_reason(finish, had_tool_calls),
        "stop_sequence": Value::Null,
        "usage": { "input_tokens": input_tokens, "output_tokens": output_tokens },
    })
}

/// Synthesize a complete Anthropic Messages SSE event sequence from a finished IR response. Used
/// when the client (Claude Code) asked to stream but the upstream was translated buffered — the
/// client still gets a valid, ordered `message_start → content_block_* → message_delta →
/// message_stop` stream, just delivered at once. True token-by-token transcoding is P2.
pub fn encode_response_sse(resp: &ChatResponse, client_model: &str) -> String {
    let full = encode_response(resp, client_model);
    let content = full.get("content").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let stop_reason = full.get("stop_reason").cloned().unwrap_or(json!("end_turn"));
    let usage = full.get("usage").cloned().unwrap_or(json!({ "input_tokens": 0, "output_tokens": 0 }));
    let id = full.get("id").cloned().unwrap_or(json!("msg_ccbud"));
    let input_tokens = usage.get("input_tokens").cloned().unwrap_or(json!(0));
    let output_tokens = usage.get("output_tokens").cloned().unwrap_or(json!(0));

    let ev = |event: &str, data: Value| {
        format!("event: {}\ndata: {}\n\n", event, serde_json::to_string(&data).unwrap_or_default())
    };
    let mut out = String::new();

    // message_start (usage input tokens known up front; output filled at message_delta)
    out.push_str(&ev(
        "message_start",
        json!({ "type": "message_start", "message": {
            "id": id, "type": "message", "role": "assistant", "model": client_model,
            "content": [], "stop_reason": Value::Null, "stop_sequence": Value::Null,
            "usage": { "input_tokens": input_tokens, "output_tokens": 0 },
        }}),
    ));

    for (i, block) in content.iter().enumerate() {
        let bt = block.get("type").and_then(|v| v.as_str()).unwrap_or("text");
        match bt {
            "text" => {
                let text = block.get("text").and_then(|v| v.as_str()).unwrap_or("");
                out.push_str(&ev("content_block_start", json!({ "type": "content_block_start", "index": i, "content_block": { "type": "text", "text": "" } })));
                if !text.is_empty() {
                    out.push_str(&ev("content_block_delta", json!({ "type": "content_block_delta", "index": i, "delta": { "type": "text_delta", "text": text } })));
                }
                out.push_str(&ev("content_block_stop", json!({ "type": "content_block_stop", "index": i })));
            }
            "thinking" => {
                let think = block.get("thinking").and_then(|v| v.as_str()).unwrap_or("");
                out.push_str(&ev("content_block_start", json!({ "type": "content_block_start", "index": i, "content_block": { "type": "thinking", "thinking": "" } })));
                if !think.is_empty() {
                    out.push_str(&ev("content_block_delta", json!({ "type": "content_block_delta", "index": i, "delta": { "type": "thinking_delta", "thinking": think } })));
                }
                out.push_str(&ev("content_block_stop", json!({ "type": "content_block_stop", "index": i })));
            }
            "tool_use" => {
                let empty = json!({});
                let input = block.get("input").unwrap_or(&empty);
                out.push_str(&ev("content_block_start", json!({ "type": "content_block_start", "index": i, "content_block": { "type": "tool_use", "id": block.get("id").cloned().unwrap_or(json!("")), "name": block.get("name").cloned().unwrap_or(json!("")), "input": {} } })));
                out.push_str(&ev("content_block_delta", json!({ "type": "content_block_delta", "index": i, "delta": { "type": "input_json_delta", "partial_json": serde_json::to_string(input).unwrap_or_else(|_| "{}".to_string()) } })));
                out.push_str(&ev("content_block_stop", json!({ "type": "content_block_stop", "index": i })));
            }
            _ => {}
        }
    }

    out.push_str(&ev(
        "message_delta",
        json!({ "type": "message_delta", "delta": { "stop_reason": stop_reason, "stop_sequence": Value::Null }, "usage": { "output_tokens": output_tokens } }),
    ));
    out.push_str(&ev("message_stop", json!({ "type": "message_stop" })));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_connector::core::Protocol;
    use llm_connector::protocols::adapters::openai::OpenAIProtocol;

    // A representative Claude Code request: system + a user prose turn (input_text blocks), an
    // assistant tool_use, and the user's tool_result — the shape the messages→chat path must map.
    fn claude_request() -> Value {
        json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 1024,
            "system": "You are a helpful coding assistant.",
            "tools": [{ "name": "read_file", "description": "Read a file",
                        "input_schema": { "type": "object", "properties": { "path": { "type": "string" } } } }],
            "messages": [
                { "role": "user", "content": [{ "type": "input_text", "text": "read a.txt" }] },
                { "role": "assistant", "content": [
                    { "type": "text", "text": "Reading it." },
                    { "type": "tool_use", "id": "toolu_1", "name": "read_file", "input": { "path": "a.txt" } }
                ] },
                { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "toolu_1", "content": "hello world" }
                ] }
            ]
        })
    }

    #[test]
    fn decodes_anthropic_request_to_openai_chat_body() {
        let ir = decode_request(&claude_request()).unwrap();
        // system prepended, tool_result split into its own tool message, ordering preserved.
        let roles: Vec<_> = ir.messages.iter().map(|m| format!("{:?}", m.role)).collect();
        assert_eq!(roles, vec!["System", "User", "Assistant", "Tool"]);
        // input_text was recognized (not dropped → this is the Claude Code footgun).
        assert_eq!(ir.messages[1].content_as_text(), "read a.txt");
        // assistant tool_use → tool_calls
        let calls = ir.messages[2].tool_calls.as_ref().unwrap();
        assert_eq!(calls[0].function.name, "read_file");
        assert!(calls[0].function.arguments.contains("a.txt"));
        // tool_result → tool message carrying the id + output
        assert_eq!(ir.messages[3].tool_call_id.as_deref(), Some("toolu_1"));
        assert_eq!(ir.messages[3].content_as_text(), "hello world");
        // tools carried through
        assert_eq!(ir.tools.as_ref().unwrap()[0].function.name, "read_file");

        // The crate encodes the IR to a real OpenAI Chat body — proves the reused half works.
        let body = OpenAIProtocol::new("k").build_chat_request_body(&ir).unwrap();
        let msgs = body.get("messages").and_then(|v| v.as_array()).unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert!(body.get("tools").is_some());
    }

    #[test]
    fn encodes_openai_chat_response_to_anthropic() {
        // A real OpenAI Chat response with a tool call, decoded by the crate → IR → Anthropic.
        let openai = r#"{
            "id":"chatcmpl-1","object":"chat.completion","created":1,"model":"gpt-4o",
            "choices":[{"index":0,"finish_reason":"tool_calls","message":{
                "role":"assistant","content":"Sure.",
                "tool_calls":[{"id":"call_9","type":"function",
                    "function":{"name":"read_file","arguments":"{\"path\":\"a.txt\"}"}}]}}],
            "usage":{"prompt_tokens":11,"completion_tokens":7,"total_tokens":18}
        }"#;
        let ir = OpenAIProtocol::new("k").parse_response(openai).unwrap();
        let out = encode_response(&ir, "claude-sonnet-4-6");

        assert_eq!(out["type"], "message");
        assert_eq!(out["role"], "assistant");
        assert_eq!(out["model"], "claude-sonnet-4-6"); // client-facing model, not gpt-4o
        assert_eq!(out["stop_reason"], "tool_use");
        assert_eq!(out["usage"]["input_tokens"], 11);
        assert_eq!(out["usage"]["output_tokens"], 7);
        let content = out["content"].as_array().unwrap();
        assert!(content.iter().any(|b| b["type"] == "text" && b["text"] == "Sure."));
        let tu = content.iter().find(|b| b["type"] == "tool_use").unwrap();
        assert_eq!(tu["name"], "read_file");
        assert_eq!(tu["input"]["path"], "a.txt");
        assert_eq!(tu["id"], "call_9");
    }

    #[test]
    fn plain_text_round_trip() {
        let req = json!({
            "model": "claude-x", "max_tokens": 100,
            "messages": [{ "role": "user", "content": "hi there" }]
        });
        let ir = decode_request(&req).unwrap();
        assert_eq!(ir.messages.len(), 1);
        assert_eq!(ir.messages[0].content_as_text(), "hi there");

        let openai = r#"{"id":"c1","object":"chat.completion","created":1,"model":"gpt","choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":"hello!"}}],"usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}}"#;
        let ir2 = OpenAIProtocol::new("k").parse_response(openai).unwrap();
        let out = encode_response(&ir2, "claude-x");
        assert_eq!(out["stop_reason"], "end_turn");
        assert_eq!(out["content"][0]["text"], "hello!");
    }
}
