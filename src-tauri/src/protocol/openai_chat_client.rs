// OpenAI Chat CLIENT-side codec (P4 reverse direction): when an OpenAI/Codex-style client hits the
// gateway at /v1/chat/completions and the provider is Anthropic, we decode the client's Chat request
// into the IR and re-encode the IR response back to Chat Completions shape. The Anthropic upstream
// side is handled by the crate's AnthropicProtocol.

use llm_connector::types::{
    ChatRequest, ChatResponse, FunctionCall, Message, MessageBlock, Role, Tool, ToolCall,
};
use serde_json::{json, Value};

fn content_to_blocks(content: &Value) -> Vec<MessageBlock> {
    if let Some(s) = content.as_str() {
        return if s.is_empty() { vec![] } else { vec![MessageBlock::text(s)] };
    }
    let arr = match content.as_array() {
        Some(a) => a,
        None => return vec![],
    };
    let mut out = vec![];
    for part in arr {
        match part.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                    out.push(MessageBlock::text(t));
                }
            }
            Some("image_url") => {
                if let Some(u) = part.get("image_url").and_then(|i| i.get("url")).and_then(|v| v.as_str()) {
                    out.push(MessageBlock::image_url(u));
                }
            }
            _ => {}
        }
    }
    out
}

/// Decode an OpenAI Chat Completions REQUEST json into the IR.
pub fn decode_request(req: &Value) -> Result<ChatRequest, String> {
    let model = req.get("model").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let mut messages: Vec<Message> = vec![];
    for m in req.get("messages").and_then(|v| v.as_array()).cloned().unwrap_or_default().iter() {
        let role = match m.get("role").and_then(|v| v.as_str()) {
            Some("system") | Some("developer") => Role::System,
            Some("assistant") => Role::Assistant,
            Some("tool") => Role::Tool,
            _ => Role::User,
        };
        let content = m.get("content").cloned().unwrap_or(Value::Null);
        let mut msg = Message::new(role, content_to_blocks(&content));
        if let Some(name) = m.get("name").and_then(|v| v.as_str()) {
            msg.name = Some(name.to_string());
        }
        if let Some(tcid) = m.get("tool_call_id").and_then(|v| v.as_str()) {
            msg.tool_call_id = Some(tcid.to_string());
        }
        if let Some(tcs) = m.get("tool_calls").and_then(|v| v.as_array()) {
            let calls: Vec<ToolCall> = tcs
                .iter()
                .map(|tc| ToolCall {
                    id: tc.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name: tc.get("function").and_then(|f| f.get("name")).and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        arguments: tc.get("function").and_then(|f| f.get("arguments")).and_then(|v| v.as_str()).unwrap_or("{}").to_string(),
                        thought_signature: None,
                    },
                    index: None,
                    thought_signature: None,
                })
                .collect();
            if !calls.is_empty() {
                msg.tool_calls = Some(calls);
            }
        }
        messages.push(msg);
    }

    let mut cr = ChatRequest::new(model).with_messages(messages);
    if let Some(mt) = req.get("max_tokens").and_then(|v| v.as_u64()) {
        cr = cr.with_max_tokens(mt as u32);
    }
    if let Some(mt) = req.get("max_completion_tokens").and_then(|v| v.as_u64()) {
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
    if let Some(tools) = req.get("tools").and_then(|v| v.as_array()) {
        let ts: Vec<Tool> = tools
            .iter()
            .filter_map(|t| {
                let f = t.get("function")?;
                let name = f.get("name").and_then(|v| v.as_str())?;
                let desc = f.get("description").and_then(|v| v.as_str()).map(|s| s.to_string());
                let params = f.get("parameters").cloned().unwrap_or_else(|| json!({ "type": "object" }));
                Some(Tool::function(name, desc, params))
            })
            .collect();
        if !ts.is_empty() {
            cr = cr.with_tools(ts);
        }
    }
    Ok(cr)
}

/// IR ChatResponse → OpenAI Chat Completions RESPONSE json.
pub fn encode_response(resp: &ChatResponse, client_model: &str) -> Value {
    let choice = resp.choices.first();
    let msg = choice.map(|c| &c.message);
    let text = {
        let t = msg.map(|m| m.content_as_text()).unwrap_or_default();
        if t.is_empty() { resp.content.clone() } else { t }
    };
    let mut message = json!({ "role": "assistant", "content": if text.is_empty() { Value::Null } else { json!(text) } });
    // Normalize the finish reason to OpenAI vocabulary (the IR may carry an Anthropic stop_reason
    // when the upstream was Anthropic).
    let mut finish = match choice.and_then(|c| c.finish_reason.as_deref()) {
        Some("end_turn") | Some("stop") | None => "stop",
        Some("max_tokens") | Some("length") => "length",
        Some("tool_use") | Some("tool_calls") => "tool_calls",
        Some(other) => other,
    }
    .to_string();
    if let Some(m) = msg {
        if let Some(calls) = &m.tool_calls {
            if !calls.is_empty() {
                let tcs: Vec<Value> = calls
                    .iter()
                    .enumerate()
                    .map(|(i, tc)| json!({
                        "index": i,
                        "id": if tc.id.is_empty() { format!("call_{}", i) } else { tc.id.clone() },
                        "type": "function",
                        "function": { "name": tc.function.name, "arguments": tc.function.arguments },
                    }))
                    .collect();
                message["tool_calls"] = json!(tcs);
                finish = "tool_calls".to_string();
            }
        }
    }
    let usage = resp.usage.as_ref();
    json!({
        "id": if resp.id.is_empty() { "chatcmpl-ccbud".to_string() } else { resp.id.clone() },
        "object": "chat.completion",
        "created": 0,
        "model": client_model,
        "choices": [{ "index": 0, "finish_reason": finish, "message": message }],
        "usage": {
            "prompt_tokens": usage.map(|u| u.prompt_tokens).unwrap_or(0),
            "completion_tokens": usage.map(|u| u.completion_tokens).unwrap_or(0),
            "total_tokens": usage.map(|u| u.total_tokens).unwrap_or(0),
        }
    })
}

/// IR ChatResponse → OpenAI Chat SSE stream (buffered synthesize: role chunk, content chunk(s),
/// tool_call chunk(s), final finish chunk, `[DONE]`).
pub fn encode_response_sse(resp: &ChatResponse, client_model: &str) -> String {
    let full = encode_response(resp, client_model);
    let choice = &full["choices"][0];
    let message = &choice["message"];
    let finish = choice.get("finish_reason").and_then(|v| v.as_str()).unwrap_or("stop");
    let id = full.get("id").cloned().unwrap_or(json!("chatcmpl-ccbud"));
    let chunk = |delta: Value, fin: Value| {
        format!(
            "data: {}\n\n",
            serde_json::to_string(&json!({
                "id": id, "object": "chat.completion.chunk", "created": 0, "model": client_model,
                "choices": [{ "index": 0, "delta": delta, "finish_reason": fin }],
            })).unwrap_or_default()
        )
    };
    let mut out = String::new();
    out.push_str(&chunk(json!({ "role": "assistant" }), Value::Null));
    if let Some(t) = message.get("content").and_then(|v| v.as_str()) {
        if !t.is_empty() {
            out.push_str(&chunk(json!({ "content": t }), Value::Null));
        }
    }
    if let Some(tcs) = message.get("tool_calls").and_then(|v| v.as_array()) {
        out.push_str(&chunk(json!({ "tool_calls": tcs }), Value::Null));
    }
    out.push_str(&chunk(json!({}), json!(finish)));
    out.push_str("data: [DONE]\n\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_connector::core::Protocol;
    use llm_connector::protocols::adapters::anthropic::AnthropicProtocol;

    #[test]
    fn chat_request_to_ir_to_anthropic_upstream() {
        let chat = json!({
            "model": "gpt-x", "max_tokens": 200,
            "messages": [
                { "role": "system", "content": "be nice" },
                { "role": "user", "content": "hello" }
            ],
            "tools": [{ "type": "function", "function": { "name": "f", "description": "d", "parameters": { "type": "object" } } }]
        });
        let ir = decode_request(&chat).unwrap();
        assert_eq!(ir.messages[0].content_as_text(), "be nice");
        assert_eq!(ir.messages[1].content_as_text(), "hello");
        assert_eq!(ir.tools.as_ref().unwrap()[0].function.name, "f");
        // crate encodes IR → Anthropic upstream request (the reverse direction's upstream half)
        let body = AnthropicProtocol::new("").build_chat_request_body(&ir).unwrap();
        assert!(body.get("messages").is_some());
    }

    #[test]
    fn anthropic_reply_to_ir_to_chat_response() {
        // crate decodes an Anthropic response → IR; we encode IR → Chat Completions for the client.
        let anthropic = r#"{"id":"msg_1","type":"message","role":"assistant","model":"claude",
            "content":[{"type":"text","text":"done"}],"stop_reason":"end_turn",
            "usage":{"input_tokens":9,"output_tokens":4}}"#;
        let ir = AnthropicProtocol::new("").parse_response(anthropic).unwrap();
        let out = encode_response(&ir, "gpt-x");
        assert_eq!(out["object"], "chat.completion");
        assert_eq!(out["model"], "gpt-x");
        assert_eq!(out["choices"][0]["message"]["content"], "done");
        assert_eq!(out["choices"][0]["finish_reason"], "stop");
        assert_eq!(out["usage"]["prompt_tokens"], 9);
        assert_eq!(out["usage"]["completion_tokens"], 4);
    }
}
