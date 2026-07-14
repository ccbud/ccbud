//! Cross-request history for bridging Codex Responses requests to chat-style upstreams.
//!
//! Responses clients may continue a tool turn with only
//! `previous_response_id + new input`. Chat-style protocols do not implement that
//! server-side continuation, so they need the previous request input and assistant
//! output restored recursively into the next request. Tool outputs additionally need
//! the original assistant call, including its name, arguments, and reasoning metadata.
//! This store records that model-visible context and restores it before conversion.

use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Write};
use tokio::sync::RwLock;

const MAX_CACHED_RESPONSES: usize = 512;
// Count-bounding alone is not enough once every entry carries the cumulative transcript: a long
// conversation would otherwise make the cache grow quadratically. This is a logical serialized
// size ceiling (including the duplicated call lookup values), which keeps resident memory in the
// same order of magnitude while still leaving ample room for large model contexts.
const MAX_CACHED_HISTORY_BYTES: usize = 32 * 1024 * 1024;

type ScopedResponseId = (String, String);
type ScopedCallId = (String, String);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ResponseOrigin {
    #[default]
    Local,
    Native(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResponseMetadata {
    pub origin: ResponseOrigin,
    pub materializable: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistoryResolution {
    pub changed: usize,
    pub had_previous_response_id: bool,
    pub previous_found: bool,
    pub previous_materialized: bool,
    pub previous_origin: Option<ResponseOrigin>,
}

#[derive(Debug, Clone, Default)]
struct CachedResponse {
    /// Full model-visible input used to create this response. For an incremental
    /// `previous_response_id` request this already includes every earlier request and response.
    request_input: Vec<Value>,
    output: Vec<Value>,
    calls_by_id: HashMap<String, Value>,
    call_order: Vec<String>,
    serialized_bytes: usize,
    origin: ResponseOrigin,
    materializable: bool,
}

#[derive(Debug, Default)]
struct HistoryInner {
    responses: HashMap<ScopedResponseId, CachedResponse>,
    response_order: VecDeque<ScopedResponseId>,
    /// Reverse index used only when `previous_response_id` is absent or stale.
    /// A fallback is safe only when a call id resolves to exactly one response.
    call_index: HashMap<ScopedCallId, VecDeque<ScopedResponseId>>,
    cached_bytes: usize,
}

#[derive(Debug, Clone, Default)]
struct CachedLookup {
    previous: Option<CachedResponse>,
    fallback: CachedResponse,
}

/// Thread-safe, bounded Responses conversation-history store.
#[derive(Debug, Default)]
pub struct CodexHistoryStore {
    inner: RwLock<HistoryInner>,
}

impl CodexHistoryStore {
    /// Record the full translated request input plus supported assistant-output items from a
    /// resumable terminal Responses response (`completed` or `incomplete`).
    ///
    /// Returns the number of cached output items. Responses without an id are ignored; an otherwise
    /// empty response is still retained so provider ownership remains known.
    pub async fn record_response(&self, request: &Value, response: &Value) -> usize {
        self.record_response_scoped("", request, response).await
    }

    /// Scoped variant used by the gateway so response/call ids from different client sessions can
    /// never satisfy one another while the same conversation can survive a provider switch.
    pub async fn record_response_scoped(
        &self,
        scope: &str,
        request: &Value,
        response: &Value,
    ) -> usize {
        // Preserve the original store API for internal callers/tests that predate Responses
        // terminal statuses. Gateway-owned/native recording uses the metadata variant below,
        // which requires an explicit resumable terminal status.
        let mut legacy_terminal;
        let response = if response.get("status").is_none() {
            legacy_terminal = response.clone();
            legacy_terminal["status"] = Value::String("completed".to_string());
            &legacy_terminal
        } else {
            response
        };
        self.record_response_scoped_with_metadata(
            scope,
            ResponseOrigin::Local,
            true,
            request,
            response,
        )
        .await
    }

    pub async fn record_response_scoped_with_metadata(
        &self,
        scope: &str,
        origin: ResponseOrigin,
        materializable: bool,
        request: &Value,
        response: &Value,
    ) -> usize {
        if response
            .get("object")
            .and_then(Value::as_str)
            .is_some_and(|object| object != "response")
        {
            return 0;
        }
        if !matches!(
            response.get("status").and_then(Value::as_str),
            Some("completed" | "incomplete")
        ) {
            return 0;
        }

        let Some(response_id) = response
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return 0;
        };

        let request_input = request_input_items(request);
        let request_input_is_complete = request_input_is_materializable(request);
        let (output, output_is_complete) = match response.get("output").and_then(Value::as_array) {
            Some(items) => {
                let output = items
                    .iter()
                    .filter_map(cached_output_item)
                    .collect::<Vec<_>>();
                let output_is_complete =
                    output.len() == items.len() && items.iter().all(history_item_is_materializable);
                (output, output_is_complete)
            }
            None => (Vec::new(), false),
        };
        // Preserve ownership for a response containing an item this bridge cannot replay, but
        // never advertise that partial transcript as safe to move to another provider.
        let materializable = materializable && request_input_is_complete && output_is_complete;

        self.inner.write().await.insert_response_with_metadata(
            scope,
            response_id,
            request_input,
            output,
            origin,
            materializable,
        )
    }

    pub async fn response_metadata(
        &self,
        scope: &str,
        response_id: &str,
    ) -> Option<ResponseMetadata> {
        let response_id = response_id.trim();
        if response_id.is_empty() {
            return None;
        }
        self.inner
            .read()
            .await
            .responses
            .get(&(scope.to_string(), response_id.to_string()))
            .map(|response| ResponseMetadata {
                origin: response.origin.clone(),
                materializable: response.materializable,
            })
    }

    /// Restore or enrich call items required by a follow-up Responses request.
    ///
    /// Missing calls are inserted immediately before the first matching output.
    /// Parallel calls from the same response are restored as one ordered group.
    /// Existing call items are enriched when fields such as `name`, `arguments`,
    /// or `reasoning_content` are missing.
    ///
    /// The primary lookup uses `previous_response_id`. If that id is absent or
    /// stale, a call-id fallback is used only when the caller supplied a safe scope and the call id
    /// is unique inside that client session. Returns the number of restored or enriched items.
    pub async fn enrich_request(&self, body: &mut Value) -> usize {
        self.enrich_request_scoped("", false, body).await
    }

    /// Scoped variant. Missing/stale-`previous_response_id` call-id recovery is allowed only when
    /// the caller can provide a client-session scope; otherwise orphan validation must fail.
    pub async fn enrich_request_scoped(
        &self,
        scope: &str,
        allow_call_id_fallback: bool,
        body: &mut Value,
    ) -> usize {
        self.resolve_request_scoped(scope, allow_call_id_fallback, false, body)
            .await
            .changed
    }

    pub async fn materialize_request_scoped(
        &self,
        scope: &str,
        allow_call_id_fallback: bool,
        body: &mut Value,
    ) -> HistoryResolution {
        self.resolve_request_scoped(scope, allow_call_id_fallback, true, body)
            .await
    }

    async fn resolve_request_scoped(
        &self,
        scope: &str,
        allow_call_id_fallback: bool,
        strip_materialized_previous_id: bool,
        body: &mut Value,
    ) -> HistoryResolution {
        let previous_response_id = body
            .get("previous_response_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string);
        let had_previous_response_id = previous_response_id.is_some();

        let input_was_missing = body.get("input").is_none();
        let original_input = body.get_mut("input").map(std::mem::take);
        let original_was_object = original_input.as_ref().is_some_and(Value::is_object);
        let mut original_string = None;
        let mut unsupported_input = None;
        let items = match original_input {
            Some(Value::Array(items)) => items,
            Some(Value::Object(object)) => vec![Value::Object(object)],
            Some(Value::String(value)) => {
                original_string = Some(value.clone());
                vec![serde_json::json!({
                    "type": "message",
                    "role": "user",
                    "content": value,
                })]
            }
            Some(other) => {
                unsupported_input = Some(other);
                Vec::new()
            }
            None => Vec::new(),
        };

        let output_call_ids = items
            .iter()
            .filter(|item| {
                item.get("type")
                    .and_then(Value::as_str)
                    .is_some_and(is_call_output_item_type)
            })
            .filter_map(response_item_call_id)
            .collect::<HashSet<_>>();
        let existing_call_ids = items
            .iter()
            .filter(|item| {
                item.get("type")
                    .and_then(Value::as_str)
                    .is_some_and(is_call_item_type)
            })
            .filter_map(response_item_call_id)
            .collect::<HashSet<_>>();
        let requested_call_ids = output_call_ids
            .union(&existing_call_ids)
            .cloned()
            .collect::<HashSet<_>>();

        let lookup = self
            .lookup(
                scope,
                previous_response_id.as_deref(),
                &requested_call_ids,
                allow_call_id_fallback,
            )
            .await;
        let previous_found = lookup.previous.is_some();
        let previous_origin = lookup
            .previous
            .as_ref()
            .map(|response| response.origin.clone());
        let previous_materialized = lookup
            .previous
            .as_ref()
            .is_some_and(|response| response.materializable);

        if let Some(original_input) = unsupported_input {
            if let Some(object) = body.as_object_mut() {
                object.insert("input".to_string(), original_input);
            }
            return HistoryResolution {
                changed: 0,
                had_previous_response_id,
                previous_found,
                previous_materialized: false,
                previous_origin,
            };
        }

        // A native provider may still own a continuation that the gateway observed only after a
        // restart. Keep that request byte-for-byte intact for same-provider passthrough; callers
        // must reject it before cross-wire/provider-switch forwarding because its prefix is absent.
        if previous_found && !previous_materialized {
            if !input_was_missing {
                let restored_input = if original_string.is_some() && items.len() == 1 {
                    Value::String(original_string.unwrap_or_default())
                } else if original_was_object && items.len() == 1 {
                    items.into_iter().next().unwrap_or(Value::Null)
                } else {
                    Value::Array(items)
                };
                if let Some(object) = body.as_object_mut() {
                    object.insert("input".to_string(), restored_input);
                }
            }
            return HistoryResolution {
                changed: 0,
                had_previous_response_id,
                previous_found,
                previous_materialized,
                previous_origin,
            };
        }
        let replay_context = lookup
            .previous
            .as_ref()
            .or_else(|| lookup.fallback.materializable.then_some(&lookup.fallback));
        let (items, restored) = merge_previous_context(items, replay_context);
        let mut enriched = 0usize;
        let mut new_items = Vec::with_capacity(items.len());

        for mut item in items {
            if item
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(is_call_item_type)
            {
                if let Some(call_id) = response_item_call_id(&item) {
                    if let Some(cached) = lookup.call(&call_id) {
                        if enrich_call_item_from_cache(&mut item, cached) {
                            enriched += 1;
                        }
                    }
                }
            }
            new_items.push(item);
        }

        let changed = restored + enriched;
        let resolved_input = if changed == 0 && original_string.is_some() && new_items.len() == 1 {
            Some(Value::String(original_string.unwrap_or_default()))
        } else if changed == 0 && original_was_object && new_items.len() == 1 {
            Some(new_items.into_iter().next().unwrap_or(Value::Null))
        } else if input_was_missing && changed == 0 {
            None
        } else {
            Some(Value::Array(new_items))
        };
        if let (Some(object), Some(resolved_input)) = (body.as_object_mut(), resolved_input) {
            object.insert("input".to_string(), resolved_input);
        }
        if strip_materialized_previous_id && previous_materialized {
            if let Some(object) = body.as_object_mut() {
                object.remove("previous_response_id");
            }
        }
        HistoryResolution {
            changed,
            had_previous_response_id,
            previous_found,
            previous_materialized,
            previous_origin,
        }
    }

    async fn lookup(
        &self,
        scope: &str,
        previous_response_id: Option<&str>,
        requested_call_ids: &HashSet<String>,
        allow_call_id_fallback: bool,
    ) -> CachedLookup {
        let inner = self.inner.read().await;
        let previous = previous_response_id.and_then(|id| {
            inner
                .responses
                .get(&(scope.to_string(), id.to_string()))
                .cloned()
        });
        let fallback = if allow_call_id_fallback {
            inner.unique_fallback_response(scope, requested_call_ids, previous.as_ref())
        } else {
            CachedResponse::default()
        };
        CachedLookup { previous, fallback }
    }
}

impl HistoryInner {
    fn insert_response(
        &mut self,
        scope: &str,
        response_id: &str,
        request_input: Vec<Value>,
        output: Vec<Value>,
    ) -> usize {
        self.insert_response_with_metadata(
            scope,
            response_id,
            request_input,
            output,
            ResponseOrigin::Local,
            true,
        )
    }

    fn insert_response_with_metadata(
        &mut self,
        scope: &str,
        response_id: &str,
        request_input: Vec<Value>,
        output: Vec<Value>,
        origin: ResponseOrigin,
        materializable: bool,
    ) -> usize {
        self.insert_response_with_metadata_and_limits(
            scope,
            response_id,
            request_input,
            output,
            origin,
            materializable,
            MAX_CACHED_RESPONSES,
            MAX_CACHED_HISTORY_BYTES,
        )
    }

    fn insert_response_with_limits(
        &mut self,
        scope: &str,
        response_id: &str,
        request_input: Vec<Value>,
        output: Vec<Value>,
        max_responses: usize,
        max_bytes: usize,
    ) -> usize {
        self.insert_response_with_metadata_and_limits(
            scope,
            response_id,
            request_input,
            output,
            ResponseOrigin::Local,
            true,
            max_responses,
            max_bytes,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_response_with_metadata_and_limits(
        &mut self,
        scope: &str,
        response_id: &str,
        request_input: Vec<Value>,
        output: Vec<Value>,
        origin: ResponseOrigin,
        materializable: bool,
        max_responses: usize,
        max_bytes: usize,
    ) -> usize {
        let mut cached_response = CachedResponse {
            request_input,
            output,
            origin,
            materializable,
            ..CachedResponse::default()
        };
        for item in &cached_response.output {
            if let Some((call_id, item)) = cached_call_item(item) {
                if !cached_response.calls_by_id.contains_key(&call_id) {
                    cached_response.call_order.push(call_id.clone());
                }
                cached_response.calls_by_id.insert(call_id, item);
            }
        }
        cached_response.serialized_bytes =
            cached_response_size(scope, response_id, &cached_response);
        let cached_count = cached_response.output.len();
        let response_key = (scope.to_string(), response_id.to_string());

        // Never let one pathological request flush every useful older entry before being evicted
        // itself. A same-id response is authoritative, though, so an oversized replacement drops
        // only its stale predecessor rather than leaving old history addressable under that id.
        if cached_response.serialized_bytes > max_bytes {
            if self.remove_response(&response_key) {
                self.response_order
                    .retain(|cached_id| cached_id != &response_key);
            }
            return 0;
        }

        let replacing = self.responses.contains_key(&response_key);
        if !replacing {
            self.response_order.push_back(response_key.clone());
        }

        // A completed response is authoritative. Replacing an already-seen id keeps
        // retry/replay recording idempotent and prevents stale call-index entries.
        self.remove_response(&response_key);

        for call_id in &cached_response.call_order {
            self.index_call(scope, &call_id, &response_key);
        }
        self.cached_bytes = self
            .cached_bytes
            .checked_add(cached_response.serialized_bytes)
            .unwrap_or(usize::MAX);
        self.responses.insert(response_key.clone(), cached_response);
        self.prune_to_limits(max_responses, max_bytes);
        if self.responses.contains_key(&response_key) {
            cached_count
        } else {
            0
        }
    }

    fn prune_to_limits(&mut self, max_responses: usize, max_bytes: usize) {
        while self.response_order.len() > max_responses || self.cached_bytes > max_bytes {
            let Some(response_id) = self.response_order.pop_front() else {
                break;
            };
            self.remove_response(&response_id);
        }
    }

    fn remove_response(&mut self, response_id: &ScopedResponseId) -> bool {
        self.remove_response_from_call_index(response_id);
        let Some(response) = self.responses.remove(response_id) else {
            return false;
        };
        self.cached_bytes = self.cached_bytes.saturating_sub(response.serialized_bytes);
        true
    }

    fn index_call(&mut self, scope: &str, call_id: &str, response_id: &ScopedResponseId) {
        let response_ids = self
            .call_index
            .entry((scope.to_string(), call_id.to_string()))
            .or_default();
        if !response_ids
            .iter()
            .any(|cached_id| cached_id == response_id)
        {
            response_ids.push_back(response_id.clone());
        }
    }

    fn remove_response_from_call_index(&mut self, response_id: &ScopedResponseId) {
        for response_ids in self.call_index.values_mut() {
            response_ids.retain(|cached_id| cached_id != response_id);
        }
        self.call_index
            .retain(|_, response_ids| !response_ids.is_empty());
    }

    fn unique_fallback_response(
        &self,
        scope: &str,
        requested_call_ids: &HashSet<String>,
        previous: Option<&CachedResponse>,
    ) -> CachedResponse {
        // A resolved previous_response_id is authoritative. Grafting calls from another cached
        // branch onto it would create history that no provider ever observed.
        if previous.is_some() || requested_call_ids.is_empty() {
            return CachedResponse::default();
        }

        let mut source_response_id: Option<ScopedResponseId> = None;
        for call_id in requested_call_ids {
            let Some(response_id) = self.unique_response_for_call(scope, call_id) else {
                return CachedResponse::default();
            };
            if source_response_id
                .as_ref()
                .is_some_and(|source| source != &response_id)
            {
                return CachedResponse::default();
            }
            source_response_id = Some(response_id);
        }

        source_response_id
            .and_then(|response_id| self.responses.get(&response_id).cloned())
            .unwrap_or_default()
    }

    fn unique_response_for_call(&self, scope: &str, call_id: &str) -> Option<ScopedResponseId> {
        let response_ids = self
            .call_index
            .get(&(scope.to_string(), call_id.to_string()))?;
        let mut found: Option<&ScopedResponseId> = None;
        for response_id in response_ids {
            let Some(response) = self.responses.get(response_id) else {
                continue;
            };
            // An owner-only native response may contain just a delta after an unavailable prefix.
            // Its calls are useful to that provider via previous_response_id, but are never a safe
            // source for session fallback because doing so would manufacture a truncated chain.
            if !response.materializable {
                continue;
            }
            if !response.calls_by_id.contains_key(call_id) {
                continue;
            }
            if found.is_some() {
                return None;
            }
            found = Some(response_id);
        }
        found.cloned()
    }

    fn unique_call(&self, scope: &str, call_id: &str) -> Option<&Value> {
        let response_id = self.unique_response_for_call(scope, call_id)?;
        self.responses
            .get(&response_id)
            .and_then(|response| response.calls_by_id.get(call_id))
    }
}

#[derive(Default)]
struct ByteCounter {
    bytes: usize,
}

impl Write for ByteCounter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.bytes = self.bytes.saturating_add(buf.len());
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn serialized_size<T: serde::Serialize + ?Sized>(value: &T) -> usize {
    let mut counter = ByteCounter::default();
    if serde_json::to_writer(&mut counter, value).is_err() {
        usize::MAX
    } else {
        counter.bytes
    }
}

fn cached_response_size(scope: &str, response_id: &str, response: &CachedResponse) -> usize {
    let origin_bytes = match &response.origin {
        ResponseOrigin::Local => 1,
        ResponseOrigin::Native(provider_id) => 1usize.saturating_add(serialized_size(provider_id)),
    };
    serialized_size(scope)
        .saturating_add(serialized_size(response_id))
        .saturating_add(serialized_size(&response.request_input))
        .saturating_add(serialized_size(&response.output))
        .saturating_add(serialized_size(&response.calls_by_id))
        .saturating_add(serialized_size(&response.call_order))
        .saturating_add(origin_bytes)
        .saturating_add(1)
}

impl CachedLookup {
    fn call(&self, call_id: &str) -> Option<&Value> {
        self.previous
            .as_ref()
            .and_then(|previous| previous.calls_by_id.get(call_id))
            .or_else(|| self.fallback.calls_by_id.get(call_id))
    }
}

/// Merge the directly referenced response's complete model-visible context into the new input.
///
/// The cached request prefix is always restored before the previous response output. A client that
/// already sent the full prefix is detected by a matching request prefix plus at least one prior
/// output anchor, while a coincidentally repeated new user message is still treated as a delta.
/// Every supported previous output item is restored. Filtering unmatched calls would no longer be
/// equivalent to provider-side `previous_response_id` continuation and could silently turn an
/// invalid continuation into a different, truncated conversation.
fn merge_previous_context(
    items: Vec<Value>,
    previous: Option<&CachedResponse>,
) -> (Vec<Value>, usize) {
    let Some(previous) = previous else {
        return (items, 0);
    };

    let eligible_output = previous.output.clone();
    let request_prefix_len = previous.request_input.len();
    let has_request_prefix = request_prefix_len <= items.len()
        && previous
            .request_input
            .iter()
            .zip(&items)
            .all(|(cached, input)| cached_item_matches_input(cached, input));
    let has_output_anchor = has_request_prefix
        && !previous.output.is_empty()
        && previous.output.iter().any(|cached| {
            items[request_prefix_len..]
                .iter()
                .any(|input| cached_item_matches_input(cached, input))
        });

    let (prefix, tail, restored_input) = if has_request_prefix && has_output_anchor {
        (
            items[..request_prefix_len].to_vec(),
            items[request_prefix_len..].to_vec(),
            0,
        )
    } else {
        (
            previous.request_input.clone(),
            items,
            previous.request_input.len(),
        )
    };
    let (tail, restored_output) = merge_cached_output(tail, &eligible_output);
    let mut merged = Vec::with_capacity(prefix.len() + tail.len());
    merged.extend(prefix);
    merged.extend(tail);
    (merged, restored_input + restored_output)
}

fn merge_cached_output(items: Vec<Value>, eligible: &[Value]) -> (Vec<Value>, usize) {
    if eligible.is_empty() {
        return (items, 0);
    }

    // Match explicit prior-output items monotonically. Legal explicit history keeps
    // response order, and monotonic matching avoids treating a coincidentally reused
    // text value later in the request as the prior item.
    let mut matches = HashMap::<usize, usize>::new();
    let mut next_input = 0usize;
    for (cached_index, cached) in eligible.iter().enumerate() {
        let Some(relative_index) = items[next_input..]
            .iter()
            .position(|item| cached_item_matches_input(cached, item))
        else {
            continue;
        };
        let input_index = next_input + relative_index;
        matches.insert(input_index, cached_index);
        next_input = input_index + 1;
    }

    if matches.is_empty() {
        let restored = eligible.len();
        let mut merged = Vec::with_capacity(restored + items.len());
        merged.extend(eligible.iter().cloned());
        merged.extend(items);
        return (merged, restored);
    }

    let last_match = matches.keys().copied().max().unwrap_or(0);
    let mut merged = Vec::with_capacity(eligible.len() + items.len());
    let mut cached_cursor = 0usize;
    let mut restored = 0usize;
    for (input_index, item) in items.into_iter().enumerate() {
        if let Some(&cached_index) = matches.get(&input_index) {
            while cached_cursor < cached_index {
                merged.push(eligible[cached_cursor].clone());
                cached_cursor += 1;
                restored += 1;
            }
            // The explicit item wins (and may intentionally contain richer content).
            merged.push(item);
            cached_cursor = cached_index + 1;
            if input_index == last_match {
                while cached_cursor < eligible.len() {
                    merged.push(eligible[cached_cursor].clone());
                    cached_cursor += 1;
                    restored += 1;
                }
            }
        } else {
            merged.push(item);
        }
    }
    (merged, restored)
}

fn request_input_items(request: &Value) -> Vec<Value> {
    match request.get("input") {
        Some(Value::Array(items)) => items.clone(),
        Some(Value::Object(object)) => vec![Value::Object(object.clone())],
        Some(Value::String(value)) => vec![serde_json::json!({
            "type": "message",
            "role": "user",
            "content": value,
        })],
        _ => Vec::new(),
    }
}

fn request_input_is_materializable(request: &Value) -> bool {
    match request.get("input") {
        None | Some(Value::String(_)) => true,
        Some(Value::Array(items)) => items.iter().all(history_item_is_materializable),
        Some(item @ Value::Object(_)) => history_item_is_materializable(item),
        _ => false,
    }
}

fn history_item_is_materializable(item: &Value) -> bool {
    let Some(object) = item.as_object() else {
        return false;
    };
    let item_type = object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_else(|| {
            if object.get("role").is_some() {
                "message"
            } else {
                ""
            }
        });
    match item_type {
        "message" => object
            .get("content")
            .map_or(true, history_content_is_materializable),
        "reasoning" => {
            let has_opaque_reasoning = object
                .get("encrypted_content")
                .is_some_and(|value| !is_empty_value(value));
            !has_opaque_reasoning
                && object
                    .get("summary")
                    .map_or(true, history_content_is_materializable)
                && object
                    .get("content")
                    .map_or(true, history_content_is_materializable)
        }
        item_type if is_call_item_type(item_type) || is_call_output_item_type(item_type) => true,
        _ => false,
    }
}

fn history_content_is_materializable(content: &Value) -> bool {
    match content {
        Value::Null | Value::String(_) => true,
        Value::Array(parts) => parts.iter().all(|part| {
            let Some(part_type) = part.get("type").and_then(Value::as_str) else {
                return false;
            };
            match part_type {
                "input_text" | "output_text" | "text" | "summary_text" => {
                    part.get("text").is_some_and(Value::is_string)
                }
                "input_image" => part
                    .get("image_url")
                    .and_then(|value| {
                        value
                            .as_str()
                            .or_else(|| value.get("url").and_then(Value::as_str))
                    })
                    .is_some(),
                _ => false,
            }
        }),
        _ => false,
    }
}

fn cached_item_matches_input(cached: &Value, input: &Value) -> bool {
    let cached_type = cached.get("type").and_then(Value::as_str);
    let input_type = input.get("type").and_then(Value::as_str);
    if cached_type.is_some_and(is_call_item_type) {
        return input_type.is_some_and(is_call_item_type)
            && response_item_call_id(cached).is_some_and(|call_id| {
                response_item_call_id(input).as_deref() == Some(call_id.as_str())
            });
    }

    if let Some(id) = cached
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        return cached_type == input_type
            && input
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|input_id| input_id.trim() == id);
    }
    cached == input
}

fn cached_call_item(item: &Value) -> Option<(String, Value)> {
    if !item
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(is_call_item_type)
    {
        return None;
    }
    let call_id = response_item_call_id(item)?;
    Some((call_id, item.clone()))
}

fn cached_output_item(item: &Value) -> Option<Value> {
    match item.get("type").and_then(Value::as_str) {
        Some("reasoning") => Some(item.clone()),
        Some("message")
            if item
                .get("role")
                .and_then(Value::as_str)
                .map_or(true, |role| role == "assistant") =>
        {
            Some(item.clone())
        }
        Some(item_type) if is_call_item_type(item_type) => Some(item.clone()),
        _ => None,
    }
}

fn response_item_call_id(item: &Value) -> Option<String> {
    item.get("call_id")
        .or_else(|| item.get("id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn is_call_item_type(item_type: &str) -> bool {
    matches!(
        item_type,
        "function_call" | "custom_tool_call" | "tool_search_call"
    )
}

fn is_call_output_item_type(item_type: &str) -> bool {
    matches!(
        item_type,
        "function_call_output" | "custom_tool_call_output" | "tool_search_output"
    )
}

fn is_empty_value(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(value) => value.trim().is_empty(),
        Value::Array(value) => value.is_empty(),
        Value::Object(value) => value.is_empty(),
        _ => false,
    }
}

fn enrich_call_item_from_cache(item: &mut Value, cached: &Value) -> bool {
    let mut changed = false;
    for key in [
        "name",
        "namespace",
        "arguments",
        "input",
        "status",
        "execution",
        "reasoning_content",
        "reasoning",
    ] {
        if item.get(key).is_some_and(|value| !is_empty_value(value)) {
            continue;
        }
        let Some(value) = cached.get(key).filter(|value| !is_empty_value(value)) else {
            continue;
        };
        if let Some(object) = item.as_object_mut() {
            object.insert(key.to_string(), value.clone());
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    #[tokio::test]
    async fn restores_call_before_output_from_previous_response() {
        let history = CodexHistoryStore::default();
        assert_eq!(
            history
                .record_response(
                    &json!({ "input": [] }),
                    &json!({
                        "id": "resp_1",
                        "output": [{
                            "type": "function_call",
                            "call_id": "call_1",
                            "name": "read_file",
                            "arguments": "{\"path\":\"README.md\"}",
                            "reasoning_content": "Need to inspect the file."
                        }]
                    })
                )
                .await,
            1
        );

        let mut request = json!({
            "previous_response_id": "resp_1",
            "input": [{
                "type": "function_call_output",
                "call_id": "call_1",
                "output": "ok"
            }]
        });

        assert_eq!(history.enrich_request(&mut request).await, 1);
        let input = request["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "function_call");
        assert_eq!(input[0]["name"], "read_file");
        assert_eq!(input[0]["reasoning_content"], "Need to inspect the file.");
        assert_eq!(input[1]["type"], "function_call_output");

        // The restored item-level reasoning must survive the JSON → chat IR half,
        // not merely remain present in the enriched request body.
        let decoded = crate::protocol::openai_responses::decode_request(&request).unwrap();
        assert_eq!(
            decoded.messages[0].reasoning_content.as_deref(),
            Some("Need to inspect the file.")
        );
        assert_eq!(
            decoded.messages[0].tool_calls.as_ref().unwrap()[0].id,
            "call_1"
        );
    }

    #[tokio::test]
    async fn restores_text_continuation_and_deduplicates_explicit_prior_output() {
        let history = CodexHistoryStore::default();
        let reasoning = json!({
            "type":"reasoning",
            "id":"rs_text",
            "summary":[{"type":"summary_text","text":"continue the thought"}]
        });
        let assistant = json!({
            "type":"message",
            "id":"msg_text",
            "role":"assistant",
            "content":[{"type":"output_text","text":"First answer."}]
        });
        assert_eq!(
            history
                .record_response(
                    &json!({ "input": [] }),
                    &json!({
                        "id":"resp_text",
                        "output":[reasoning.clone(), assistant.clone()]
                    })
                )
                .await,
            2
        );

        let mut continuation = json!({
            "previous_response_id":"resp_text",
            "input":"Continue."
        });
        assert_eq!(history.enrich_request(&mut continuation).await, 2);
        let input = continuation["input"].as_array().unwrap();
        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["id"], "rs_text");
        assert_eq!(input[1]["id"], "msg_text");
        assert_eq!(input[2]["role"], "user");

        let decoded = crate::protocol::openai_responses::decode_request(&continuation).unwrap();
        assert_eq!(decoded.messages.len(), 2);
        assert_eq!(decoded.messages[0].content_as_text(), "First answer.");
        assert_eq!(
            decoded.messages[0].reasoning_content.as_deref(),
            Some("continue the thought")
        );
        assert_eq!(decoded.messages[1].content_as_text(), "Continue.");

        // A client may send explicit history even while retaining previous_response_id.
        // Stable item ids anchor that history, so the cache must not duplicate it.
        let mut explicit = json!({
            "previous_response_id":"resp_text",
            "input":[
                reasoning,
                assistant,
                {"type":"message","role":"user","content":"Continue."}
            ]
        });
        assert_eq!(history.enrich_request(&mut explicit).await, 0);
        assert_eq!(explicit["input"].as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn restores_request_and_response_context_across_multiple_hops() {
        let history = CodexHistoryStore::default();
        let first_request = json!({
            "input":[{"type":"message","role":"user","content":"First question."}]
        });
        let first_response = json!({
            "id":"resp_first",
            "output":[{
                "type":"message","id":"msg_first","role":"assistant",
                "content":[{"type":"output_text","text":"First answer."}]
            }]
        });
        assert_eq!(
            history
                .record_response(&first_request, &first_response)
                .await,
            1
        );

        let mut second_request = json!({
            "previous_response_id":"resp_first",
            "input":[{"type":"message","role":"user","content":"Second question."}]
        });
        assert_eq!(history.enrich_request(&mut second_request).await, 2);
        let second_response = json!({
            "id":"resp_second",
            "output":[{
                "type":"message","id":"msg_second","role":"assistant",
                "content":[{"type":"output_text","text":"Second answer."}]
            }]
        });
        assert_eq!(
            history
                .record_response(&second_request, &second_response)
                .await,
            1
        );

        let mut third_request = json!({
            "previous_response_id":"resp_second",
            "input":[{"type":"message","role":"user","content":"Third question."}]
        });
        assert_eq!(history.enrich_request(&mut third_request).await, 4);
        let decoded = crate::protocol::openai_responses::decode_request(&third_request).unwrap();
        let transcript = decoded
            .messages
            .iter()
            .map(|message| message.content_as_text())
            .collect::<Vec<_>>();
        assert_eq!(
            transcript,
            vec![
                "First question.",
                "First answer.",
                "Second question.",
                "Second answer.",
                "Third question.",
            ]
        );

        // Full explicit history plus previous_response_id must remain idempotent.
        let before = third_request.clone();
        assert_eq!(history.enrich_request(&mut third_request).await, 0);
        assert_eq!(third_request, before);
    }

    #[tokio::test]
    async fn restores_parallel_calls_as_one_ordered_group() {
        let history = CodexHistoryStore::default();
        history
            .record_response(
                &json!({ "input": [] }),
                &json!({
                    "id": "resp_parallel",
                    "output": [
                        {"type":"function_call","call_id":"call_a","name":"first","arguments":"{}"},
                        {"type":"function_call","call_id":"call_b","name":"second","arguments":"{}"}
                    ]
                }),
            )
            .await;

        // Outputs may arrive in a different order. The assistant call group must
        // retain the order in which the response originally emitted the calls.
        let mut request = json!({
            "previous_response_id": "resp_parallel",
            "input": [
                {"type":"function_call_output","call_id":"call_b","output":"two"},
                {"type":"function_call_output","call_id":"call_a","output":"one"}
            ]
        });

        assert_eq!(history.enrich_request(&mut request).await, 2);
        let input = request["input"].as_array().unwrap();
        assert_eq!(input[0]["call_id"], "call_a");
        assert_eq!(input[1]["call_id"], "call_b");
        assert_eq!(input[2]["type"], "function_call_output");
        assert_eq!(input[3]["type"], "function_call_output");
    }

    #[tokio::test]
    async fn same_client_session_recovers_across_provider_switches() {
        let history = CodexHistoryStore::default();
        // The scope deliberately contains no provider identity: switching the active provider
        // must not sever the client's previous_response_id chain.
        let scope = "session-1";
        history
            .record_response_scoped(
                scope,
                &json!({ "input": [] }),
                &json!({
                    "id": "resp_1",
                    "output": [{
                        "type":"function_call",
                        "call_id":"unique_call",
                        "name":"lookup",
                        "arguments":"{}"
                    }]
                }),
            )
            .await;

        for previous in [None, Some("stale_response"), Some("resp_1")] {
            let mut request = json!({
                "input": [{
                    "type":"function_call_output",
                    "call_id":"unique_call",
                    "output":"ok"
                }]
            });
            if let Some(previous) = previous {
                request["previous_response_id"] = json!(previous);
            }

            assert_eq!(
                history
                    .enrich_request_scoped(scope, true, &mut request)
                    .await,
                1
            );
            assert_eq!(request["input"][0]["type"], "function_call");
            assert_eq!(request["input"][0]["name"], "lookup");
        }
    }

    #[tokio::test]
    async fn missing_previous_response_fallback_requires_a_safe_scope() {
        let history = CodexHistoryStore::default();
        history
            .record_response(
                &json!({ "input": [] }),
                &json!({
                    "id":"resp_1",
                    "output":[{
                        "type":"function_call","call_id":"call_1",
                        "name":"lookup","arguments":"{}"
                    }]
                }),
            )
            .await;
        let mut request = json!({
            "input":[{
                "type":"function_call_output","call_id":"call_1","output":"ok"
            }]
        });

        assert_eq!(history.enrich_request(&mut request).await, 0);
        assert!(crate::protocol::openai_responses::decode_request(&request).is_err());
    }

    #[tokio::test]
    async fn call_id_fallback_never_crosses_client_session_scope() {
        let history = CodexHistoryStore::default();
        history
            .record_response_scoped(
                "session-a",
                &json!({"input":[]}),
                &json!({
                    "id":"resp_a",
                    "output":[{
                        "type":"function_call","call_id":"call_a",
                        "name":"lookup","arguments":"{}"
                    }]
                }),
            )
            .await;
        let mut request = json!({
            "previous_response_id":"stale",
            "input":[{
                "type":"function_call_output","call_id":"call_a","output":"ok"
            }]
        });

        assert_eq!(
            history
                .enrich_request_scoped("session-b", true, &mut request)
                .await,
            0
        );
        assert!(crate::protocol::openai_responses::decode_request(&request).is_err());
    }

    #[tokio::test]
    async fn ambiguous_call_id_does_not_use_fallback() {
        let history = CodexHistoryStore::default();
        let scope = "session-1";
        for response_id in ["resp_1", "resp_2"] {
            history
                .record_response_scoped(
                    scope,
                    &json!({ "input": [] }),
                    &json!({
                        "id": response_id,
                        "output": [{
                            "type":"function_call",
                            "call_id":"shared_call",
                            "name":"lookup",
                            "arguments":"{}"
                        }]
                    }),
                )
                .await;
        }

        let mut request = json!({
            "input": [{
                "type":"function_call_output",
                "call_id":"shared_call",
                "output":"ok"
            }]
        });

        assert_eq!(
            history
                .enrich_request_scoped(scope, true, &mut request)
                .await,
            0
        );
        assert_eq!(request["input"].as_array().unwrap().len(), 1);
        assert_eq!(request["input"][0]["type"], "function_call_output");
        let error = crate::protocol::openai_responses::decode_request(&request).unwrap_err();
        assert!(error.contains("shared_call"));
    }

    #[tokio::test]
    async fn enriches_existing_call_without_duplicating_it() {
        let history = CodexHistoryStore::default();
        history
            .record_response(
                &json!({ "input": [] }),
                &json!({
                    "id": "resp_1",
                    "output": [{
                        "type":"function_call",
                        "call_id":"call_1",
                        "name":"read_file",
                        "arguments":"{\"path\":\"README.md\"}",
                        "reasoning_content":"Need the file."
                    }]
                }),
            )
            .await;

        let mut request = json!({
            "previous_response_id":"resp_1",
            "input":[
                {"type":"function_call","call_id":"call_1"},
                {"type":"function_call_output","call_id":"call_1","output":"ok"}
            ]
        });

        assert_eq!(history.enrich_request(&mut request).await, 1);
        let input = request["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["name"], "read_file");
        assert_eq!(input[0]["arguments"], "{\"path\":\"README.md\"}");
        assert_eq!(input[0]["reasoning_content"], "Need the file.");
    }

    #[tokio::test]
    async fn restores_custom_and_tool_search_calls() {
        let history = CodexHistoryStore::default();
        assert_eq!(
            history
                .record_response(
                    &json!({ "input": [] }),
                    &json!({
                        "id":"resp_tools",
                        "output":[
                            {
                                "type":"custom_tool_call",
                                "call_id":"call_patch",
                                "name":"apply_patch",
                                "input":"*** Begin Patch\n*** End Patch"
                            },
                            {
                                "type":"tool_search_call",
                                "call_id":"call_search",
                                "status":"completed",
                                "execution":"client",
                                "arguments":{"query":"mail tools"}
                            }
                        ]
                    })
                )
                .await,
            2
        );

        let mut request = json!({
            "previous_response_id":"resp_tools",
            "input":[
                {"type":"custom_tool_call_output","call_id":"call_patch","output":"patched"},
                {"type":"tool_search_output","call_id":"call_search","tools":[]}
            ]
        });

        assert_eq!(history.enrich_request(&mut request).await, 2);
        let input = request["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "custom_tool_call");
        assert_eq!(input[0]["input"], "*** Begin Patch\n*** End Patch");
        assert_eq!(input[1]["type"], "tool_search_call");
        assert_eq!(input[2]["type"], "custom_tool_call_output");
        assert_eq!(input[3]["type"], "tool_search_output");
    }

    #[tokio::test]
    async fn preserves_scalar_and_single_object_input_when_no_change_is_needed() {
        let history = CodexHistoryStore::default();
        let mut scalar_request = json!({"input":"hello"});
        assert_eq!(history.enrich_request(&mut scalar_request).await, 0);
        assert_eq!(scalar_request["input"], "hello");

        let mut request = json!({
            "input": {"type":"message","role":"user","content":"hello"}
        });

        assert_eq!(history.enrich_request(&mut request).await, 0);
        assert!(request["input"].is_object());
        assert_eq!(request["input"]["content"], "hello");
    }

    #[tokio::test]
    async fn concurrent_recording_is_safe_and_searchable() {
        let history = Arc::new(CodexHistoryStore::default());
        let scope = "session-1";
        let mut tasks = Vec::new();
        for index in 0..16 {
            let history = history.clone();
            tasks.push(tokio::spawn(async move {
                history
                    .record_response_scoped(
                        scope,
                        &json!({ "input": [] }),
                        &json!({
                            "id": format!("resp_{index}"),
                            "output": [{
                                "type":"function_call",
                                "call_id":format!("call_{index}"),
                                "name":"work",
                                "arguments":"{}"
                            }]
                        }),
                    )
                    .await
            }));
        }
        for task in tasks {
            assert_eq!(task.await.unwrap(), 1);
        }

        let mut request = json!({
            "input":[{
                "type":"function_call_output",
                "call_id":"call_9",
                "output":"done"
            }]
        });
        assert_eq!(
            history
                .enrich_request_scoped(scope, true, &mut request)
                .await,
            1
        );
        assert_eq!(request["input"][0]["call_id"], "call_9");
        assert_eq!(request["input"][0]["name"], "work");
    }

    #[test]
    fn byte_budget_evicts_oldest_responses_and_cleans_call_index() {
        let old_request = vec![json!({
            "type":"message","role":"user","content":"old request"
        })];
        let old_output = vec![json!({
            "type":"function_call",
            "call_id":"shared_call",
            "name":"old_tool",
            "arguments":"{\"value\":\"old\"}"
        })];
        let new_request = vec![json!({
            "type":"message","role":"user","content":"new request"
        })];
        let new_output = vec![json!({
            "type":"function_call",
            "call_id":"shared_call",
            "name":"new_tool",
            "arguments":"{\"value\":\"new\"}"
        })];
        let mut probe = HistoryInner::default();
        let scope = "session-1";
        let new_key = (scope.to_string(), "resp_new".to_string());
        let old_key = (scope.to_string(), "resp_old".to_string());
        probe.insert_response(scope, "resp_new", new_request.clone(), new_output.clone());
        let newest_size = probe.responses[&new_key].serialized_bytes;

        let mut inner = HistoryInner::default();
        assert_eq!(
            inner.insert_response_with_limits(
                scope,
                "resp_old",
                old_request,
                old_output,
                MAX_CACHED_RESPONSES,
                usize::MAX,
            ),
            1
        );
        assert_eq!(
            inner.insert_response_with_limits(
                scope,
                "resp_new",
                new_request,
                new_output,
                MAX_CACHED_RESPONSES,
                newest_size,
            ),
            1
        );

        assert_eq!(inner.cached_bytes, newest_size);
        assert!(!inner.responses.contains_key(&old_key));
        assert!(inner.responses.contains_key(&new_key));
        assert_eq!(
            inner
                .unique_call(scope, "shared_call")
                .and_then(|item| item.get("name"))
                .and_then(Value::as_str),
            Some("new_tool")
        );
    }

    #[test]
    fn same_id_replacement_keeps_exact_accounting_and_no_stale_call_index() {
        let mut inner = HistoryInner::default();
        let scope = "session-1";
        let response_key = (scope.to_string(), "resp_same".to_string());
        let call_key = (scope.to_string(), "call_new".to_string());
        assert_eq!(
            inner.insert_response(
                scope,
                "resp_same",
                vec![json!({"type":"message","role":"user","content":"short"})],
                vec![json!({
                    "type":"function_call","call_id":"call_old",
                    "name":"old_tool","arguments":"{}"
                })],
            ),
            1
        );
        assert_eq!(
            inner.insert_response(
                scope,
                "resp_same",
                vec![json!({
                    "type":"message","role":"user",
                    "content":"a longer authoritative replacement"
                })],
                vec![json!({
                    "type":"function_call","call_id":"call_new",
                    "name":"new_tool","arguments":"{\"ok\":true}"
                })],
            ),
            1
        );

        let replacement_bytes = inner.responses[&response_key].serialized_bytes;
        assert_eq!(inner.response_order.len(), 1);
        assert_eq!(inner.cached_bytes, replacement_bytes);
        assert!(inner.unique_call(scope, "call_old").is_none());
        assert_eq!(
            inner
                .unique_call(scope, "call_new")
                .and_then(|item| item.get("name"))
                .and_then(Value::as_str),
            Some("new_tool")
        );

        // Replaying the same completed response must not duplicate order/index entries or bytes.
        let request = inner.responses[&response_key].request_input.clone();
        let output = inner.responses[&response_key].output.clone();
        assert_eq!(
            inner.insert_response(scope, "resp_same", request, output),
            1
        );
        assert_eq!(inner.response_order.len(), 1);
        assert_eq!(inner.cached_bytes, replacement_bytes);
        assert_eq!(inner.call_index[&call_key].len(), 1);
    }

    #[test]
    fn oversized_insert_preserves_unrelated_entries_and_drops_stale_replacement() {
        let keep_request = vec![json!({
            "type":"message","role":"user","content":"keep"
        })];
        let keep_output = vec![json!({
            "type":"function_call","call_id":"call_keep",
            "name":"keep_tool","arguments":"{}"
        })];
        let oversized_request = vec![json!({
            "type":"message","role":"user","content":"x".repeat(2048)
        })];
        let oversized_output = vec![json!({
            "type":"function_call","call_id":"call_huge",
            "name":"huge_tool","arguments":"y".repeat(2048)
        })];

        let scope = "session-1";
        let keep_key = (scope.to_string(), "resp_keep".to_string());
        let mut probe = HistoryInner::default();
        probe.insert_response(
            scope,
            "resp_keep",
            keep_request.clone(),
            keep_output.clone(),
        );
        let budget = probe.responses[&keep_key].serialized_bytes;

        let mut inner = HistoryInner::default();
        assert_eq!(
            inner.insert_response_with_limits(
                scope,
                "resp_keep",
                keep_request,
                keep_output,
                MAX_CACHED_RESPONSES,
                budget,
            ),
            1
        );
        assert_eq!(
            inner.insert_response_with_limits(
                scope,
                "resp_huge",
                oversized_request.clone(),
                oversized_output.clone(),
                MAX_CACHED_RESPONSES,
                budget,
            ),
            0
        );
        assert!(inner.responses.contains_key(&keep_key));
        assert_eq!(inner.cached_bytes, budget);
        assert!(inner.unique_call(scope, "call_huge").is_none());

        assert_eq!(
            inner.insert_response_with_limits(
                scope,
                "resp_keep",
                oversized_request,
                oversized_output,
                MAX_CACHED_RESPONSES,
                budget,
            ),
            0
        );
        assert!(inner.responses.is_empty());
        assert!(inner.response_order.is_empty());
        assert!(inner.call_index.is_empty());
        assert_eq!(inner.cached_bytes, 0);
    }

    #[tokio::test]
    async fn native_history_materializes_across_provider_boundaries_and_strips_previous_id() {
        let history = CodexHistoryStore::default();
        let scope = "session-native";
        history
            .record_response_scoped_with_metadata(
                scope,
                ResponseOrigin::Native("provider-a".to_string()),
                true,
                &json!({
                    "input":[{"type":"message","role":"user","content":"first"}]
                }),
                &json!({
                    "id":"resp_native_a","status":"completed",
                    "output":[{
                        "type":"message","id":"msg_native_a","role":"assistant",
                        "content":[{"type":"output_text","text":"answer"}]
                    }]
                }),
            )
            .await;
        let mut next = json!({
            "previous_response_id":"resp_native_a",
            "input":[{"type":"message","role":"user","content":"second"}]
        });

        let resolution = history
            .materialize_request_scoped(scope, true, &mut next)
            .await;
        assert!(resolution.previous_found);
        assert!(resolution.previous_materialized);
        assert_eq!(
            resolution.previous_origin,
            Some(ResponseOrigin::Native("provider-a".to_string()))
        );
        assert!(next.get("previous_response_id").is_none());
        let decoded = crate::protocol::openai_responses::decode_request(&next).unwrap();
        assert_eq!(
            decoded
                .messages
                .iter()
                .map(|message| message.content_as_text())
                .collect::<Vec<_>>(),
            vec!["first", "answer", "second"]
        );
    }

    #[tokio::test]
    async fn owner_only_native_history_is_reported_but_never_materialized() {
        let history = CodexHistoryStore::default();
        let scope = "session-owner-only";
        history
            .record_response_scoped_with_metadata(
                scope,
                ResponseOrigin::Native("provider-a".to_string()),
                false,
                &json!({
                    "previous_response_id":"unknown-before-restart",
                    "input":[{"type":"message","role":"user","content":"delta"}]
                }),
                &json!({
                    "id":"resp_owner_only","status":"completed",
                    "output":[{
                        "type":"message","id":"msg_owner_only","role":"assistant",
                        "content":[{"type":"output_text","text":"answer"}]
                    }]
                }),
            )
            .await;
        let mut next = json!({
            "previous_response_id":"resp_owner_only",
            "input":[{"type":"message","role":"user","content":"next"}]
        });
        let before = next.clone();

        let resolution = history
            .materialize_request_scoped(scope, true, &mut next)
            .await;
        assert!(resolution.previous_found);
        assert!(!resolution.previous_materialized);
        assert_eq!(
            resolution.previous_origin,
            Some(ResponseOrigin::Native("provider-a".to_string()))
        );
        assert_eq!(next, before);
    }

    #[tokio::test]
    async fn incomplete_response_remains_resumable_history() {
        let history = CodexHistoryStore::default();
        let scope = "session-incomplete";
        history
            .record_response_scoped_with_metadata(
                scope,
                ResponseOrigin::Local,
                true,
                &json!({
                    "input":[{"type":"message","role":"user","content":"write a lot"}]
                }),
                &json!({
                    "id":"resp_incomplete","status":"incomplete",
                    "incomplete_details":{"reason":"max_output_tokens"},
                    "output":[{
                        "type":"message","id":"msg_partial","role":"assistant",
                        "content":[{"type":"output_text","text":"partial"}]
                    }]
                }),
            )
            .await;
        let mut next = json!({
            "previous_response_id":"resp_incomplete",
            "input":[{"type":"message","role":"user","content":"continue"}]
        });

        let resolution = history
            .materialize_request_scoped(scope, true, &mut next)
            .await;
        assert!(resolution.previous_materialized);
        assert!(next.get("previous_response_id").is_none());
        let decoded = crate::protocol::openai_responses::decode_request(&next).unwrap();
        assert_eq!(
            decoded
                .messages
                .iter()
                .map(|message| message.content_as_text())
                .collect::<Vec<_>>(),
            vec!["write a lot", "partial", "continue"]
        );
    }

    #[tokio::test]
    async fn metadata_recording_rejects_non_resumable_terminals() {
        let history = CodexHistoryStore::default();
        let request = json!({
            "input":[{"type":"message","role":"user","content":"hello"}]
        });
        for response in [
            json!({"id":"resp_failed","status":"failed","output":[]}),
            json!({"id":"resp_partial","output":[]}),
            json!({
                "id":"resp_compaction","object":"response.compaction","status":"completed",
                "output":[{"type":"compaction","encrypted_content":"opaque"}]
            }),
        ] {
            assert_eq!(
                history
                    .record_response_scoped_with_metadata(
                        "session-terminal",
                        ResponseOrigin::Native("provider-a".to_string()),
                        true,
                        &request,
                        &response,
                    )
                    .await,
                0
            );
            assert!(history
                .response_metadata("session-terminal", response["id"].as_str().unwrap())
                .await
                .is_none());
        }
    }

    #[tokio::test]
    async fn unsupported_output_and_owner_only_calls_never_become_portable_fallback() {
        let history = CodexHistoryStore::default();
        let scope = "session-partial";
        history
            .record_response_scoped_with_metadata(
                scope,
                ResponseOrigin::Native("provider-a".to_string()),
                true,
                &json!({"input":[{"type":"message","role":"user","content":"look"}]}),
                &json!({
                    "id":"resp_unsupported","object":"response","status":"completed",
                    "output":[{"type":"computer_call","id":"computer_1"}]
                }),
            )
            .await;
        assert_eq!(
            history
                .response_metadata(scope, "resp_unsupported")
                .await
                .unwrap(),
            ResponseMetadata {
                origin: ResponseOrigin::Native("provider-a".to_string()),
                materializable: false,
            }
        );

        history
            .record_response_scoped_with_metadata(
                scope,
                ResponseOrigin::Native("provider-a".to_string()),
                false,
                &json!({
                    "previous_response_id":"missing-prefix",
                    "input":[{"type":"message","role":"user","content":"run"}]
                }),
                &json!({
                    "id":"resp_owner_call","status":"completed",
                    "output":[{
                        "type":"function_call","call_id":"call_owner_only",
                        "name":"shell","arguments":"{}"
                    }]
                }),
            )
            .await;
        let mut fallback = json!({
            "input":[{
                "type":"function_call_output","call_id":"call_owner_only","output":"ok"
            }]
        });
        assert_eq!(
            history
                .enrich_request_scoped(scope, true, &mut fallback)
                .await,
            0
        );
        assert_eq!(fallback["input"].as_array().unwrap().len(), 1);

        history
            .record_response_scoped_with_metadata(
                scope,
                ResponseOrigin::Native("provider-a".to_string()),
                true,
                &json!({
                    "input":[{"type":"compaction","encrypted_content":"opaque-prefix"}]
                }),
                &json!({
                    "id":"resp_compacted_input","status":"completed",
                    "output":[{
                        "type":"message","role":"assistant",
                        "content":[{"type":"output_text","text":"answer"}]
                    }]
                }),
            )
            .await;
        assert!(
            !history
                .response_metadata(scope, "resp_compacted_input")
                .await
                .unwrap()
                .materializable
        );
    }

    #[tokio::test]
    async fn empty_output_does_not_collapse_an_identical_follow_up_input() {
        let history = CodexHistoryStore::default();
        history
            .record_response_scoped_with_metadata(
                "session-empty-output",
                ResponseOrigin::Local,
                true,
                &json!({"input":"ping"}),
                &json!({"id":"resp_empty","status":"completed","output":[]}),
            )
            .await;
        let mut next = json!({
            "previous_response_id":"resp_empty",
            "input":"ping"
        });

        let resolution = history
            .materialize_request_scoped("session-empty-output", true, &mut next)
            .await;
        assert_eq!(resolution.changed, 1);
        assert!(next.get("previous_response_id").is_none());
        let input = next["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["content"], "ping");
        assert_eq!(input[1]["content"], "ping");
    }

    #[tokio::test]
    async fn call_fallback_uses_one_complete_branch_and_never_grafts_onto_previous() {
        let history = CodexHistoryStore::default();
        let scope = "session-fallback-branch";
        for (response_id, call_id, prompt) in [
            ("resp_a", "call_a", "branch a"),
            ("resp_b", "call_b", "branch b"),
        ] {
            history
                .record_response_scoped_with_metadata(
                    scope,
                    ResponseOrigin::Local,
                    true,
                    &json!({
                        "input":[{"type":"message","role":"user","content":prompt}]
                    }),
                    &json!({
                        "id":response_id,"status":"completed",
                        "output":[{
                            "type":"function_call","call_id":call_id,
                            "name":"lookup","arguments":"{}"
                        }]
                    }),
                )
                .await;
        }

        let mut one_branch = json!({
            "input":[{
                "type":"function_call_output","call_id":"call_a","output":"a"
            }]
        });
        let resolution = history
            .materialize_request_scoped(scope, true, &mut one_branch)
            .await;
        assert!(!resolution.had_previous_response_id);
        assert_eq!(resolution.changed, 2);
        assert_eq!(one_branch["input"][0]["content"], "branch a");
        assert_eq!(one_branch["input"][1]["call_id"], "call_a");
        assert_eq!(one_branch["input"][2]["call_id"], "call_a");
        history
            .record_response_scoped_with_metadata(
                scope,
                ResponseOrigin::Local,
                true,
                &one_branch,
                &json!({
                    "id":"resp_after_fallback","status":"completed",
                    "output":[{
                        "type":"message","role":"assistant",
                        "content":[{"type":"output_text","text":"done"}]
                    }]
                }),
            )
            .await;
        let mut switched_provider = json!({
            "previous_response_id":"resp_after_fallback",
            "input":"next"
        });
        let switched = history
            .materialize_request_scoped(scope, true, &mut switched_provider)
            .await;
        assert!(switched.previous_materialized);
        assert!(switched_provider.get("previous_response_id").is_none());
        assert_eq!(switched_provider["input"][0]["content"], "branch a");

        let mut mixed_branches = json!({
            "input":[
                {"type":"function_call_output","call_id":"call_a","output":"a"},
                {"type":"function_call_output","call_id":"call_b","output":"b"}
            ]
        });
        assert_eq!(
            history
                .enrich_request_scoped(scope, true, &mut mixed_branches)
                .await,
            0
        );
        assert_eq!(mixed_branches["input"].as_array().unwrap().len(), 2);

        let mut unrelated_to_previous = json!({
            "previous_response_id":"resp_a",
            "input":[{
                "type":"function_call_output","call_id":"call_b","output":"b"
            }]
        });
        history
            .materialize_request_scoped(scope, true, &mut unrelated_to_previous)
            .await;
        let input = unrelated_to_previous["input"].as_array().unwrap();
        assert!(input.iter().any(|item| item["call_id"] == "call_a"));
        assert!(!input
            .iter()
            .any(|item| { item["type"] == "function_call" && item["call_id"] == "call_b" }));
        assert!(crate::protocol::openai_responses::decode_request(&unrelated_to_previous).is_err());
    }

    #[tokio::test]
    async fn previous_response_is_resolved_and_materialized_without_new_input() {
        let history = CodexHistoryStore::default();
        let scope = "session-no-input";
        history
            .record_response_scoped_with_metadata(
                scope,
                ResponseOrigin::Native("provider-a".to_string()),
                true,
                &json!({
                    "input":[{"type":"message","role":"user","content":"first"}]
                }),
                &json!({
                    "id":"resp_no_input","status":"completed",
                    "output":[{
                        "type":"message","id":"msg_no_input","role":"assistant",
                        "content":[{"type":"output_text","text":"answer"}]
                    }]
                }),
            )
            .await;
        let mut next = json!({"previous_response_id":"resp_no_input"});

        let resolution = history
            .materialize_request_scoped(scope, true, &mut next)
            .await;
        assert!(resolution.had_previous_response_id);
        assert!(resolution.previous_found);
        assert!(resolution.previous_materialized);
        assert_eq!(
            resolution.previous_origin,
            Some(ResponseOrigin::Native("provider-a".to_string()))
        );
        assert!(next.get("previous_response_id").is_none());
        let input = next["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["content"], "first");
        assert_eq!(input[1]["content"][0]["text"], "answer");
    }
}
