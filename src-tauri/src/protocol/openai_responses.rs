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
    ChatRequest, ChatResponse, FunctionCall, Message, MessageBlock, Role, Tool, ToolCall,
    ToolChoice,
};
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use std::collections::{HashMap, HashSet};

const CUSTOM_TOOL_INPUT_FIELD: &str = "input";
const CUSTOM_TOOL_RAW_INPUT_INSTRUCTION: &str =
    "Pass the custom tool's raw input unchanged in the `input` string field.";
const APPLY_PATCH_CHAT_INSTRUCTION: &str = "For apply_patch, the first line must be `*** Begin Patch` and the final line must be an unprefixed `*** End Patch`. Exact Add File skeleton:\n*** Begin Patch\n*** Add File: path\n+content\n*** End Patch\nPrefix every added file-content line with `+`, but never prefix either boundary marker. For updates, use `*** Update File: path` with an `@@` context hunk and ` `, `-`, or `+` line prefixes; for deletion, use `*** Delete File: path`.";
const TOOL_SEARCH_CHAT_NAME: &str = "tool_search";
const CHAT_TOOL_NAME_MAX_LEN: usize = 64;
const CHAT_TOOL_NAME_HASH_LEN: usize = 12;

/// The Responses tool shape that a chat-compatible upstream is standing in for.
///
/// OpenAI Chat only has flat JSON-schema functions, while current Codex requests also carry
/// freeform custom tools, tool search, and namespace tools. The translation layer flattens all of
/// them to chat functions, then uses this metadata to restore the exact Responses item type on the
/// way back to Codex.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CodexToolKind {
    Function,
    Namespace,
    Custom,
    ToolSearch,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CodexToolSpec {
    pub kind: CodexToolKind,
    pub name: String,
    pub namespace: Option<String>,
}

/// Request-scoped tool metadata used by both buffered and streaming Responses encoders.
///
/// Build this from the original Codex request before decoding it to the connector IR. Loaded tools
/// embedded in `tool_search_output` history are included, so subsequent calls can be translated
/// even when their definitions are not repeated in the top-level `tools` array.
#[derive(Clone, Debug, Default)]
pub struct CodexToolContext {
    ir_tools: Vec<Tool>,
    seen_chat_names: HashSet<String>,
    colliding_preferred_names: HashSet<String>,
    chat_name_to_spec: HashMap<String, CodexToolSpec>,
    spec_to_chat_name: HashMap<CodexToolSpec, String>,
}

impl CodexToolContext {
    pub fn from_request(req: &Value) -> Self {
        let mut context = Self::default();
        if let Some(tools) = req.get("tools").and_then(Value::as_array) {
            for tool in tools {
                context.add_response_tool(tool);
            }
        }
        if let Some(input) = req.get("input") {
            collect_tool_search_output_tools(input, &mut context);
            collect_response_tool_call_identities(input, &mut context);
        }
        collect_tool_choice_identity(req.get("tool_choice"), &mut context);
        context
    }

    pub fn ir_tools(&self) -> Vec<Tool> {
        self.ir_tools.clone()
    }

    pub fn lookup_chat_name(&self, chat_name: &str) -> Option<&CodexToolSpec> {
        self.chat_name_to_spec.get(chat_name)
    }

    pub fn kind_for_chat_name(&self, chat_name: &str) -> CodexToolKind {
        self.lookup_chat_name(chat_name)
            .map(|spec| spec.kind)
            .unwrap_or(CodexToolKind::Function)
    }

    pub fn chat_name_for_response_tool(&self, name: &str, namespace: Option<&str>) -> String {
        let namespace = namespace.filter(|value| !value.is_empty());
        self.chat_name_for_spec(&CodexToolSpec {
            kind: if namespace.is_some() {
                CodexToolKind::Namespace
            } else {
                CodexToolKind::Function
            },
            name: name.to_string(),
            namespace: namespace.map(ToString::to_string),
        })
    }

    fn chat_name_for_custom_tool(&self, name: &str) -> String {
        self.chat_name_for_spec(&CodexToolSpec {
            kind: CodexToolKind::Custom,
            name: name.to_string(),
            namespace: None,
        })
    }

    fn chat_name_for_tool_search(&self) -> String {
        self.chat_name_for_spec(&CodexToolSpec {
            kind: CodexToolKind::ToolSearch,
            name: TOOL_SEARCH_CHAT_NAME.to_string(),
            namespace: None,
        })
    }

    fn chat_name_for_spec(&self, spec: &CodexToolSpec) -> String {
        self.spec_to_chat_name
            .get(spec)
            .cloned()
            .unwrap_or_else(|| self.allocate_chat_name(spec))
    }

    pub(crate) fn response_item_id(
        &self,
        chat_name: &str,
        response_id: &str,
        index: usize,
    ) -> String {
        let prefix = match self.kind_for_chat_name(chat_name) {
            CodexToolKind::Custom => "ctc",
            CodexToolKind::ToolSearch => "tsc",
            CodexToolKind::Function | CodexToolKind::Namespace => "fc",
        };
        format!(
            "{}_{}_{}",
            prefix,
            response_id.trim_start_matches("resp_"),
            index
        )
    }

    pub(crate) fn response_tool_item(
        &self,
        item_id: &str,
        status: &str,
        call_id: &str,
        chat_name: &str,
        arguments: &str,
    ) -> Value {
        self.response_tool_item_with_reasoning(item_id, status, call_id, chat_name, arguments, None)
    }

    pub(crate) fn response_tool_item_with_reasoning(
        &self,
        item_id: &str,
        status: &str,
        call_id: &str,
        chat_name: &str,
        arguments: &str,
        reasoning: Option<&str>,
    ) -> Value {
        let mut item = match self.lookup_chat_name(chat_name) {
            Some(spec) if spec.kind == CodexToolKind::Custom => json!({
                "type": "custom_tool_call",
                "id": item_id,
                "status": status,
                "call_id": call_id,
                "name": spec.name,
                "input": custom_tool_input_from_chat_arguments(arguments),
            }),
            Some(spec) if spec.kind == CodexToolKind::ToolSearch => json!({
                "type": "tool_search_call",
                "status": status,
                "call_id": call_id,
                "execution": "client",
                "arguments": parse_tool_arguments_object(arguments),
            }),
            Some(spec) => {
                let mut item = json!({
                    "type": "function_call",
                    "id": item_id,
                    "status": status,
                    "call_id": call_id,
                    "name": spec.name,
                    "arguments": if arguments.is_empty() { "{}" } else { arguments },
                });
                if let Some(namespace) = spec.namespace.as_deref().filter(|value| !value.is_empty())
                {
                    item["namespace"] = json!(namespace);
                }
                item
            }
            None => json!({
                "type": "function_call",
                "id": item_id,
                "status": status,
                "call_id": call_id,
                "name": chat_name,
                "arguments": if arguments.is_empty() { "{}" } else { arguments },
            }),
        };
        if let Some(reasoning) = reasoning.map(str::trim).filter(|value| !value.is_empty()) {
            item["reasoning_content"] = json!(reasoning);
        }
        item
    }

    fn add_response_tool(&mut self, tool: &Value) {
        match tool {
            Value::String(name) => self.add_custom_tool(&json!({
                "type": "custom",
                "name": name,
            })),
            Value::Object(_) => match tool.get("type").and_then(Value::as_str) {
                Some("function") | None => self.add_function_tool(tool, None),
                Some("custom") => self.add_custom_tool(tool),
                Some("tool_search") => self.add_tool_search_tool(tool),
                Some("namespace") => self.add_namespace_tool(tool),
                _ => {}
            },
            _ => {}
        }
    }

    fn add_function_tool(&mut self, tool: &Value, namespace: Option<&str>) {
        let function = tool
            .get("function")
            .filter(|value| value.is_object())
            .unwrap_or(tool);
        let Some(name) = function.get("name").and_then(Value::as_str) else {
            return;
        };
        if name.trim().is_empty() {
            return;
        }
        let description = function
            .get("description")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        let parameters = normalize_function_parameters(function.get("parameters"));
        let spec = CodexToolSpec {
            kind: if namespace.is_some() {
                CodexToolKind::Namespace
            } else {
                CodexToolKind::Function
            },
            name: name.to_string(),
            namespace: namespace.map(ToString::to_string),
        };
        self.add_chat_tool(spec, description, parameters);
    }

    fn add_custom_tool(&mut self, tool: &Value) {
        let Some(name) = tool.get("name").and_then(Value::as_str) else {
            return;
        };
        if name.trim().is_empty() {
            return;
        }
        let mut description = tool
            .get("description")
            .and_then(Value::as_str)
            .map(|description| format!("{description}\n\n{CUSTOM_TOOL_RAW_INPUT_INSTRUCTION}"))
            .unwrap_or_else(|| CUSTOM_TOOL_RAW_INPUT_INSTRUCTION.to_string());
        if name == "apply_patch" {
            description.push_str("\n\n");
            description.push_str(APPLY_PATCH_CHAT_INSTRUCTION);
        }
        let parameters = json!({
            "type": "object",
            "properties": {
                "input": {
                    "type": "string",
                    "description": "Raw string input for the original custom tool. Preserve formatting exactly."
                }
            },
            "required": [CUSTOM_TOOL_INPUT_FIELD],
            "additionalProperties": false,
        });
        self.add_chat_tool(
            CodexToolSpec {
                kind: CodexToolKind::Custom,
                name: name.to_string(),
                namespace: None,
            },
            Some(description),
            parameters,
        );
    }

    fn add_tool_search_tool(&mut self, tool: &Value) {
        let description = tool
            .get("description")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .unwrap_or_else(|| {
                "Search and load Codex tools, plugins, connectors, and MCP namespaces for the current task."
                    .to_string()
            });
        let parameters = if tool.get("parameters").is_some_and(Value::is_object) {
            normalize_function_parameters(tool.get("parameters"))
        } else {
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "limit": { "type": "integer" }
                },
                "required": ["query"],
                "additionalProperties": false,
            })
        };
        self.add_chat_tool(
            CodexToolSpec {
                kind: CodexToolKind::ToolSearch,
                name: TOOL_SEARCH_CHAT_NAME.to_string(),
                namespace: None,
            },
            Some(description),
            parameters,
        );
    }

    fn add_namespace_tool(&mut self, tool: &Value) {
        let Some(namespace) = tool.get("name").and_then(Value::as_str) else {
            return;
        };
        if namespace.trim().is_empty() {
            return;
        }
        let Some(children) = tool
            .get("tools")
            .or_else(|| tool.get("children"))
            .and_then(Value::as_array)
        else {
            return;
        };
        for child in children {
            if child.get("type").and_then(Value::as_str) == Some("function") {
                self.add_function_tool(child, Some(namespace));
            }
        }
    }

    fn add_chat_tool(
        &mut self,
        spec: CodexToolSpec,
        description: Option<String>,
        parameters: Value,
    ) {
        if self.spec_to_chat_name.contains_key(&spec) {
            return;
        }
        let chat_name = self.reserve_chat_name(&spec);
        self.ir_tools
            .push(Tool::function(chat_name.clone(), description, parameters));
    }

    fn register_tool_identity(&mut self, spec: CodexToolSpec) {
        if self.spec_to_chat_name.contains_key(&spec) {
            return;
        }
        self.reserve_chat_name(&spec);
    }

    fn reserve_chat_name(&mut self, spec: &CodexToolSpec) -> String {
        let preferred = preferred_chat_tool_name(spec);
        let chat_name = if is_valid_chat_tool_name(&preferred)
            && !self.colliding_preferred_names.contains(&preferred)
        {
            if let Some(existing_spec) = self.chat_name_to_spec.get(&preferred).cloned() {
                self.colliding_preferred_names.insert(preferred.clone());
                if preferred_chat_tool_name(&existing_spec) == preferred {
                    self.move_identity_to_hashed_alias(&existing_spec, &preferred);
                }
                self.allocate_hashed_chat_name(spec)
            } else {
                preferred
            }
        } else {
            self.allocate_hashed_chat_name(spec)
        };
        self.seen_chat_names.insert(chat_name.clone());
        self.chat_name_to_spec
            .insert(chat_name.clone(), spec.clone());
        self.spec_to_chat_name
            .insert(spec.clone(), chat_name.clone());
        chat_name
    }

    fn move_identity_to_hashed_alias(&mut self, spec: &CodexToolSpec, old_name: &str) {
        self.seen_chat_names.remove(old_name);
        self.chat_name_to_spec.remove(old_name);
        let new_name = self.allocate_hashed_chat_name(spec);
        self.seen_chat_names.insert(new_name.clone());
        self.chat_name_to_spec
            .insert(new_name.clone(), spec.clone());
        self.spec_to_chat_name
            .insert(spec.clone(), new_name.clone());
        if let Some(tool) = self
            .ir_tools
            .iter_mut()
            .find(|tool| tool.function.name == old_name)
        {
            tool.function.name = new_name;
        }
    }

    fn allocate_chat_name(&self, spec: &CodexToolSpec) -> String {
        let preferred = preferred_chat_tool_name(spec);
        if is_valid_chat_tool_name(&preferred)
            && !self.colliding_preferred_names.contains(&preferred)
            && !self.seen_chat_names.contains(&preferred)
        {
            return preferred;
        }

        self.allocate_hashed_chat_name(spec)
    }

    fn allocate_hashed_chat_name(&self, spec: &CodexToolSpec) -> String {
        let preferred = preferred_chat_tool_name(spec);
        let digest = tool_identity_digest(spec);
        for attempt in 0_u64.. {
            let candidate = hashed_chat_tool_name(&preferred, &digest, attempt);
            if !self.seen_chat_names.contains(&candidate) {
                return candidate;
            }
        }
        unreachable!("the finite request cannot exhaust all valid Chat tool aliases")
    }
}

/// Client-visible call ids must be unique even when an OpenAI-compatible upstream repeats or
/// omits its own ids for parallel calls. Scope them to the response and output position; the
/// client echoes this id, so subsequent translated history remains unambiguous.
pub(crate) fn response_scoped_call_id(response_id: &str, index: usize) -> String {
    let mut digest = Sha1::new();
    digest.update(response_id.as_bytes());
    let digest = format!("{:x}", digest.finalize());
    format!("call_{}_{}", &digest[..16], index)
}

fn normalize_function_parameters(parameters: Option<&Value>) -> Value {
    let mut parameters = parameters
        .filter(|value| value.is_object())
        .cloned()
        .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
    if let Some(object) = parameters.as_object_mut() {
        if object.get("type").and_then(Value::as_str) != Some("object") {
            object.insert("type".to_string(), json!("object"));
        }
        object
            .entry("properties".to_string())
            .or_insert_with(|| json!({}));
    }
    parameters
}

fn preferred_chat_tool_name(spec: &CodexToolSpec) -> String {
    match spec.namespace.as_deref() {
        Some(namespace) if !namespace.is_empty() => format!("{namespace}__{}", spec.name),
        _ => spec.name.clone(),
    }
}

fn is_valid_chat_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= CHAT_TOOL_NAME_MAX_LEN
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn sanitized_chat_tool_name(name: &str) -> String {
    let mut sanitized = String::with_capacity(name.len().min(CHAT_TOOL_NAME_MAX_LEN));
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-') {
            sanitized.push(ch);
        } else {
            sanitized.push('_');
        }
    }
    if sanitized.is_empty() {
        sanitized.push_str("tool");
    }
    sanitized
}

fn tool_identity_digest(spec: &CodexToolSpec) -> String {
    let mut digest = Sha1::new();
    digest.update([match spec.kind {
        CodexToolKind::Function => 0,
        CodexToolKind::Namespace => 1,
        CodexToolKind::Custom => 2,
        CodexToolKind::ToolSearch => 3,
    }]);
    match spec.namespace.as_deref() {
        Some(namespace) => {
            digest.update([1]);
            digest.update((namespace.len() as u64).to_be_bytes());
            digest.update(namespace.as_bytes());
        }
        None => digest.update([0]),
    }
    digest.update((spec.name.len() as u64).to_be_bytes());
    digest.update(spec.name.as_bytes());
    format!("{:x}", digest.finalize())
}

fn hashed_chat_tool_name(preferred: &str, digest: &str, attempt: u64) -> String {
    let suffix = if attempt == 0 {
        format!("__{}", &digest[..CHAT_TOOL_NAME_HASH_LEN])
    } else {
        format!("__{}_{attempt}", &digest[..CHAT_TOOL_NAME_HASH_LEN])
    };
    let prefix_len = CHAT_TOOL_NAME_MAX_LEN.saturating_sub(suffix.len());
    let mut prefix = sanitized_chat_tool_name(preferred);
    prefix.truncate(prefix_len);
    format!("{prefix}{suffix}")
}

fn collect_tool_search_output_tools(value: &Value, context: &mut CodexToolContext) {
    match value {
        Value::Array(items) => {
            for item in items {
                collect_tool_search_output_tools(item, context);
            }
        }
        Value::Object(object) => {
            if object.get("type").and_then(Value::as_str) == Some("tool_search_output") {
                if let Some(tools) = object.get("tools").and_then(Value::as_array) {
                    for tool in tools {
                        context.add_response_tool(tool);
                    }
                }
            }
            for child in object.values() {
                collect_tool_search_output_tools(child, context);
            }
        }
        _ => {}
    }
}

fn collect_response_tool_call_identities(value: &Value, context: &mut CodexToolContext) {
    match value {
        Value::Array(items) => {
            for item in items {
                collect_response_tool_call_identities(item, context);
            }
        }
        Value::Object(object) => {
            let spec = match object.get("type").and_then(Value::as_str) {
                Some("function_call") => object
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.trim().is_empty())
                    .map(|name| {
                        let namespace = object
                            .get("namespace")
                            .and_then(Value::as_str)
                            .filter(|namespace| !namespace.is_empty());
                        CodexToolSpec {
                            kind: if namespace.is_some() {
                                CodexToolKind::Namespace
                            } else {
                                CodexToolKind::Function
                            },
                            name: name.to_string(),
                            namespace: namespace.map(ToString::to_string),
                        }
                    }),
                Some("custom_tool_call") => object
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.trim().is_empty())
                    .map(|name| CodexToolSpec {
                        kind: CodexToolKind::Custom,
                        name: name.to_string(),
                        namespace: None,
                    }),
                Some("tool_search_call") => Some(CodexToolSpec {
                    kind: CodexToolKind::ToolSearch,
                    name: TOOL_SEARCH_CHAT_NAME.to_string(),
                    namespace: None,
                }),
                _ => None,
            };
            if let Some(spec) = spec {
                context.register_tool_identity(spec);
            }
            for child in object.values() {
                collect_response_tool_call_identities(child, context);
            }
        }
        _ => {}
    }
}

fn collect_tool_choice_identity(tool_choice: Option<&Value>, context: &mut CodexToolContext) {
    let Some(tool_choice) = tool_choice.filter(|value| value.is_object()) else {
        return;
    };
    let spec = match tool_choice.get("type").and_then(Value::as_str) {
        Some("function") => tool_choice
            .get("name")
            .and_then(Value::as_str)
            .or_else(|| {
                tool_choice
                    .get("function")
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str)
            })
            .filter(|name| !name.trim().is_empty())
            .map(|name| {
                let namespace = tool_choice
                    .get("namespace")
                    .and_then(Value::as_str)
                    .filter(|namespace| !namespace.is_empty());
                CodexToolSpec {
                    kind: if namespace.is_some() {
                        CodexToolKind::Namespace
                    } else {
                        CodexToolKind::Function
                    },
                    name: name.to_string(),
                    namespace: namespace.map(ToString::to_string),
                }
            }),
        Some("custom") => tool_choice
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .map(|name| CodexToolSpec {
                kind: CodexToolKind::Custom,
                name: name.to_string(),
                namespace: None,
            }),
        Some("tool_search") => Some(CodexToolSpec {
            kind: CodexToolKind::ToolSearch,
            name: TOOL_SEARCH_CHAT_NAME.to_string(),
            namespace: None,
        }),
        _ => None,
    };
    if let Some(spec) = spec {
        context.register_tool_identity(spec);
    }
}

pub(crate) fn custom_tool_input_from_chat_arguments(arguments: &str) -> String {
    if arguments.trim().is_empty() {
        return String::new();
    }
    match serde_json::from_str::<Value>(arguments) {
        Ok(Value::Object(object)) => object
            .get(CUSTOM_TOOL_INPUT_FIELD)
            .and_then(Value::as_str)
            .unwrap_or(arguments)
            .to_string(),
        _ => arguments.to_string(),
    }
}

fn wrap_custom_tool_input(input: &Value) -> String {
    let input = input
        .as_str()
        .map(ToString::to_string)
        .unwrap_or_else(|| input.to_string());
    json!({ "input": input }).to_string()
}

fn parse_tool_arguments_object(arguments: &str) -> Value {
    if arguments.trim().is_empty() {
        return json!({});
    }
    serde_json::from_str::<Value>(arguments)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({ "query": arguments }))
}

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

fn response_incomplete_reason(finish_reason: Option<&str>) -> Option<&'static str> {
    match finish_reason {
        Some("length" | "max_tokens" | "model_context_window_exceeded") => {
            Some("max_output_tokens")
        }
        Some("content_filter") => Some("content_filter"),
        _ => None,
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
            .map(|t| {
                json!({
                    "type": "function",
                    "name": t.function.name,
                    "description": t.function.description,
                    "parameters": t.function.parameters,
                })
            })
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
    if v.get("status").and_then(Value::as_str) == Some("failed") {
        let message = v
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("upstream Responses request failed");
        return Err(message.to_string());
    }
    let output = v
        .get("output")
        .and_then(|o| o.as_array())
        .cloned()
        .unwrap_or_default();

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
    let finish_reason = if v.get("status").and_then(Value::as_str) == Some("incomplete") {
        match v
            .pointer("/incomplete_details/reason")
            .and_then(Value::as_str)
        {
            Some("content_filter") => "content_filter",
            _ => "length",
        }
    } else if had_tool {
        "tool_calls"
    } else {
        "stop"
    };
    let chat = json!({
        "id": v.get("id").cloned().unwrap_or(json!("resp")),
        "object": "chat.completion",
        "created": 0,
        "model": v.get("model").cloned().unwrap_or(json!("")),
        "choices": [{ "index": 0, "finish_reason": finish_reason, "message": message }],
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
        let url = p.get("image_url").and_then(|v| v.as_str()).or_else(|| {
            p.get("image_url")
                .and_then(|v| v.get("url"))
                .and_then(|v| v.as_str())
        });
        let Some(u) = url else { continue };
        if let Some(rest) = u.strip_prefix("data:") {
            if let Some((meta, data)) = rest.split_once(";base64,") {
                if !data.is_empty() {
                    out.push(MessageBlock::image_base64(
                        if meta.is_empty() { "image/png" } else { meta },
                        data,
                    ));
                }
                continue;
            }
        }
        out.push(MessageBlock::image_url(u));
    }
    out
}

/// Reasoning text carried by a Responses `reasoning` item: the summary parts (what a transcoded
/// stream emits and Codex echoes back), falling back to full `content` parts.
fn reasoning_item_text(item: &Value) -> Option<String> {
    for key in ["summary", "content"] {
        let Some(parts) = item.get(key).and_then(|v| v.as_array()) else {
            continue;
        };
        let text = parts
            .iter()
            .filter_map(|p| {
                p.get("text")
                    .and_then(|v| v.as_str())
                    .or_else(|| p.as_str())
            })
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        if !text.trim().is_empty() {
            return Some(text);
        }
    }
    None
}

/// Append reasoning text onto a message's `reasoning_content` (the OpenAI-chat wire field).
fn append_reasoning_content(message: &mut Message, text: &str) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    match &mut message.reasoning_content {
        // Transcoded Responses output deliberately carries the same reasoning both
        // as a sibling `reasoning` item and on each call item, so either surviving
        // history representation is sufficient. Do not multiply it when both (or
        // several parallel calls) are present.
        Some(existing) if existing.trim() == text => {}
        Some(existing) if !existing.is_empty() => {
            existing.push_str("\n\n");
            existing.push_str(text);
        }
        slot => *slot = Some(text.to_string()),
    }
}

fn response_item_call_id(item: &Value) -> Option<&str> {
    item.get("call_id")
        .or_else(|| item.get("id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResponseCallKind {
    Function,
    Custom,
    ToolSearch,
}

impl ResponseCallKind {
    fn call_item(item_type: &str) -> Option<Self> {
        match item_type {
            "function_call" => Some(Self::Function),
            "custom_tool_call" => Some(Self::Custom),
            "tool_search_call" => Some(Self::ToolSearch),
            _ => None,
        }
    }

    fn output_item(item_type: &str) -> Option<Self> {
        match item_type {
            "function_call_output" => Some(Self::Function),
            "custom_tool_call_output" => Some(Self::Custom),
            "tool_search_output" => Some(Self::ToolSearch),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Custom => "custom tool",
            Self::ToolSearch => "tool search",
        }
    }
}

fn validate_call_output_pairs(req: &Value) -> Result<(), String> {
    let items = match req.get("input") {
        Some(Value::Array(items)) => items.iter().collect::<Vec<_>>(),
        Some(Value::Object(_)) => req.get("input").into_iter().collect::<Vec<_>>(),
        _ => return Ok(()),
    };
    let mut calls = HashMap::<String, ResponseCallKind>::new();
    let mut seen_call_ids = HashSet::new();
    let mut unresolved = Vec::new();
    let mut consumed_in_group = false;
    for item in items {
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
        if item
            .get("role")
            .and_then(Value::as_str)
            .is_some_and(|role| matches!(role, "user" | "system" | "developer"))
        {
            // A new client-authored turn closes the window in which an older call can be
            // satisfied. Outputs after this point are stale/out of order.
            calls.clear();
            consumed_in_group = false;
            continue;
        }

        if let Some(kind) = ResponseCallKind::call_item(item_type) {
            let Some(call_id) = response_item_call_id(item) else {
                return Err("Responses call item is missing call_id".to_string());
            };
            if consumed_in_group && !calls.is_empty() {
                return Err(format!(
                    "Responses call order is ambiguous: new call {call_id} appeared before every preceding call produced an output"
                ));
            }
            if calls.is_empty() {
                consumed_in_group = false;
            }
            if !seen_call_ids.insert(call_id.to_string())
                || calls.insert(call_id.to_string(), kind).is_some()
            {
                return Err(format!(
                    "Responses call id is ambiguous because it appears more than once before output: {call_id}"
                ));
            }
            continue;
        }

        if let Some(output_kind) = ResponseCallKind::output_item(item_type) {
            match response_item_call_id(item) {
                Some(call_id) => match calls.remove(call_id) {
                    Some(call_kind) if call_kind == output_kind => {
                        consumed_in_group = true;
                    }
                    Some(call_kind) => unresolved.push(format!(
                        "{call_id} ({} output cannot satisfy {} call)",
                        output_kind.label(),
                        call_kind.label()
                    )),
                    None => {
                        if !unresolved.iter().any(|value| value == call_id) {
                            unresolved.push(call_id.to_string());
                        }
                    }
                },
                None => unresolved.push("<missing call_id>".to_string()),
            }
            if !unresolved.is_empty() {
                // Keep collecting only adjacent invalid outputs so the client gets useful ids,
                // but never let a later call retroactively legitimize an earlier output.
                consumed_in_group = true;
            }
            continue;
        }

        match item_type {
            // Reasoning and assistant output items can neighbor the same model turn and do not
            // make otherwise ordered call/output pairs stale.
            "reasoning" | "message" | "" => {}
            _ => {
                if item.get("role").is_some() {
                    calls.clear();
                    consumed_in_group = false;
                }
            }
        }
    }
    if unresolved.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Responses call output has no preceding matching call: {}",
            unresolved.join(", ")
        ))
    }
}

fn response_history_tool_call(item: &Value, context: &CodexToolContext) -> Option<ToolCall> {
    let ty = item.get("type").and_then(Value::as_str).unwrap_or("");
    let id = response_item_call_id(item).unwrap_or("").to_string();
    if id.is_empty() {
        return None;
    }
    let (name, arguments) = match ty {
        "function_call" => {
            let original_name = item.get("name").and_then(Value::as_str).unwrap_or("");
            let namespace = item.get("namespace").and_then(Value::as_str);
            let name = context.chat_name_for_response_tool(original_name, namespace);
            let arguments = match item.get("arguments") {
                Some(Value::String(arguments)) => arguments.clone(),
                Some(arguments) if !arguments.is_null() => arguments.to_string(),
                _ => "{}".to_string(),
            };
            (name, arguments)
        }
        "custom_tool_call" => {
            let original_name = item.get("name").and_then(Value::as_str).unwrap_or("");
            let name = context.chat_name_for_custom_tool(original_name);
            let input = item
                .get("input")
                .cloned()
                .unwrap_or(Value::String(String::new()));
            (name, wrap_custom_tool_input(&input))
        }
        "tool_search_call" => {
            let arguments = item
                .get("arguments")
                .map(|value| {
                    if let Some(arguments) = value.as_str() {
                        arguments.to_string()
                    } else {
                        value.to_string()
                    }
                })
                .unwrap_or_else(|| "{}".to_string());
            (context.chat_name_for_tool_search(), arguments)
        }
        _ => return None,
    };
    if name.is_empty() {
        return None;
    }
    Some(ToolCall {
        id,
        call_type: "function".to_string(),
        function: FunctionCall {
            name,
            arguments,
            thought_signature: None,
        },
        index: None,
        thought_signature: None,
    })
}

fn append_history_tool_call(
    messages: &mut Vec<Message>,
    pending_reasoning: &mut Option<String>,
    item_reasoning: Option<&str>,
    call: ToolCall,
) {
    // Codex emits a turn's prose and tool calls as sibling items. Fold the calls into the trailing
    // assistant message so Chat/Anthropic upstreams receive one coherent assistant turn.
    match messages.last_mut() {
        Some(message) if message.role == Role::Assistant => {
            if let Some(reasoning) = pending_reasoning.take() {
                append_reasoning_content(message, &reasoning);
            }
            if let Some(reasoning) = item_reasoning {
                append_reasoning_content(message, reasoning);
            }
            message.tool_calls.get_or_insert_with(Vec::new).push(call);
        }
        _ => {
            let mut message = Message::new(Role::Assistant, vec![]);
            if let Some(reasoning) = pending_reasoning.take() {
                append_reasoning_content(&mut message, &reasoning);
            }
            if let Some(reasoning) = item_reasoning {
                append_reasoning_content(&mut message, reasoning);
            }
            message.tool_calls = Some(vec![call]);
            messages.push(message);
        }
    }
}

fn response_tool_output_text(item: &Value) -> String {
    if item.get("type").and_then(Value::as_str) == Some("tool_search_output") {
        return json!({
            "status": item.get("status").cloned().unwrap_or(json!("completed")),
            "execution": item.get("execution").cloned().unwrap_or(json!("client")),
            "tools": item.get("tools").cloned().unwrap_or_else(|| json!([])),
        })
        .to_string();
    }
    match item.get("output") {
        Some(Value::String(output)) => output.clone(),
        Some(output @ Value::Array(_)) => {
            let text = parts_text(output);
            if text.is_empty() {
                output.to_string()
            } else {
                text
            }
        }
        Some(Value::Object(object)) => object
            .get("content")
            .map(|content| {
                let text = parts_text(content);
                if text.is_empty() {
                    content.to_string()
                } else {
                    text
                }
            })
            .unwrap_or_else(|| Value::Object(object.clone()).to_string()),
        _ => String::new(),
    }
}

/// Decode an OpenAI Responses REQUEST json (what Codex sends with wire_api="responses") into the
/// IR. Handles the full item vocabulary of an agentic history: message items (user input_text /
/// input_image, assistant output_text), all client-executed tool call/output item types, and `reasoning`
/// items — whose text is bridged onto the adjacent assistant message as `reasoning_content`,
/// because thinking chat upstreams (Kimi/Moonshot, DeepSeek, …) reject assistant tool-call
/// history that lost its reasoning. System/developer items collapse into ONE leading system
/// message: strict providers (MiniMax) reject `role:system` anywhere but the head. Custom,
/// tool-search, and namespace tools are flattened to chat functions and restored with the returned
/// [`CodexToolContext`].
pub fn decode_request(req: &Value) -> Result<ChatRequest, String> {
    decode_request_with_context(req).map(|(request, _)| request)
}

pub fn decode_request_with_context(req: &Value) -> Result<(ChatRequest, CodexToolContext), String> {
    validate_call_output_pairs(req)?;
    let model = req
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let tool_context = CodexToolContext::from_request(req);
    let mut messages: Vec<Message> = vec![];
    // All system text (instructions + system/developer message items), merged to the head.
    let mut system_texts: Vec<String> = vec![];
    // Reasoning waiting for the assistant message it belongs to (model output order is
    // reasoning → prose → tool calls, so reasoning usually precedes its assistant message).
    let mut pending_reasoning: Option<String> = None;

    if let Some(instr) = req.get("instructions").and_then(|v| v.as_str()) {
        if !instr.trim().is_empty() {
            system_texts.push(instr.to_string());
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
                let ty = item.get("type").and_then(|v| v.as_str()).unwrap_or(
                    if item.get("role").is_some() {
                        "message"
                    } else {
                        ""
                    },
                );
                match ty {
                    "message" => {
                        let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("user");
                        let content = item.get("content").cloned().unwrap_or(Value::Null);
                        let text = parts_text(&content);
                        match role {
                            "assistant" => {
                                if !text.is_empty() {
                                    let mut m = Message::text(Role::Assistant, text);
                                    if let Some(r) = pending_reasoning.take() {
                                        append_reasoning_content(&mut m, &r);
                                    }
                                    messages.push(m);
                                }
                            }
                            "system" | "developer" => {
                                pending_reasoning = None;
                                if !text.trim().is_empty() {
                                    system_texts.push(text);
                                }
                            }
                            _ => {
                                pending_reasoning = None;
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
                    "reasoning" => {
                        // Belongs to the assistant step it neighbors: fold backward onto a
                        // directly preceding assistant message, else hold for the next one.
                        if let Some(text) = reasoning_item_text(item) {
                            match messages.last_mut() {
                                Some(m) if m.role == Role::Assistant => {
                                    append_reasoning_content(m, &text)
                                }
                                _ => match &mut pending_reasoning {
                                    Some(existing) if !existing.is_empty() => {
                                        existing.push_str("\n\n");
                                        existing.push_str(text.trim());
                                    }
                                    slot => *slot = Some(text.trim().to_string()),
                                },
                            }
                        }
                    }
                    "function_call" | "custom_tool_call" | "tool_search_call" => {
                        if let Some(call) = response_history_tool_call(item, &tool_context) {
                            let item_reasoning = item
                                .get("reasoning_content")
                                .or_else(|| item.get("reasoning"))
                                .and_then(Value::as_str);
                            append_history_tool_call(
                                &mut messages,
                                &mut pending_reasoning,
                                item_reasoning,
                                call,
                            );
                        }
                    }
                    "function_call_output" | "custom_tool_call_output" | "tool_search_output" => {
                        pending_reasoning = None;
                        let id = response_item_call_id(item).unwrap_or("").to_string();
                        if !id.is_empty() {
                            messages.push(Message::tool(response_tool_output_text(item), id));
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }

    if !system_texts.is_empty() {
        messages.insert(0, Message::text(Role::System, system_texts.join("\n\n")));
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
    let tools = tool_context.ir_tools();
    if !tools.is_empty() {
        cr = cr.with_tools(tools);
    }
    // tool_choice: mode strings pass through; both the flattened Responses object form
    // ({type:"function",name}) and the nested Chat form pin a specific function.
    if let Some(tc) = req.get("tool_choice") {
        if let Some(mode) = tc.as_str() {
            if matches!(mode, "auto" | "none" | "required") {
                cr.tool_choice = Some(ToolChoice::Mode(mode.to_string()));
            }
        } else if let Some(kind) = tc.get("type").and_then(Value::as_str) {
            let selected = match kind {
                "function" => tc
                    .get("name")
                    .and_then(Value::as_str)
                    .or_else(|| {
                        tc.get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(Value::as_str)
                    })
                    .map(|name| {
                        tool_context.chat_name_for_response_tool(
                            name,
                            tc.get("namespace").and_then(Value::as_str),
                        )
                    }),
                "custom" => tc
                    .get("name")
                    .and_then(Value::as_str)
                    .map(|name| tool_context.chat_name_for_custom_tool(name)),
                "tool_search" => Some(tool_context.chat_name_for_tool_search()),
                _ => None,
            };
            if let Some(name) = selected {
                cr.tool_choice = Some(ToolChoice::function(name));
            }
        }
    }
    // reasoning.effort → IR thinking (an Anthropic upstream turns this into a thinking budget;
    // chat upstreams ignore it).
    if let Some(effort) = req
        .get("reasoning")
        .and_then(|r| r.get("effort"))
        .and_then(|v| v.as_str())
    {
        cr = cr
            .with_enable_thinking(true)
            .with_thinking_budget(effort_to_budget(effort));
    }
    Ok((cr, tool_context))
}

/// Encode the IR response back into an OpenAI Responses RESPONSE json. `client_model` is the name
/// the client asked for (so Codex sees its own model, not the upstream's). Unlike the crate's
/// chat_response_to_responses_response this maps tool_calls → function_call items and provider
/// reasoning → a reasoning item — both load-bearing for Codex's agent loop.
pub fn encode_response(resp: &ChatResponse, client_model: &str) -> Value {
    encode_response_with_context(resp, client_model, &CodexToolContext::default())
}

pub fn encode_response_with_context(
    resp: &ChatResponse,
    client_model: &str,
    tool_context: &CodexToolContext,
) -> Value {
    let choice = resp.choices.first();
    let msg = choice.map(|c| &c.message);
    // Same fallback as anthropic.rs: when a turn has tool_calls the crate parks the prose only in
    // the top-level ChatResponse.content.
    let text = {
        let t = msg.map(|m| m.content_as_text()).unwrap_or_default();
        if t.is_empty() {
            resp.content.clone()
        } else {
            t
        }
    };
    // never a constant fallback — item ids derive from this and land in client history
    let rid = if resp.id.is_empty() {
        super::uid("ccbud")
    } else {
        resp.id.clone()
    };

    let mut output: Vec<Value> = vec![];
    if let Some(reasoning) = msg.and_then(|m| m.reasoning_any()) {
        if !reasoning.trim().is_empty() {
            output.push(json!({ "type": "reasoning", "id": format!("rs_{}", rid),
                "summary": [{ "type": "summary_text", "text": reasoning }] }));
        }
    }
    if !text.is_empty() {
        output.push(
            json!({ "type": "message", "id": format!("msg_{}", rid), "status": "completed",
            "role": "assistant",
            "content": [{ "type": "output_text", "annotations": [], "text": text }] }),
        );
    }
    if let Some(m) = msg {
        if let Some(calls) = &m.tool_calls {
            for (i, tc) in calls.iter().enumerate() {
                let call_id = response_scoped_call_id(&format!("resp_{}", rid), i);
                let item_id = tool_context.response_item_id(&tc.function.name, &rid, i);
                output.push(tool_context.response_tool_item_with_reasoning(
                    &item_id,
                    "completed",
                    &call_id,
                    &tc.function.name,
                    &tc.function.arguments,
                    m.reasoning_any(),
                ));
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
    let total =
        (usage.map(|u| u.total_tokens).unwrap_or(0) as i64).max(input_tokens + output_tokens);
    let incomplete_reason =
        response_incomplete_reason(choice.and_then(|choice| choice.finish_reason.as_deref()));
    let mut response = json!({
        "id": format!("resp_{}", rid),
        "object": "response",
        "created_at": resp.created,
        "status": if incomplete_reason.is_some() { "incomplete" } else { "completed" },
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
    });
    if let Some(reason) = incomplete_reason {
        response["incomplete_details"] = json!({ "reason": reason });
    }
    response
}

fn sse_ev(data: &Value) -> String {
    let t = data
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("message");
    format!(
        "event: {}\ndata: {}\n\n",
        t,
        serde_json::to_string(data).unwrap_or_default()
    )
}

/// Synthesize an OpenAI Responses SSE event sequence from a finished IR response. Used when the
/// client (Codex) asked to stream but the upstream was translated buffered — the client still gets
/// a valid `response.created → output_item.added/delta/done per item → terminal event` stream, just
/// delivered at once. Codex materializes items only from `response.output_item.done`; truncations
/// terminate with `response.incomplete` instead of being mislabeled completed.
pub fn encode_response_sse(resp: &ChatResponse, client_model: &str) -> String {
    encode_response_sse_with_context(resp, client_model, &CodexToolContext::default())
}

pub fn encode_response_sse_with_context(
    resp: &ChatResponse,
    client_model: &str,
    tool_context: &CodexToolContext,
) -> String {
    let full = encode_response_with_context(resp, client_model, tool_context);
    let rid = full
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("resp_ccbud")
        .to_string();
    let mut out = String::new();
    out.push_str(&sse_ev(&json!({ "type": "response.created",
        "response": { "id": rid, "object": "response", "status": "in_progress", "model": client_model } })));

    let items = full
        .get("output")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for (idx, item) in items.iter().enumerate() {
        let item_id = item
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("item")
            .to_string();
        match item.get("type").and_then(|v| v.as_str()).unwrap_or("") {
            "message" => {
                let text = item["content"][0]["text"].as_str().unwrap_or("");
                out.push_str(&sse_ev(&json!({ "type": "response.output_item.added", "output_index": idx,
                    "item": { "type": "message", "id": item_id, "status": "in_progress", "role": "assistant", "content": [] } })));
                out.push_str(&sse_ev(
                    &json!({ "type": "response.content_part.added", "item_id": item_id,
                    "output_index": idx, "content_index": 0,
                    "part": { "type": "output_text", "annotations": [], "text": "" } }),
                ));
                if !text.is_empty() {
                    out.push_str(&sse_ev(
                        &json!({ "type": "response.output_text.delta", "item_id": item_id,
                        "output_index": idx, "content_index": 0, "delta": text }),
                    ));
                }
                out.push_str(&sse_ev(
                    &json!({ "type": "response.output_text.done", "item_id": item_id,
                    "output_index": idx, "content_index": 0, "text": text }),
                ));
                out.push_str(&sse_ev(
                    &json!({ "type": "response.content_part.done", "item_id": item_id,
                    "output_index": idx, "content_index": 0,
                    "part": { "type": "output_text", "annotations": [], "text": text } }),
                ));
                out.push_str(&sse_ev(&json!({ "type": "response.output_item.done", "output_index": idx, "item": item })));
            }
            "function_call" => {
                let args = item
                    .get("arguments")
                    .and_then(|v| v.as_str())
                    .unwrap_or("{}");
                let mut added = item.clone();
                added["status"] = json!("in_progress");
                added["arguments"] = json!("");
                out.push_str(&sse_ev(&json!({ "type": "response.output_item.added", "output_index": idx, "item": added })));
                out.push_str(&sse_ev(
                    &json!({ "type": "response.function_call_arguments.delta", "item_id": item_id,
                    "output_index": idx, "delta": args }),
                ));
                out.push_str(&sse_ev(
                    &json!({ "type": "response.function_call_arguments.done", "item_id": item_id,
                    "output_index": idx, "arguments": args }),
                ));
                out.push_str(&sse_ev(&json!({ "type": "response.output_item.done", "output_index": idx, "item": item })));
            }
            "custom_tool_call" => {
                let input = item.get("input").and_then(Value::as_str).unwrap_or("");
                let mut added = item.clone();
                added["status"] = json!("in_progress");
                added["input"] = json!("");
                out.push_str(&sse_ev(&json!({ "type": "response.output_item.added", "output_index": idx, "item": added })));
                if !input.is_empty() {
                    out.push_str(&sse_ev(&json!({ "type": "response.custom_tool_call_input.delta",
                        "item_id": item_id, "call_id": item.get("call_id").cloned().unwrap_or(json!("")),
                        "output_index": idx, "delta": input })));
                }
                out.push_str(&sse_ev(&json!({ "type": "response.custom_tool_call_input.done",
                    "item_id": item_id, "call_id": item.get("call_id").cloned().unwrap_or(json!("")),
                    "output_index": idx, "input": input })));
                out.push_str(&sse_ev(&json!({ "type": "response.output_item.done", "output_index": idx, "item": item })));
            }
            "tool_search_call" => {
                let mut added = item.clone();
                added["status"] = json!("in_progress");
                out.push_str(&sse_ev(&json!({ "type": "response.output_item.added", "output_index": idx, "item": added })));
                out.push_str(&sse_ev(&json!({ "type": "response.output_item.done", "output_index": idx, "item": item })));
            }
            "reasoning" => {
                let think = item["summary"][0]["text"].as_str().unwrap_or("");
                out.push_str(&sse_ev(
                    &json!({ "type": "response.output_item.added", "output_index": idx,
                    "item": { "type": "reasoning", "id": item_id, "summary": [] } }),
                ));
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

    let terminal_type = if full.get("status").and_then(Value::as_str) == Some("incomplete") {
        "response.incomplete"
    } else {
        "response.completed"
    };
    out.push_str(&sse_ev(&json!({ "type": terminal_type, "response": full })));
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
        assert!(input.iter().any(|i| i["type"] == "message"
            && i["role"] == "user"
            && i["content"][0]["type"] == "input_text"
            && i["content"][0]["text"] == "find foo"));
        let fc = input.iter().find(|i| i["type"] == "function_call").unwrap();
        assert_eq!(fc["name"], "grep");
        assert_eq!(fc["call_id"], "c1");
        let fco = input
            .iter()
            .find(|i| i["type"] == "function_call_output")
            .unwrap();
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
        assert!(content
            .iter()
            .any(|b| b["type"] == "text" && b["text"] == "Working on it."));
        let tu = content.iter().find(|b| b["type"] == "tool_use").unwrap();
        assert_eq!(tu["name"], "grep");
        assert_eq!(tu["input"]["q"], "foo");
    }

    // A representative Codex request (wire_api="responses"): instructions, flattened function
    // tools, and an agentic history — user message, assistant prose + function_call, its
    // function_call_output, and a reasoning item bridged onto the assistant turn.
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
        let roles: Vec<_> = ir
            .messages
            .iter()
            .map(|m| format!("{:?}", m.role))
            .collect();
        // instructions → System; assistant prose + function_call folded into ONE assistant turn;
        // function_call_output → Tool; reasoning item bridged onto the assistant turn.
        assert_eq!(roles, vec!["System", "User", "Assistant", "Tool", "User"]);
        assert_eq!(ir.messages[0].content_as_text(), "You are Codex.");
        assert_eq!(ir.messages[1].content_as_text(), "list files");
        assert_eq!(ir.messages[2].content_as_text(), "Running ls.");
        assert_eq!(
            ir.messages[2].reasoning_content.as_deref(),
            Some("thinking…")
        );
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
        let body = AnthropicProtocol::new("")
            .build_chat_request_body(&ir)
            .unwrap();
        let msgs = body.get("messages").and_then(|v| v.as_array()).unwrap();
        // assistant turn carries a tool_use block; tool output became a user tool_result turn
        assert!(msgs.iter().any(|m| m["role"] == "assistant"
            && m["content"]
                .as_array()
                .unwrap()
                .iter()
                .any(|b| b["type"] == "tool_use" && b["id"] == "call_1")));
        assert!(msgs.iter().any(|m| m["role"] == "user"
            && m["content"]
                .as_array()
                .unwrap()
                .iter()
                .any(|b| b["type"] == "tool_result" && b["tool_use_id"] == "call_1")));
        assert_eq!(body["system"], "You are Codex.");
    }

    #[test]
    fn rejects_call_outputs_without_a_preceding_matching_call() {
        for input in [
            json!([{
                "type":"function_call_output","call_id":"missing_call","output":"done"
            }]),
            json!([
                {"type":"custom_tool_call_output","call_id":"late_call","output":"done"},
                {"type":"custom_tool_call","call_id":"late_call","name":"apply_patch","input":"patch"}
            ]),
            json!([{"type":"tool_search_output","tools":[]}]),
            json!([
                {"type":"function_call","call_id":"duplicate_output","name":"shell","arguments":"{}"},
                {"type":"function_call_output","call_id":"duplicate_output","output":"one"},
                {"type":"function_call_output","call_id":"duplicate_output","output":"two"}
            ]),
            json!([
                {"type":"function_call","call_id":"wrong_kind","name":"shell","arguments":"{}"},
                {"type":"custom_tool_call_output","call_id":"wrong_kind","output":"done"}
            ]),
            json!([
                {"type":"function_call","call_id":"stale_call","name":"shell","arguments":"{}"},
                {"type":"message","role":"user","content":"start another turn"},
                {"type":"function_call_output","call_id":"stale_call","output":"done"}
            ]),
            json!([
                {"type":"function_call","call_id":"stale_bare","name":"shell","arguments":"{}"},
                {"role":"user","content":"start another turn"},
                {"type":"function_call_output","call_id":"stale_bare","output":"done"}
            ]),
        ] {
            let error = decode_request(&json!({ "model":"m", "input":input })).unwrap_err();
            assert!(
                error.contains("no preceding matching call"),
                "unexpected validation error: {error}"
            );
        }
    }

    #[test]
    fn rejects_ambiguous_duplicate_call_ids_and_interleaved_call_groups() {
        for input in [
            json!([
                {"type":"function_call","call_id":"same","name":"first","arguments":"{}"},
                {"type":"custom_tool_call","call_id":"same","name":"second","input":"x"}
            ]),
            json!([
                {"type":"function_call","call_id":"c1","name":"first","arguments":"{}"},
                {"type":"function_call","call_id":"c2","name":"second","arguments":"{}"},
                {"type":"function_call_output","call_id":"c1","output":"one"},
                {"type":"function_call","call_id":"c3","name":"third","arguments":"{}"}
            ]),
            json!([
                {"type":"function_call","call_id":"reused","name":"first","arguments":"{}"},
                {"type":"function_call_output","call_id":"reused","output":"one"},
                {"role":"user","content":"next turn"},
                {"type":"function_call","call_id":"reused","name":"second","arguments":"{}"}
            ]),
        ] {
            let error = decode_request(&json!({"model":"m","input":input})).unwrap_err();
            assert!(
                error.contains("ambiguous"),
                "unexpected validation error: {error}"
            );
        }
    }

    // Thinking chat upstreams reject tool-call history without reasoning, and MiniMax rejects
    // `role:system` anywhere but the head — the decoder must bridge reasoning items onto their
    // assistant turn and merge all system/developer text into one leading system message.
    #[test]
    fn bridges_reasoning_and_collapses_system_into_head() {
        let req = json!({
            "model": "m",
            "instructions": "You are Codex.",
            "input": [
                { "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "run ls" }] },
                { "type": "reasoning", "summary": [{ "type": "summary_text", "text": "need to list" }] },
                { "type": "function_call", "call_id": "c1", "name": "shell", "arguments": "{}" },
                { "type": "function_call_output", "call_id": "c1", "output": "a.txt" },
                { "type": "message", "role": "developer", "content": [{ "type": "input_text", "text": "be careful" }] },
                { "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "again" }] }
            ]
        });
        let ir = decode_request(&req).unwrap();
        let roles: Vec<_> = ir
            .messages
            .iter()
            .map(|m| format!("{:?}", m.role))
            .collect();
        // exactly ONE system message, at the head, carrying instructions + the developer item
        assert_eq!(roles, vec!["System", "User", "Assistant", "Tool", "User"]);
        assert_eq!(
            ir.messages[0].content_as_text(),
            "You are Codex.\n\nbe careful"
        );
        // the reasoning that produced the tool call rides the tool-call assistant turn
        assert_eq!(
            ir.messages[2].reasoning_content.as_deref(),
            Some("need to list")
        );
        assert_eq!(ir.messages[2].tool_calls.as_ref().unwrap()[0].id, "c1");
    }

    #[test]
    fn bridges_call_item_reasoning_without_duplicate_parallel_copies() {
        let req = json!({
            "model":"m",
            "input":[
                {"type":"reasoning","summary":[{"type":"summary_text","text":"inspect both"}]},
                {"type":"function_call","call_id":"c1","name":"first","arguments":"{}",
                 "reasoning_content":"inspect both"},
                {"type":"function_call","call_id":"c2","name":"second","arguments":"{}",
                 "reasoning_content":"inspect both"},
                {"type":"function_call_output","call_id":"c1","output":"one"},
                {"type":"function_call_output","call_id":"c2","output":"two"}
            ]
        });

        let ir = decode_request(&req).unwrap();
        assert_eq!(ir.messages[0].role, Role::Assistant);
        assert_eq!(
            ir.messages[0].reasoning_content.as_deref(),
            Some("inspect both")
        );
        assert_eq!(ir.messages[0].tool_calls.as_ref().unwrap().len(), 2);

        let item_only = json!({
            "model":"m",
            "input":[
                {"type":"function_call","call_id":"c3","name":"third","arguments":"{}",
                 "reasoning_content":"cached reasoning"},
                {"type":"function_call_output","call_id":"c3","output":"three"}
            ]
        });
        let ir = decode_request(&item_only).unwrap();
        assert_eq!(
            ir.messages[0].reasoning_content.as_deref(),
            Some("cached reasoning")
        );
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
        let fc = output
            .iter()
            .find(|i| i["type"] == "function_call")
            .unwrap();
        assert_eq!(
            fc["call_id"],
            response_scoped_call_id(out["id"].as_str().unwrap(), 0)
        );
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
        assert!(sse.contains(&format!(
            r#""call_id":"{}""#,
            response_scoped_call_id("resp_c1", 0)
        )));
        assert!(sse.contains(r#""name":"apply_patch""#));
        assert!(sse.contains(r#""arguments":"{\"p\":1}""#));
        // completed carries id + usage (codex errors without them)
        assert!(sse.contains(r#""input_tokens":5"#));
        assert!(sse.contains(r#""output_tokens":3"#));
        assert!(sse.contains(r#""id":"resp_c1""#));
    }

    #[test]
    fn buffered_duplicate_upstream_call_ids_become_unique_and_response_scoped() {
        use llm_connector::core::Protocol;

        let parse = |response_id: &str| {
            OpenAIProtocol::new("")
                .parse_response(
                    &json!({
                        "id":response_id,
                        "object":"chat.completion",
                        "created":1,
                        "model":"up",
                        "choices":[{"index":0,"finish_reason":"tool_calls","message":{
                            "role":"assistant","content":"",
                            "tool_calls":[
                                {"id":"same","type":"function","function":{"name":"first","arguments":"{}"}},
                                {"id":"same","type":"function","function":{"name":"second","arguments":"{}"}}
                            ]
                        }}]
                    })
                    .to_string(),
                )
                .unwrap()
        };
        let first = encode_response(&parse("turn-1"), "alias");
        let second = encode_response(&parse("turn-2"), "alias");
        let call_ids = |response: &Value| {
            response["output"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|item| item.get("call_id").and_then(Value::as_str))
                .map(str::to_string)
                .collect::<Vec<_>>()
        };
        let first_ids = call_ids(&first);
        let second_ids = call_ids(&second);

        assert_eq!(first_ids.len(), 2);
        assert_ne!(first_ids[0], first_ids[1]);
        assert_ne!(first_ids[0], second_ids[0]);
        assert_eq!(
            first_ids[0],
            response_scoped_call_id(first["id"].as_str().unwrap(), 0)
        );
        assert_eq!(
            first_ids[1],
            response_scoped_call_id(first["id"].as_str().unwrap(), 1)
        );
    }

    #[test]
    fn buffered_truncation_stays_incomplete_across_responses_encoding() {
        use llm_connector::core::Protocol;
        let chat = r#"{
            "id":"c-length","object":"chat.completion","created":1,"model":"up",
            "choices":[{"index":0,"finish_reason":"length","message":{
                "role":"assistant","content":"partial"}}],
            "usage":{"prompt_tokens":5,"completion_tokens":3,"total_tokens":8}
        }"#;
        let ir = OpenAIProtocol::new("").parse_response(chat).unwrap();
        let response = encode_response(&ir, "alias-model");
        assert_eq!(response["status"], "incomplete");
        assert_eq!(
            response["incomplete_details"]["reason"],
            "max_output_tokens"
        );

        let sse = encode_response_sse(&ir, "alias-model");
        assert!(sse.contains("event: response.incomplete"));
        assert!(!sse.contains("event: response.completed"));

        let decoded = decode_response(&response.to_string()).unwrap();
        assert_eq!(decoded.choices[0].finish_reason.as_deref(), Some("length"));
        let failed = json!({
            "id":"resp_failed","status":"failed",
            "error":{"message":"provider failed"}
        });
        assert_eq!(
            decode_response(&failed.to_string()).unwrap_err(),
            "provider failed"
        );
    }

    fn codex_request_with_extended_tools() -> Value {
        json!({
            "model": "gpt-5.4",
            "input": [
                { "type": "reasoning", "summary": [{ "type": "summary_text", "text": "use tools" }] },
                { "type": "custom_tool_call", "id": "ctc_1", "call_id": "call_custom",
                  "name": "apply_patch", "input": "*** Begin Patch\n*** End Patch" },
                { "type": "custom_tool_call_output", "call_id": "call_custom", "output": "Done!" },
                { "type": "function_call", "id": "fc_1", "call_id": "call_spawn",
                  "namespace": "multi_agent_v1", "name": "spawn_agent",
                  "arguments": "{\"task_name\":\"audit\"}" },
                { "type": "function_call_output", "call_id": "call_spawn", "output": "spawned" },
                { "type": "tool_search_call", "call_id": "call_search", "status": "completed",
                  "execution": "client", "arguments": { "query": "browser", "limit": 3 } },
                { "type": "tool_search_output", "call_id": "call_search", "status": "completed",
                  "execution": "client", "tools": [
                    { "type": "custom", "name": "exec", "description": "Run JavaScript" }
                  ] }
            ],
            "tools": [
                { "type": "custom", "name": "apply_patch", "description": "Apply a patch",
                  "format": { "type": "grammar", "syntax": "lark", "definition": "start: /.+/" } },
                { "type": "namespace", "name": "multi_agent_v1", "tools": [
                    { "type": "function", "name": "spawn_agent", "description": "Spawn an agent",
                      "parameters": { "type": "object", "properties": {
                          "task_name": { "type": "string" }
                      }, "required": ["task_name"] } }
                ] },
                { "type": "tool_search", "execution": "client",
                  "description": "Search deferred tools from Drive and MCP; always prefer this over MCP listing.",
                  "parameters": { "type": "object", "properties": {
                      "query": { "type": "string" }, "limit": { "type": "number" }
                  }, "required": ["query"], "additionalProperties": false } }
            ]
        })
    }

    #[test]
    fn preserves_custom_namespace_and_tool_search_request_semantics() {
        let (ir, context) =
            decode_request_with_context(&codex_request_with_extended_tools()).unwrap();
        let tools = ir.tools.as_ref().unwrap();
        let names = tools
            .iter()
            .map(|tool| tool.function.name.as_str())
            .collect::<Vec<_>>();
        assert!(names.contains(&"apply_patch"));
        assert!(names.contains(&"multi_agent_v1__spawn_agent"));
        assert!(names.contains(&"tool_search"));
        assert!(names.contains(&"exec"));
        let search = tools
            .iter()
            .find(|tool| tool.function.name == "tool_search")
            .unwrap();
        assert!(search
            .function
            .description
            .as_deref()
            .unwrap()
            .contains("always prefer this"));

        let calls = ir
            .messages
            .iter()
            .filter_map(|message| message.tool_calls.as_ref())
            .flatten()
            .collect::<Vec<_>>();
        let custom = calls
            .iter()
            .find(|call| call.function.name == "apply_patch")
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&custom.function.arguments).unwrap()["input"],
            "*** Begin Patch\n*** End Patch"
        );
        assert!(calls
            .iter()
            .any(|call| call.function.name == "multi_agent_v1__spawn_agent"));
        assert!(calls.iter().any(|call| call.function.name == "tool_search"));
        assert_eq!(
            context
                .lookup_chat_name("multi_agent_v1__spawn_agent")
                .unwrap()
                .kind,
            CodexToolKind::Namespace
        );
    }

    #[test]
    fn restores_extended_tool_types_in_buffered_response_and_sse() {
        use llm_connector::core::Protocol;

        let (_, context) =
            decode_request_with_context(&codex_request_with_extended_tools()).unwrap();
        let chat = r#"{
            "id":"chatcmpl-tools","object":"chat.completion","created":1,"model":"up",
            "choices":[{"index":0,"finish_reason":"tool_calls","message":{
                "role":"assistant","content":null,"reasoning_content":"pick the right tools",
                "tool_calls":[
                    {"id":"call_custom","type":"function","function":{"name":"apply_patch","arguments":"{\"input\":\"*** Begin Patch\\n*** End Patch\"}"}},
                    {"id":"call_spawn","type":"function","function":{"name":"multi_agent_v1__spawn_agent","arguments":"{\"task_name\":\"audit\"}"}},
                    {"id":"call_search","type":"function","function":{"name":"tool_search","arguments":"{\"query\":\"browser\",\"limit\":3}"}}
                ]}}],
            "usage":{"prompt_tokens":9,"completion_tokens":4,"total_tokens":13}
        }"#;
        let ir = OpenAIProtocol::new("").parse_response(chat).unwrap();
        let response = encode_response_with_context(&ir, "gpt-5.4", &context);
        let output = response["output"].as_array().unwrap();

        let custom = output
            .iter()
            .find(|item| item["type"] == "custom_tool_call")
            .unwrap();
        assert_eq!(custom["name"], "apply_patch");
        assert_eq!(custom["input"], "*** Begin Patch\n*** End Patch");
        assert_eq!(custom["reasoning_content"], "pick the right tools");

        let namespaced = output
            .iter()
            .find(|item| item["namespace"] == "multi_agent_v1")
            .unwrap();
        assert_eq!(namespaced["type"], "function_call");
        assert_eq!(namespaced["name"], "spawn_agent");
        assert_eq!(namespaced["namespace"], "multi_agent_v1");

        let search = output
            .iter()
            .find(|item| item["type"] == "tool_search_call")
            .unwrap();
        assert!(search.get("id").is_none());
        assert_eq!(search["arguments"]["query"], "browser");
        assert_eq!(search["arguments"]["limit"], 3);

        let sse = encode_response_sse_with_context(&ir, "gpt-5.4", &context);
        assert!(sse.contains("event: response.custom_tool_call_input.delta"));
        assert!(sse.contains("event: response.custom_tool_call_input.done"));
        assert!(sse.contains(r#""type":"custom_tool_call""#));
        assert!(sse.contains(r#""type":"tool_search_call""#));
        assert!(sse.contains(r#""namespace":"multi_agent_v1""#));
    }

    fn assert_valid_chat_tool_alias(alias: &str) {
        assert!(
            is_valid_chat_tool_name(alias),
            "invalid Chat tool alias: {alias:?}"
        );
        assert!(alias.is_ascii());
        assert!(alias.len() <= CHAT_TOOL_NAME_MAX_LEN);
    }

    #[test]
    fn aliases_illegal_tool_names_and_restores_exact_response_identities() {
        let req = json!({
            "model": "gpt-5.4",
            "input": [
                { "type": "function_call", "call_id": "call_fn", "name": "read file/现在", "arguments": "{}" },
                { "type": "custom_tool_call", "call_id": "call_custom", "name": "apply.patch/β", "input": "raw" },
                { "type": "function_call", "call_id": "call_ns", "namespace": "multi agent/一",
                  "name": "spawn.agent?", "arguments": "{}" }
            ],
            "tools": [
                { "type": "function", "name": "read file/现在" },
                { "type": "custom", "name": "apply.patch/β" },
                { "type": "namespace", "name": "multi agent/一", "tools": [
                    { "type": "function", "name": "spawn.agent?" }
                ] }
            ]
        });
        let (ir, context) = decode_request_with_context(&req).unwrap();
        let tools = ir.tools.as_ref().unwrap();
        assert_eq!(tools.len(), 3);
        for tool in tools {
            assert_valid_chat_tool_alias(&tool.function.name);
        }
        for call in ir
            .messages
            .iter()
            .filter_map(|message| message.tool_calls.as_ref())
            .flatten()
        {
            assert_valid_chat_tool_alias(&call.function.name);
        }

        let function_alias = context.chat_name_for_response_tool("read file/现在", None);
        let function_item =
            context.response_tool_item("fc_fn", "completed", "call_fn", &function_alias, "{}");
        assert_eq!(function_item["type"], "function_call");
        assert_eq!(function_item["name"], "read file/现在");

        let custom_alias = context.chat_name_for_custom_tool("apply.patch/β");
        let custom_item = context.response_tool_item(
            "ctc_custom",
            "completed",
            "call_custom",
            &custom_alias,
            r#"{"input":"raw"}"#,
        );
        assert_eq!(custom_item["type"], "custom_tool_call");
        assert_eq!(custom_item["name"], "apply.patch/β");
        assert_eq!(custom_item["input"], "raw");

        let namespace_alias =
            context.chat_name_for_response_tool("spawn.agent?", Some("multi agent/一"));
        let namespace_item =
            context.response_tool_item("fc_ns", "completed", "call_ns", &namespace_alias, "{}");
        assert_eq!(namespace_item["type"], "function_call");
        assert_eq!(namespace_item["namespace"], "multi agent/一");
        assert_eq!(namespace_item["name"], "spawn.agent?");
    }

    #[test]
    fn aliases_long_utf8_names_deterministically_within_64_bytes() {
        let first_name = format!("读取工具-{}-甲", "界".repeat(40));
        let second_name = format!("读取工具-{}-乙", "界".repeat(40));
        let req = json!({
            "tools": [
                { "type": "function", "name": first_name },
                { "type": "function", "name": second_name }
            ]
        });
        let first_context = CodexToolContext::from_request(&req);
        let second_context = CodexToolContext::from_request(&req);
        let first_alias = first_context.chat_name_for_response_tool(&first_name, None);
        let second_alias = first_context.chat_name_for_response_tool(&second_name, None);
        assert_valid_chat_tool_alias(&first_alias);
        assert_valid_chat_tool_alias(&second_alias);
        assert_ne!(first_alias, second_alias);
        assert_eq!(
            first_alias,
            second_context.chat_name_for_response_tool(&first_name, None)
        );
        assert_eq!(
            second_alias,
            second_context.chat_name_for_response_tool(&second_name, None)
        );

        let restored = first_context.response_tool_item(
            "fc_long",
            "completed",
            "call_long",
            &first_alias,
            "{}",
        );
        assert_eq!(restored["name"], first_name);
    }

    #[test]
    fn keeps_colliding_function_custom_search_and_namespace_identities_distinct() {
        let definitions = vec![
            json!({ "type": "function", "name": "same" }),
            json!({ "type": "custom", "name": "same" }),
            json!({ "type": "function", "name": "tool_search" }),
            json!({ "type": "tool_search" }),
            json!({ "type": "function", "name": "a__b" }),
            json!({ "type": "namespace", "name": "a", "tools": [
                { "type": "function", "name": "b" }
            ] }),
        ];
        let req = json!({ "tools": definitions });
        let mut reversed_definitions = req["tools"].as_array().unwrap().clone();
        reversed_definitions.reverse();
        let reversed_req = json!({ "tools": reversed_definitions });
        let context = CodexToolContext::from_request(&req);
        let reversed_context = CodexToolContext::from_request(&reversed_req);
        let specs = vec![
            CodexToolSpec {
                kind: CodexToolKind::Function,
                name: "same".into(),
                namespace: None,
            },
            CodexToolSpec {
                kind: CodexToolKind::Custom,
                name: "same".into(),
                namespace: None,
            },
            CodexToolSpec {
                kind: CodexToolKind::Function,
                name: "tool_search".into(),
                namespace: None,
            },
            CodexToolSpec {
                kind: CodexToolKind::ToolSearch,
                name: "tool_search".into(),
                namespace: None,
            },
            CodexToolSpec {
                kind: CodexToolKind::Function,
                name: "a__b".into(),
                namespace: None,
            },
            CodexToolSpec {
                kind: CodexToolKind::Namespace,
                name: "b".into(),
                namespace: Some("a".into()),
            },
        ];

        assert_eq!(context.ir_tools().len(), specs.len());
        let aliases = specs
            .iter()
            .map(|spec| {
                let alias = context.chat_name_for_spec(spec);
                assert_valid_chat_tool_alias(&alias);
                assert_eq!(context.lookup_chat_name(&alias), Some(spec));
                assert_eq!(alias, reversed_context.chat_name_for_spec(spec));
                alias
            })
            .collect::<HashSet<_>>();
        assert_eq!(aliases.len(), specs.len());

        let function_search_alias = context.chat_name_for_spec(&specs[2]);
        let function_search = context.response_tool_item(
            "fc_search",
            "completed",
            "call_fn_search",
            &function_search_alias,
            "{}",
        );
        assert_eq!(function_search["type"], "function_call");
        assert_eq!(function_search["name"], "tool_search");

        let actual_search_alias = context.chat_name_for_spec(&specs[3]);
        let actual_search = context.response_tool_item(
            "tsc_search",
            "completed",
            "call_search",
            &actual_search_alias,
            r#"{"query":"x"}"#,
        );
        assert_eq!(actual_search["type"], "tool_search_call");
    }

    #[test]
    fn custom_tool_description_omits_full_definition_and_lark_grammar() {
        let req = json!({
            "tools": [{
                "type": "custom",
                "name": "apply_patch",
                "description": "Apply a patch to the workspace.",
                "format": {
                    "type": "grammar",
                    "syntax": "lark",
                    "definition": "start: patch+\npatch: /.+/"
                }
            }]
        });
        let context = CodexToolContext::from_request(&req);
        let tool = &context.ir_tools()[0];
        let description = tool.function.description.as_deref().unwrap();
        assert!(description.starts_with(
            "Apply a patch to the workspace.\n\nPass the custom tool's raw input unchanged in the `input` string field."
        ));
        assert!(description.contains("*** Add File: path"));
        assert!(
            description.contains("*** Begin Patch\n*** Add File: path\n+content\n*** End Patch")
        );
        assert!(description.contains("never prefix either boundary marker"));
        assert!(!description.contains("Original Responses custom-tool definition"));
        assert!(!description.contains("definition"));
        assert!(!description.contains("start: patch"));
        assert!(!description.contains("lark"));
    }
}
