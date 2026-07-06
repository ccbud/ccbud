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

    /// The upstream path segment for this provider protocol, appended to the provider baseUrl.
    pub fn upstream_path(self) -> &'static str {
        match self {
            Wire::Anthropic => "/v1/messages",
            Wire::OpenAiChat => "/v1/chat/completions",
            Wire::OpenAiResponses => "/v1/responses",
        }
    }

    /// Full upstream URL for a translated request, joining the provider baseUrl with this
    /// protocol's endpoint WITHOUT doubling the version prefix — a baseUrl that already ends in
    /// `/v1` (OpenAI/NVIDIA convention) gets the bare `/chat/completions`, one without gets the
    /// full `/v1/chat/completions`.
    pub fn upstream_url(self, base_url: &str) -> String {
        let base = base_url.trim_end_matches('/');
        let bare = match self {
            Wire::Anthropic => "/messages",
            Wire::OpenAiChat => "/chat/completions",
            Wire::OpenAiResponses => "/responses",
        };
        if base.ends_with("/v1") {
            format!("{}{}", base, bare)
        } else {
            format!("{}{}", base, self.upstream_path())
        }
    }
}

use llm_connector::core::Protocol;
use llm_connector::protocols::adapters::anthropic::AnthropicProtocol;
use llm_connector::protocols::adapters::openai::OpenAIProtocol;
use llm_connector::types::{ChatRequest, ChatResponse};
use serde_json::Value;

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
        Wire::OpenAiChat => OpenAIProtocol::new("")
            .build_chat_request_body(&ir)
            .map_err(|e| e.to_string()),
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
        Wire::OpenAiChat => OpenAIProtocol::new("").parse_response(text).map_err(|e| e.to_string()),
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
