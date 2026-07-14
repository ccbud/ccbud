// Protocol translation between the three LLM API wire formats:
//   - Anthropic Messages   (/v1/messages)          — what Claude Code speaks
//   - OpenAI Chat           (/v1/chat/completions)
//   - OpenAI Responses      (/v1/responses)
//
// Engine: the `llm-connector` crate provides a mature unified IR (ChatRequest / ChatResponse),
// OpenAI+Responses request encoders (build_chat_request_body / build_responses_request), and
// per-protocol response decoders (parse_response / …). llm-connector is a CLIENT library, so it
// lacks the two "Anthropic server-side" halves a Claude-Code-facing gateway needs — those live
// here (anthropic.rs): the Anthropic REQUEST → IR decoder and the IR → Anthropic RESPONSE encoder.
//
// Direction for a request = decode(client wire) → IR → encode(provider wire). The identity case
// (client and provider speak the same protocol) never enters this module — gateway.rs keeps its
// verbatim passthrough fast path for it, so existing Anthropic→Anthropic behavior is unchanged.

#![allow(dead_code)]

pub mod anthropic;
pub mod openai_chat_client;
pub mod openai_responses;
pub mod stream;

use axum::http::Uri;

/// A wire protocol a request or provider speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    Anthropic,
    OpenAiChat,
    OpenAiResponses,
}

impl Wire {
    /// The provider's declared protocol (config `protocol` field). Unknown / absent → Anthropic,
    /// which is today's passthrough default.
    pub fn from_provider(s: Option<&str>) -> Wire {
        match s {
            Some("openai-chat") => Wire::OpenAiChat,
            Some("openai-responses") => Wire::OpenAiResponses,
            _ => Wire::Anthropic,
        }
    }

    /// The client's protocol, inferred from the inbound request path. Claude Code hits
    /// `/v1/messages`; an OpenAI/Codex client hits `/v1/chat/completions` or `/v1/responses`.
    pub fn from_request_path(uri: &Uri) -> Wire {
        let p = uri.path();
        if p.ends_with("/responses") || p.ends_with("/responses/") {
            Wire::OpenAiResponses
        } else if p.contains("/chat/completions") {
            Wire::OpenAiChat
        } else {
            Wire::Anthropic
        }
    }

    /// Short human label for exchange records / monitor UI.
    pub fn label(self) -> &'static str {
        match self {
            Wire::Anthropic => "anthropic",
            Wire::OpenAiChat => "openai-chat",
            Wire::OpenAiResponses => "openai-responses",
        }
    }

    /// The upstream path segment for this provider protocol, appended to the provider baseUrl.
    pub fn upstream_path(self) -> &'static str {
        match self {
            Wire::Anthropic => "/v1/messages",
            Wire::OpenAiChat => "/v1/chat/completions",
            Wire::OpenAiResponses => "/v1/responses",
        }
    }

    /// Full upstream URL for a translated request, joining the provider baseUrl with this
    /// protocol's endpoint WITHOUT doubling the version prefix — OpenAI-style roots that already
    /// end in `/v1`, or Google's compatibility root ending in `/openai`, get the bare endpoint.
    pub fn upstream_url(self, base_url: &str) -> String {
        let base = base_url.trim_end_matches('/');
        let bare = match self {
            Wire::Anthropic => "/messages",
            Wire::OpenAiChat => "/chat/completions",
            Wire::OpenAiResponses => "/responses",
        };
        if base.ends_with("/v1") || (self == Wire::OpenAiChat && base.ends_with("/openai")) {
            format!("{}{}", base, bare)
        } else {
            format!("{}{}", base, self.upstream_path())
        }
    }
}

use llm_connector::core::Protocol;
use llm_connector::protocols::adapters::anthropic::AnthropicProtocol;
use llm_connector::protocols::adapters::openai::OpenAIProtocol;
use llm_connector::types::{ChatRequest, ChatResponse, ToolCall};
use serde_json::{json, Value};

/// Unique id for a synthesized response ("msg_ccbud_<ms>_<n>"). Clients persist these ids into
/// their history, and usage analytics de-dupes assistant messages BY id — a constant fallback id
/// would collapse every translated turn into a single counted request.
pub fn uid(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{}_{}_{}", prefix, ms, N.fetch_add(1, Ordering::Relaxed))
}

/// Extract Gemini's opaque thought signature from its OpenAI-compatible wire location, or from
/// an internal/native spelling encountered while translating. The canonical OpenAI compatibility
/// shape is `extra_content.google.thought_signature`.
pub(crate) fn json_thought_signature(value: &Value) -> Option<String> {
    value
        .pointer("/extra_content/google/thought_signature")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| value.get("thought_signature").and_then(Value::as_str).filter(|s| !s.is_empty()))
        .or_else(|| value.pointer("/function/thought_signature").and_then(Value::as_str).filter(|s| !s.is_empty()))
        .map(str::to_string)
}

/// Read the signature from the llm-connector IR. The crate supports both placements for native
/// Gemini, so accept either while keeping a single canonical wire representation at the edge.
pub(crate) fn tool_call_thought_signature(call: &ToolCall) -> Option<String> {
    call.thought_signature
        .as_deref()
        .filter(|s| !s.is_empty())
        .or_else(|| call.function.thought_signature.as_deref().filter(|s| !s.is_empty()))
        .map(str::to_string)
}

fn strip_internal_thought_signature(call: &mut Value) {
    let Some(call_obj) = call.as_object_mut() else { return };
    call_obj.remove("thought_signature");
    if let Some(function) = call_obj.get_mut("function").and_then(Value::as_object_mut) {
        function.remove("thought_signature");
    }
}

fn set_google_thought_signature(call: &mut Value, signature: &str) {
    strip_internal_thought_signature(call);
    call["extra_content"]["google"]["thought_signature"] = json!(signature);
}

/// llm-connector serializes its internal signature fields literally. Rewrite them into Gemini's
/// OpenAI-compatible `extra_content.google.thought_signature` before forwarding.
fn normalize_openai_request_thought_signatures(body: &mut Value) {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else { return };
    for message in messages {
        let Some(calls) = message.get_mut("tool_calls").and_then(Value::as_array_mut) else { continue };
        for call in calls {
            if let Some(signature) = json_thought_signature(call) {
                set_google_thought_signature(call, &signature);
            }
        }
    }
}

/// Gemini/OpenRouter/Cloudflare return provider metadata in `extra_content`, which serde ignores
/// when llm-connector parses a standard OpenAI ToolCall. Copy the opaque signature into the
/// crate's internal field before parsing; the original response remains otherwise unchanged.
fn normalize_openai_response_thought_signatures(body: &mut Value) {
    let Some(choices) = body.get_mut("choices").and_then(Value::as_array_mut) else { return };
    for choice in choices {
        let Some(calls) = choice
            .get_mut("message")
            .and_then(|message| message.get_mut("tool_calls"))
            .and_then(Value::as_array_mut)
        else { continue };
        for call in calls {
            if let Some(signature) = json_thought_signature(call) {
                call["thought_signature"] = json!(signature);
            }
        }
    }
}

/// Decode an inbound client request (in its wire format) into the unified IR.
pub fn decode_client_request(client: Wire, body: &Value) -> Result<ChatRequest, String> {
    match client {
        Wire::Anthropic => anthropic::decode_request(body),
        Wire::OpenAiChat => openai_chat_client::decode_request(body),
        // Hand-rolled (not the crate's responses_request_to_chat_request, which drops
        // function_call / function_call_output / assistant items and rejects flattened tools —
        // fatal for Codex).
        Wire::OpenAiResponses => openai_responses::decode_request(body),
    }
}

/// Encode the IR into the upstream provider's request BODY. `outgoing_model` is the provider's real
/// model (gateway already resolved it); `stream` requests SSE from the upstream. For the first cut
/// we translate cross-protocol responses buffered, so callers pass stream=false here and synthesize
/// the client SSE from the full response (true incremental transcoding is P2).
pub fn encode_upstream_request(
    provider: Wire,
    ir: &ChatRequest,
    outgoing_model: &str,
    stream: bool,
) -> Result<Value, String> {
    let mut ir = ir.clone();
    ir.model = outgoing_model.to_string();
    ir.stream = Some(stream);
    match provider {
        Wire::OpenAiChat => {
            let mut body = OpenAIProtocol::new("")
                .build_chat_request_body(&ir)
                .map_err(|e| e.to_string())?;
            if outgoing_model.to_ascii_lowercase().contains("gemini") {
                normalize_openai_request_thought_signatures(&mut body);
            }
            Ok(body)
        }
        Wire::OpenAiResponses => Ok(openai_responses::encode_request(&ir, outgoing_model, stream)),
        // Reverse direction: an OpenAI/Codex client → an Anthropic upstream. The crate encodes the
        // IR into an Anthropic Messages request (tool_calls→tool_use blocks, etc.). Anthropic
        // requires max_tokens; OpenAI-family clients (Codex) usually omit it and the crate's
        // fallback (1024) truncates agent turns — default to a workable ceiling instead.
        Wire::Anthropic => {
            if ir.max_tokens.is_none() {
                ir.max_tokens = Some(8192);
            }
            AnthropicProtocol::new("")
                .build_chat_request_body(&ir)
                .map_err(|e| e.to_string())
        }
    }
}

/// Decode an upstream provider RESPONSE (its wire format, buffered) into the IR.
pub fn decode_upstream_response(provider: Wire, text: &str) -> Result<ChatResponse, String> {
    match provider {
        Wire::OpenAiChat => {
            let normalized = match serde_json::from_str::<Value>(text) {
                Ok(mut body) => {
                    normalize_openai_response_thought_signatures(&mut body);
                    body.to_string()
                }
                Err(_) => text.to_string(),
            };
            OpenAIProtocol::new("").parse_response(&normalized).map_err(|e| e.to_string())
        }
        Wire::OpenAiResponses => openai_responses::decode_response(text),
        Wire::Anthropic => AnthropicProtocol::new("").parse_response(text).map_err(|e| e.to_string()),
    }
}

/// Encode the IR response back to the client's wire format as a buffered JSON body.
pub fn encode_client_response(client: Wire, ir: &ChatResponse, client_model: &str) -> Result<Value, String> {
    match client {
        Wire::Anthropic => Ok(anthropic::encode_response(ir, client_model)),
        Wire::OpenAiChat => Ok(openai_chat_client::encode_response(ir, client_model)),
        // Hand-rolled (not the crate's chat_response_to_responses_response, which drops
        // tool_calls from the output — Codex would never see a function call).
        Wire::OpenAiResponses => Ok(openai_responses::encode_response(ir, client_model)),
    }
}

/// Whether we have an incremental (event-by-event) stream transcoder from `provider` to `client`.
/// When false, cross-protocol streaming falls back to buffer-upstream + synthesize-client-SSE.
pub fn can_transcode_stream(provider: Wire, client: Wire) -> bool {
    stream::Transcoder::supports(provider, client)
}

/// Encode the IR response to the client's wire format as a full SSE stream body (used when the
/// client asked to stream but we translated the upstream buffered — synthesize the event sequence).
pub fn encode_client_response_sse(client: Wire, ir: &ChatResponse, client_model: &str) -> Result<String, String> {
    match client {
        Wire::Anthropic => Ok(anthropic::encode_response_sse(ir, client_model)),
        Wire::OpenAiChat => Ok(openai_chat_client::encode_response_sse(ir, client_model)),
        Wire::OpenAiResponses => Ok(openai_responses::encode_response_sse(ir, client_model)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn google_openai_root_gets_the_bare_chat_endpoint() {
        assert_eq!(
            Wire::OpenAiChat.upstream_url("https://generativelanguage.googleapis.com/v1beta/openai"),
            "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions"
        );
    }

    #[test]
    fn gemini_thought_signature_maps_between_openai_wire_and_ir() {
        let signature = "sig-regression-abc";
        let upstream_response = json!({
            "id": "chatcmpl-gemini", "object": "chat.completion", "created": 1,
            "model": "google/gemini-3-flash-preview",
            "choices": [{ "index": 0, "finish_reason": "tool_calls", "message": {
                "role": "assistant", "content": Value::Null,
                "tool_calls": [{
                    "id": "default_api:Bash", "type": "function",
                    "function": { "name": "default_api:Bash", "arguments": "{\"command\":\"pwd\"}" },
                    "extra_content": { "google": { "thought_signature": signature } }
                }]
            }}],
            "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
        });

        let ir = decode_upstream_response(Wire::OpenAiChat, &upstream_response.to_string()).unwrap();
        let call = &ir.choices[0].message.tool_calls.as_ref().unwrap()[0];
        assert_eq!(tool_call_thought_signature(call).as_deref(), Some(signature));
        assert_eq!(json_thought_signature(&json!({
            "thought_signature": "", "function": { "thought_signature": signature }
        })).as_deref(), Some(signature));

        let mut message = llm_connector::types::Message::new(
            llm_connector::types::Role::Assistant,
            vec![],
        );
        message.tool_calls = Some(vec![call.clone()]);
        let next_ir = ChatRequest::new("gemini").with_messages(vec![message]);
        let outgoing = encode_upstream_request(
            Wire::OpenAiChat, &next_ir, "google/gemini-3-flash-preview", false,
        ).unwrap();
        let assistant = outgoing["messages"].as_array().unwrap().iter()
            .find(|message| message["role"] == "assistant").unwrap();
        let outgoing_call = &assistant["tool_calls"][0];
        assert_eq!(outgoing_call["extra_content"]["google"]["thought_signature"], signature);
        assert!(outgoing_call.get("thought_signature").is_none());
        assert!(outgoing_call["function"].get("thought_signature").is_none());
    }
}
