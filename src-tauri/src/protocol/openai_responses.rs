// OpenAI Responses (/v1/responses) codec (P3). Encodes the unified IR into a Responses REQUEST
// body, and decodes a Responses RESPONSE back into the IR.
//
// The Responses API uses an item-based `input` array (role messages + function_call /
// function_call_output items), `instructions` for the system prompt, `max_output_tokens`, and a
// `reasoning.effort` knob. Its response is an `output` array of items. We build the request JSON
// directly, and decode by re-shaping the Responses reply into an OpenAI Chat completion so the
// crate's mature `parse_response` builds the IR (no hand-rolled ChatResponse).

use llm_connector::core::Protocol;
use llm_connector::protocols::adapters::openai::OpenAIProtocol;
use llm_connector::types::{ChatRequest, ChatResponse, Role};
use serde_json::{json, Value};

/// Map a thinking budget (tokens) to a Responses reasoning effort tier.
fn budget_to_effort(budget: Option<u32>) -> &'static str {
    match budget {
        Some(b) if b >= 8192 => "high",
        Some(b) if b >= 2048 => "medium",
        _ => "low",
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
}
