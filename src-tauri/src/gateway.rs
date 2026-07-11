// Gateway core.
//
// Implements deterministic model routing and the localhost reverse proxy: header sanitizing,
// upstream forwarding, 429 retry, SSE streaming with model rewrite + usage sniffing, buffered-JSON
// model rewrite, /v1/models merge/synthesize, count_tokens fallback, HEAD / fallback, and bounded
// monitor exchange capture.
#![allow(dead_code)]

use axum::{
    body::{to_bytes, Body},
    extract::State,
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri},
    response::Response,
    Router,
};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tauri::Emitter;
use tokio::sync::{oneshot, Mutex};

use crate::store;

/// Default Claude tier models ccbud advertises to Claude-family clients (Claude Code,
/// Claude Desktop). Second field is the Claude Desktop `anthropicFamilyTier` keyword.
pub const CLAUDE_TIER_MODELS: &[(&str, &str)] = &[
    ("claude-fable-5", "opus"),
    ("claude-opus-4-8", "opus"),
    ("claude-sonnet-5", "sonnet"),
    ("claude-haiku-4-5", "haiku"),
    ("claude-haiku-4-5-20251001", "haiku"),
];

/// Default models ccbud advertises to Codex/OpenAI-family clients. `-sol`/`-terra`
/// are the primary tier; the rest (e.g. `-luna`) are the fast tier — see model_family.
pub const CODEX_TIER_MODELS: &[&str] = &["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"];

/// Which coding-agent family a model name belongs to. Claude Code sends `claude-*`,
/// Codex sends `gpt-*`; each names its primary vs fast tier differently.
enum ModelFamily {
    Claude,
    Codex,
    Other,
}
fn model_family(name: &str) -> ModelFamily {
    let n = name.to_ascii_lowercase();
    if n.starts_with("claude-") || n.starts_with("claude_") {
        ModelFamily::Claude
    } else if n.starts_with("gpt-") || n.starts_with("gpt_") {
        ModelFamily::Codex
    } else {
        ModelFamily::Other
    }
}
/// Claude fast/light tier = the haiku models; fable/opus/sonnet (and any other
/// claude-*) route to the primary model.
fn is_claude_fast(name: &str) -> bool {
    name.to_ascii_lowercase().contains("haiku")
}
/// Codex primary tier = `gpt-<ver>-sol` / `gpt-<ver>-terra` (trailing segment). Every
/// other gpt-* (e.g. `-luna`, `-mini`) routes to the fast model.
fn is_codex_primary(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    let last = n.rsplit('-').next().unwrap_or("");
    last == "sol" || last == "terra"
}
/// True if the request comes from a Codex/OpenAI-family client (vs Claude), detected by
/// the client's self-reported identity — User-Agent, or Codex's `originator` header.
fn client_is_codex(h: &HeaderMap) -> bool {
    let field = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
    field("user-agent").contains("codex") || field("originator").contains("codex")
}

#[derive(Debug, Clone)]
pub struct Routing {
    pub provider_id: String,
    pub outgoing_model: Option<String>,
    pub client_facing_model: Option<String>,
}

/// Decide how to route a request and translate its model name. Mirrors proxy.js `resolveRouting`.
pub fn resolve_routing(
    requested_model: Option<&str>,
    config: &Value,
    known_models: Option<&HashSet<String>>,
) -> Option<Routing> {
    let providers = config.get("providers")?.as_array()?;
    if providers.is_empty() {
        return None;
    }
    let active_id = config.get("activeProviderId").and_then(|v| v.as_str());
    let active = providers
        .iter()
        .find(|p| p.get("id").and_then(|v| v.as_str()) == active_id)
        .or_else(|| providers.first())?;
    let pid = active.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();

    let pass = |m: &str| {
        Some(Routing {
            provider_id: pid.clone(),
            outgoing_model: Some(m.to_string()),
            client_facing_model: Some(m.to_string()),
        })
    };

    let requested = match requested_model {
        None => {
            return Some(Routing {
                provider_id: pid.clone(),
                outgoing_model: None,
                client_facing_model: None,
            })
        }
        Some(m) => m,
    };

    let primary = active.get("defaultModel").and_then(|v| v.as_str()).unwrap_or("");
    let light = active.get("smallFastModel").and_then(|v| v.as_str()).unwrap_or("");
    let models = active.get("models").and_then(|v| v.as_array());

    if let Some(ms) = models {
        for m in ms {
            let alias = m.get("alias").and_then(|v| v.as_str()).unwrap_or("");
            let upstream = m.get("upstream").and_then(|v| v.as_str()).unwrap_or("");
            if !alias.is_empty() && alias == requested && !upstream.is_empty() {
                return Some(Routing {
                    provider_id: pid.clone(),
                    outgoing_model: Some(upstream.to_string()),
                    client_facing_model: Some(requested.to_string()),
                });
            }
        }
    }
    if requested == primary || requested == light {
        return pass(requested);
    }
    if let Some(ms) = models {
        for m in ms {
            if m.get("upstream").and_then(|v| v.as_str()) == Some(requested) {
                return pass(requested);
            }
        }
    }
    if let Some(known) = known_models {
        if known.contains(requested) {
            return pass(requested);
        }
    }
    // Codex connects with the sentinel model "gpt-5.5-ccbud" — a name Codex's model-family
    // detection accepts (gpt-5.5 prefix), so it doesn't warn about an unknown model. Route the
    // sentinel to the active provider's PRIMARY model (never the lightweight fallback).
    if requested.ends_with("-ccbud") {
        let target = if !primary.is_empty() { primary } else { light };
        if !target.is_empty() {
            return Some(Routing {
                provider_id: pid.clone(),
                outgoing_model: Some(target.to_string()),
                client_facing_model: Some(requested.to_string()),
            });
        }
    }
    let map_default = active
        .get("mapDefaultModels")
        .map(|v| v.as_bool().unwrap_or(true))
        .unwrap_or(true);
    if !map_default {
        return pass(requested);
    }
    let big = if !primary.is_empty() { primary } else { light };
    let small = if !light.is_empty() { light } else { primary };
    // Claude and Codex name their primary vs fast tiers differently, so classify by
    // family: claude-haiku* → fast, other claude-* → primary; gpt-*-sol / gpt-*-terra
    // → primary, other gpt-* → fast; anything else → fast.
    let target = match model_family(requested) {
        ModelFamily::Claude => if is_claude_fast(requested) { small } else { big },
        ModelFamily::Codex => if is_codex_primary(requested) { big } else { small },
        ModelFamily::Other => small,
    };
    if !target.is_empty() {
        return Some(Routing {
            provider_id: pid.clone(),
            outgoing_model: Some(target.to_string()),
            client_facing_model: Some(requested.to_string()),
        });
    }
    pass(requested)
}

// ---------------- gateway runtime ----------------

pub struct GatewayState {
    app: tauri::AppHandle,
    known: Mutex<HashMap<String, HashSet<String>>>,
    seq: AtomicU64,
    running: Mutex<Option<RunningServer>>,
    // Sync mirror of the bound port (0 = stopped) for callers that can't await (tray refresh).
    running_port: std::sync::atomic::AtomicU32,
    exchanges: Mutex<VecDeque<Value>>,
    client: reqwest::Client,
    client_insecure: reqwest::Client,
    // Ring buffer of recent gateway log lines (seq+ts stamped) so the settings Logs panel can
    // backfill on open — mirrors main.js gatewayLogs (cap 80). std Mutex: log() is sync.
    logs: std::sync::Mutex<VecDeque<Value>>,
    log_seq: AtomicU64,
}
struct RunningServer {
    port: u16,
    shutdown: oneshot::Sender<()>,
}

impl GatewayState {
    pub fn new(app: tauri::AppHandle) -> Arc<Self> {
        let client = reqwest::Client::builder()
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        let client_insecure = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Arc::new(Self {
            app,
            known: Mutex::new(HashMap::new()),
            seq: AtomicU64::new(0),
            running: Mutex::new(None),
            running_port: std::sync::atomic::AtomicU32::new(0),
            exchanges: Mutex::new(VecDeque::new()),
            client,
            client_insecure,
            logs: std::sync::Mutex::new(VecDeque::new()),
            log_seq: AtomicU64::new(0),
        })
    }

    pub fn log(&self, level: &str, msg: impl AsRef<str>) {
        let seq = self.log_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let entry = json!({ "seq": seq, "ts": ts, "level": level, "msg": msg.as_ref() });
        if let Ok(mut buf) = self.logs.lock() {
            buf.push_back(entry.clone());
            while buf.len() > 80 {
                buf.pop_front();
            }
        }
        let _ = self.app.emit("gateway:log", entry);
    }

    /// Snapshot of the recent-log ring, oldest→newest (logs_get backfill).
    pub fn logs_snapshot(&self) -> Value {
        self.logs
            .lock()
            .map(|b| Value::Array(b.iter().cloned().collect()))
            .unwrap_or_else(|_| json!([]))
    }
    pub fn logs_clear(&self) {
        if let Ok(mut b) = self.logs.lock() {
            b.clear();
        }
    }

    pub async fn status(&self) -> Value {
        match self.running.lock().await.as_ref() {
            Some(rs) => json!({ "running": true, "port": rs.port }),
            None => json!({ "running": false, "port": Value::Null }),
        }
    }

    pub async fn current_port(&self) -> Option<u16> {
        self.running.lock().await.as_ref().map(|r| r.port)
    }

    /// Sync view of the running state (tray menu refresh runs on the main thread, no await).
    pub fn port_sync(&self) -> Option<u16> {
        match self.running_port.load(Ordering::Relaxed) {
            0 => None,
            p => Some(p as u16),
        }
    }

    pub fn emit(&self, event: &str, payload: Value) {
        let _ = self.app.emit(event, payload);
    }

    pub async fn start(self: &Arc<Self>, port: u16) -> Result<u16, String> {
        if let Some(rs) = self.running.lock().await.as_ref() {
            return Ok(rs.port);
        }
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .map_err(|e| e.to_string())?;
        let actual = listener.local_addr().map_err(|e| e.to_string())?.port();
        let (tx, rx) = oneshot::channel::<()>();
        let router = Router::new().fallback(handle).with_state(self.clone());
        tauri::async_runtime::spawn(async move {
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let _ = rx.await;
                })
                .await;
        });
        *self.running.lock().await = Some(RunningServer { port: actual, shutdown: tx });
        self.running_port.store(actual as u32, Ordering::Relaxed);
        self.log("info", format!("gateway listening on http://127.0.0.1:{}", actual));
        let status = self.status().await;
        let _ = self.app.emit("gateway:status", status);
        Ok(actual)
    }

    pub async fn stop(self: &Arc<Self>) {
        self.running_port.store(0, Ordering::Relaxed);
        let taken = self.running.lock().await.take();
        if let Some(rs) = taken {
            let _ = rs.shutdown.send(());
            self.log("info", "gateway stopped");
        }
        let status = self.status().await;
        let _ = self.app.emit("gateway:status", status);
    }

    fn next_id(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed) + 1
    }
    /// Bounded live-debugging capture: keep only the most recent exchanges (matches the monitor
    /// stream's 100-row window so every visible row can open its detail).
    pub async fn record_exchange(&self, ex: Value) {
        let mut buf = self.exchanges.lock().await;
        buf.push_back(ex);
        while buf.len() > 100 {
            buf.pop_front();
        }
    }
    pub async fn monitor_get(&self, id: i64) -> Value {
        let buf = self.exchanges.lock().await;
        buf.iter()
            .rev()
            .find(|e| e.get("id").and_then(|v| v.as_i64()) == Some(id))
            .cloned()
            .unwrap_or(Value::Null)
    }
    pub async fn monitor_clear(&self) {
        self.exchanges.lock().await.clear();
    }
    pub async fn monitor_recent(&self) -> Value {
        self.exchanges.lock().await.back().cloned().unwrap_or(Value::Null)
    }

    fn emit_request(&self, id: u64, started: std::time::Instant, method: &Method, path: &str, provider: &str, routing: &Routing, status: u16, usage: Option<&UsageAcc>) {
        let (it, ot, cr, cc) = usage
            .map(|u| (u.input, u.output, u.cache_read, u.cache_creation))
            .unwrap_or((0, 0, 0, 0));
        let _ = self.app.emit(
            "gateway:request",
            json!({
                "id": id,
                "method": method.as_str(),
                "path": path,
                "provider": provider,
                "requestedModel": routing.client_facing_model,
                "outgoingModel": routing.outgoing_model,
                "clientFacingModel": routing.client_facing_model,
                "status": status,
                "ms": started.elapsed().as_millis() as u64,
                "inputTokens": it, "outputTokens": ot, "cacheRead": cr, "cacheCreation": cc,
            }),
        );
    }
}

#[derive(Default, Clone)]
struct UsageAcc {
    input: i64,
    output: i64,
    cache_read: i64,
    cache_creation: i64,
    saw: bool,
}

/// Makes a streaming request visible in the monitor even when the client aborts mid-stream.
/// The row + exchange record are normally emitted at the END of the response generator; when the
/// client disconnects, axum simply drops the generator and that code never runs — the request
/// vanished from the request stream (Codex users interrupt turns constantly). The generator owns
/// this guard: `complete()` hands back the prepared exchange (bodies filled) for the normal path,
/// and Drop-without-complete emits the row + a record.
///
/// The response capture buffers live IN the guard rather than in generator locals: a dropped
/// generator then still records whatever already streamed through. This matters beyond real
/// aborts — Responses clients (Codex) tear the connection down the moment the terminal
/// `response.completed` event arrives, before upstream EOF, which used to lose BOTH response
/// bodies on every transcoded turn. When the transcoder has already emitted its terminal event
/// (`finished`), that disconnect is the normal end of a turn and is not flagged `aborted`.
struct StreamAbortGuard {
    armed: bool,
    st: Arc<GatewayState>,
    id: u64,
    started: std::time::Instant,
    method: Method,
    path: String,
    provider: String,
    routing: Routing,
    status: u16,
    ex: Value,
    res_cap: String,
    up_cap: Option<UpCapture>,
    finished: bool,
    usage: Option<UsageAcc>,
}

/// Raw upstream capture (pre-translation) for transcoded streams: status + headers are fixed at
/// guard construction, text accumulates as chunks arrive.
struct UpCapture {
    status: u16,
    headers: Value,
    text: String,
    total: usize,
}

const RES_CAP_MAX: usize = 2 * 1024 * 1024;
const UP_CAP_MAX: usize = 1024 * 1024;

impl StreamAbortGuard {
    #[allow(clippy::too_many_arguments)]
    fn new(
        st: Arc<GatewayState>,
        id: u64,
        started: std::time::Instant,
        method: Method,
        path: String,
        provider: String,
        routing: Routing,
        status: u16,
        ex: Value,
        upstream: Option<(u16, Value)>,
    ) -> Self {
        let up_cap = upstream.map(|(status, headers)| UpCapture { status, headers, text: String::new(), total: 0 });
        Self {
            armed: true, st, id, started, method, path, provider, routing, status, ex,
            res_cap: String::new(), up_cap, finished: false, usage: None,
        }
    }

    /// Append to the client-facing response capture (the translated stream for transcoded pairs).
    fn push_res(&mut self, s: &str) {
        if self.res_cap.len() < RES_CAP_MAX {
            self.res_cap.push_str(s);
        }
    }

    /// Append raw upstream bytes (pre-translation) when this guard tracks an upstream capture.
    fn push_up(&mut self, raw: &str) {
        if let Some(u) = self.up_cap.as_mut() {
            u.total += raw.len();
            if u.text.len() < UP_CAP_MAX {
                u.text.push_str(raw);
            }
        }
    }

    /// Write the captured bodies into the exchange skeleton — shared by normal and abort paths.
    fn fill_bodies(&mut self) {
        self.ex["resBody"] = json!({ "text": self.res_cap, "bytes": self.res_cap.len(), "truncated": 0 });
        if let Some(u) = self.up_cap.as_ref() {
            self.ex["upstreamRes"] = json!({ "status": u.status, "headers": u.headers,
                "body": { "text": u.text, "bytes": u.total, "truncated": u.total.saturating_sub(u.text.len()) } });
        }
    }

    /// Normal completion: disarm and hand the exchange (bodies filled) back to the caller (who
    /// fills in ms / usage and records it).
    fn complete(&mut self) -> Value {
        self.armed = false;
        self.fill_bodies();
        std::mem::take(&mut self.ex)
    }
}

impl Drop for StreamAbortGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.fill_bodies();
        let mut ex = std::mem::take(&mut self.ex);
        ex["ms"] = json!(self.started.elapsed().as_millis() as u64);
        // A disconnect after the transcoder's terminal event is the normal end of a Responses
        // turn — only flag genuinely interrupted streams.
        if !self.finished {
            ex["aborted"] = json!(true);
        }
        self.st.emit_request(self.id, self.started, &self.method, &self.path, &self.provider, &self.routing, self.status, self.usage.as_ref());
        let st = self.st.clone();
        // record_exchange is async and Drop is sync — spawn it, tolerating an already-torn-down
        // runtime at app quit.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            tauri::async_runtime::spawn(async move { st.record_exchange(ex).await });
        }));
    }
}

fn retry_delay(retry_after: Option<&str>, attempt: i64, base: i64) -> u64 {
    let cap = 30_000u64;
    if let Some(ra) = retry_after {
        let s = ra.trim();
        if let Ok(n) = s.parse::<u64>() {
            return (n.saturating_mul(1000)).min(cap);
        }
        // HTTP-date form (RFC 7231 IMF-fixdate) — honor the absolute time the upstream named
        // (proxy.js parity). chrono is already a dep, so no extra crate is pulled in for this.
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%a, %d %b %Y %H:%M:%S GMT") {
            let ms = (dt.and_utc() - chrono::Utc::now()).num_milliseconds().max(0) as u64;
            return ms.min(cap);
        }
    }
    let base = if base > 0 { base as u64 } else { 500 };
    base.saturating_mul(2u64.saturating_pow(attempt.clamp(0, 20) as u32))
        .min(8000)
}

fn model_rewrite_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r#"("model"\s*:\s*")[^"]*(")"#).unwrap())
}

fn absorb_usage_sse(obj: &Value, usage: &mut UsageAcc) {
    match obj.get("type").and_then(|v| v.as_str()) {
        Some("message_start") => {
            if let Some(u) = obj.get("message").and_then(|m| m.get("usage")) {
                usage.input += u.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                usage.cache_read += u.get("cache_read_input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                usage.cache_creation += u.get("cache_creation_input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                usage.saw = true;
            }
        }
        Some("message_delta") => {
            if let Some(o) = obj.get("usage").and_then(|u| u.get("output_tokens")).and_then(|v| v.as_i64()) {
                usage.output = o;
                usage.saw = true;
            }
        }
        _ => {}
    }
}

fn process_sse_line(line: &str, rewrite_model: Option<&str>, usage: &mut UsageAcc) -> String {
    if line.contains("\"usage\"") {
        if let Some(i) = line.find('{') {
            if let Ok(obj) = serde_json::from_str::<Value>(line[i..].trim()) {
                absorb_usage_sse(&obj, usage);
            }
        }
    }
    if let Some(m) = rewrite_model {
        if line.contains("\"model\"") {
            return model_rewrite_re()
                .replace_all(line, |caps: &regex::Captures| format!("{}{}{}", &caps[1], m, &caps[2]))
                .into_owned();
        }
    }
    line.to_string()
}

fn build_target(base_url: &str, uri: &Uri) -> Option<String> {
    if base_url.is_empty() {
        return None;
    }
    let base = base_url.trim_end_matches('/');
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    Some(format!("{}{}", base, pq))
}

fn error_response(status: StatusCode, msg: &str, etype: &str) -> Response {
    json_response(status, &json!({ "type": "error", "error": { "type": etype, "message": msg } }))
}
fn json_response(status: StatusCode, body: &Value) -> Response {
    let bytes = serde_json::to_vec(body).unwrap_or_default();
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(bytes))
        .unwrap()
}

// ---- /v1/models augmentation ----
fn model_entry(id: &str) -> Value {
    json!({ "type": "model", "id": id, "display_name": id, "created_at": "2025-01-01T00:00:00Z" })
}
fn alias_entries(config: &Value) -> Vec<Value> {
    let mut out = vec![];
    let mut seen = HashSet::new();
    if let Some(ps) = config.get("providers").and_then(|v| v.as_array()) {
        for p in ps {
            if let Some(ms) = p.get("models").and_then(|v| v.as_array()) {
                for m in ms {
                    if let Some(a) = m.get("alias").and_then(|v| v.as_str()) {
                        if !a.is_empty() && seen.insert(a.to_string()) {
                            out.push(model_entry(a));
                        }
                    }
                }
            }
        }
    }
    out
}
/// Default tier models for the requesting client's family (Codex → gpt tiers,
/// Claude → claude tiers).
fn tier_entries(is_codex: bool) -> Vec<Value> {
    if is_codex {
        CODEX_TIER_MODELS.iter().map(|n| model_entry(n)).collect()
    } else {
        CLAUDE_TIER_MODELS.iter().map(|(n, _)| model_entry(n)).collect()
    }
}
fn merge_models(upstream: &Value, config: &Value, is_codex: bool) -> Value {
    let data = upstream.get("data").and_then(|d| d.as_array()).cloned().unwrap_or_default();
    let mut have: HashSet<String> = data
        .iter()
        .filter_map(|m| m.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))
        .collect();
    let mut adds = vec![];
    for a in alias_entries(config).into_iter().chain(tier_entries(is_codex)) {
        let id = a.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if have.insert(id) {
            adds.push(a);
        }
    }
    let mut merged = upstream.clone();
    adds.extend(data);
    merged["data"] = json!(adds);
    merged
}
fn synthesize_models(config: &Value, is_codex: bool) -> Value {
    let mut out = alias_entries(config);
    if out.is_empty() {
        let ps = config.get("providers").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let active_id = config.get("activeProviderId").and_then(|v| v.as_str());
        let active = ps
            .iter()
            .find(|p| p.get("id").and_then(|v| v.as_str()) == active_id)
            .or_else(|| ps.first());
        let mut seen = HashSet::new();
        if let Some(a) = active {
            for k in ["defaultModel", "smallFastModel"] {
                if let Some(id) = a.get(k).and_then(|v| v.as_str()) {
                    if !id.is_empty() && seen.insert(id.to_string()) {
                        out.push(model_entry(id));
                    }
                }
            }
        }
    }
    let mut have: HashSet<String> = out
        .iter()
        .filter_map(|m| m.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))
        .collect();
    for e in tier_entries(is_codex) {
        let id = e.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if have.insert(id) {
            out.push(e);
        }
    }
    let first = out.first().and_then(|m| m.get("id").cloned()).unwrap_or(Value::Null);
    let last = out.last().and_then(|m| m.get("id").cloned()).unwrap_or(Value::Null);
    json!({ "data": out, "has_more": false, "first_id": first, "last_id": last })
}

fn redact_value(key: &str, val: &str) -> String {
    let k = key.to_ascii_lowercase();
    if matches!(k.as_str(), "authorization" | "x-api-key" | "cookie" | "set-cookie" | "proxy-authorization" | "x-goog-api-key") {
        "••••••（已隐藏）".to_string()
    } else {
        val.to_string()
    }
}
fn redact_headers(h: &HeaderMap) -> Value {
    let mut o = serde_json::Map::new();
    for (k, v) in h.iter() {
        o.insert(k.as_str().to_string(), Value::String(redact_value(k.as_str(), v.to_str().unwrap_or(""))));
    }
    Value::Object(o)
}
fn vec_headers(pairs: &[(String, String)]) -> Value {
    let mut o = serde_json::Map::new();
    for (k, v) in pairs {
        o.insert(k.clone(), Value::String(redact_value(k, v)));
    }
    Value::Object(o)
}
fn cap_text(bytes: &[u8], cap: usize) -> Value {
    let total = bytes.len();
    let end = total.min(cap);
    json!({ "text": String::from_utf8_lossy(&bytes[..end]), "bytes": total, "truncated": total.saturating_sub(cap) })
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

const HOP_BY_HOP_REQ: &[&str] = &[
    "host", "content-length", "authorization", "x-api-key", "accept-encoding", "cookie",
    "proxy-authorization", "connection", "proxy-connection", "transfer-encoding",
];
const HOP_BY_HOP_RES: &[&str] = &[
    "content-length", "transfer-encoding", "content-encoding", "connection", "keep-alive",
    "proxy-authenticate", "proxy-connection", "set-cookie",
];

/// The localhost reverse-proxy handler. Mirrors proxy.js `handle`.
async fn handle(State(st): State<Arc<GatewayState>>, req: axum::extract::Request) -> Response {
    let started = std::time::Instant::now();
    let (parts, body) = req.into_parts();
    let method = parts.method;
    let uri = parts.uri;
    let in_headers = parts.headers;
    let req_path = uri.path().to_string();
    let body_bytes = to_bytes(body, 64 * 1024 * 1024).await.unwrap_or_default();

    let config = store::read_config();

    // Optional local gateway token (defense in depth; already bound to localhost).
    if config.get("requireToken").and_then(|v| v.as_bool()).unwrap_or(false) {
        let token = config.get("gatewayToken").and_then(|v| v.as_str()).unwrap_or("");
        if !token.is_empty() {
            let auth = in_headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("");
            let bearer = auth
                .strip_prefix("Bearer ")
                .or_else(|| auth.strip_prefix("bearer "));
            let presented = bearer.unwrap_or_else(|| {
                in_headers.get("x-api-key").and_then(|v| v.to_str().ok()).unwrap_or("")
            });
            if presented != token {
                return error_response(StatusCode::UNAUTHORIZED, "ccbud: invalid gateway token", "authentication_error");
            }
        }
    }

    let is_json = in_headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.contains("application/json"))
        .unwrap_or(false);
    let mut parsed: Option<Value> = None;
    let mut requested_model: Option<String> = None;
    if !body_bytes.is_empty() && is_json {
        if let Ok(v) = serde_json::from_slice::<Value>(&body_bytes) {
            requested_model = v.get("model").and_then(|m| m.as_str()).map(|s| s.to_string());
            parsed = Some(v);
        }
    }

    let providers = config.get("providers").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let active_id = config.get("activeProviderId").and_then(|v| v.as_str());
    let active_pid = providers
        .iter()
        .find(|p| p.get("id").and_then(|v| v.as_str()) == active_id)
        .or_else(|| providers.first())
        .and_then(|p| p.get("id").and_then(|v| v.as_str()))
        .map(|s| s.to_string());
    let known = match &active_pid {
        Some(pid) => st.known.lock().await.get(pid).cloned(),
        None => None,
    };

    let routing = match resolve_routing(requested_model.as_deref(), &config, known.as_ref()) {
        Some(r) => r,
        None => {
            st.log("warn", "request rejected: no provider configured");
            return error_response(StatusCode::BAD_GATEWAY, "ccbud: no provider configured. Add one in the app.", "api_error");
        }
    };
    let provider = match providers.iter().find(|p| p.get("id").and_then(|v| v.as_str()) == Some(routing.provider_id.as_str())) {
        Some(p) => p,
        None => return error_response(StatusCode::BAD_GATEWAY, "ccbud: no provider configured.", "api_error"),
    };
    let base_url = provider.get("baseUrl").and_then(|v| v.as_str()).unwrap_or("");
    let auth_token = provider.get("authToken").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let provider_name = provider
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or(routing.provider_id.as_str())
        .to_string();

    let need_rewrite = match (routing.client_facing_model.as_ref(), routing.outgoing_model.as_ref()) {
        (Some(c), Some(o)) => c != o,
        _ => false,
    };
    let mut out_body = body_bytes.clone();
    if let (Some(p0), Some(out_model)) = (parsed.as_ref(), routing.outgoing_model.as_ref()) {
        if Some(out_model) != requested_model.as_ref() {
            let mut p = p0.clone();
            p["model"] = json!(out_model);
            if let Ok(b) = serde_json::to_vec(&p) {
                out_body = Bytes::from(b);
            }
        }
    }

    let mut target = match build_target(base_url, &uri) {
        Some(t) => t,
        None => return error_response(StatusCode::BAD_GATEWAY, "ccbud: invalid provider baseUrl", "api_error"),
    };
    let is_models_list = method == Method::GET && (req_path.ends_with("/v1/models") || req_path.ends_with("/v1/models/"));
    // Codex and Claude clients both GET /v1/models — tell them apart by client identity
    // so each gets its own family's default model list.
    let client_codex = client_is_codex(&in_headers);
    let is_head_root = method == Method::HEAD && req_path == "/";
    let is_count_tokens = method == Method::POST
        && (req_path.ends_with("/v1/messages/count_tokens") || req_path.ends_with("/v1/messages/count_tokens/"));

    // ---- protocol translation ----
    // When the client's wire protocol (inferred from the request path) differs from the provider's
    // declared protocol, translate the request into the provider's format and remember to translate
    // the response back. Same-protocol requests skip this entirely and keep the verbatim passthrough
    // fast path below (so Anthropic→Anthropic behavior is byte-for-byte unchanged). Streaming pairs
    // with an incremental transcoder (see protocol::stream::Transcoder) stream token-by-token; the
    // rest force the upstream buffered (stream=false) and synthesize the client SSE from the full
    // response.
    let client_wire = crate::protocol::Wire::from_request_path(&uri);
    let provider_wire =
        crate::protocol::Wire::from_provider(provider.get("protocol").and_then(|v| v.as_str()));
    // translate ctx: (client_wire, provider_wire, client_facing_model, client_wanted_stream, incremental)
    // `incremental` = we can transcode the upstream stream event-by-event to the client (true
    // token-by-token). Otherwise we force the upstream buffered and synthesize the client response.
    let mut translate: Option<(crate::protocol::Wire, crate::protocol::Wire, String, bool, bool)> = None;
    if client_wire != provider_wire && method == Method::POST && !is_models_list && !is_count_tokens {
        if let Some(p) = parsed.as_ref() {
            let wanted_stream = p.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
            let incremental = wanted_stream
                && crate::protocol::can_transcode_stream(provider_wire, client_wire);
            let client_model = routing.client_facing_model.clone().unwrap_or_default();
            let outgoing = routing.outgoing_model.clone().unwrap_or_default();
            match crate::protocol::decode_client_request(client_wire, p).and_then(|ir| {
                crate::protocol::encode_upstream_request(provider_wire, &ir, &outgoing, incremental)
            }) {
                Ok(mut body) => {
                    // Ask OpenAI-family upstreams to include usage in the final stream chunk.
                    if incremental && provider_wire == crate::protocol::Wire::OpenAiChat {
                        body["stream_options"] = json!({ "include_usage": true });
                    }
                    if let Ok(b) = serde_json::to_vec(&body) {
                        out_body = Bytes::from(b);
                    }
                    // Send to the provider protocol's endpoint (drop the inbound path/query).
                    target = provider_wire.upstream_url(base_url);
                    translate = Some((client_wire, provider_wire, client_model, wanted_stream, incremental));
                }
                Err(e) => {
                    st.log("error", format!("protocol translate ({:?}→{:?}) failed: {}", client_wire, provider_wire, e));
                    return error_response(StatusCode::BAD_GATEWAY, &format!("ccbud protocol translation failed: {}", e), "api_error");
                }
            }
        }
    }

    // upstream headers (sanitized + provider token swapped in)
    let mut up_headers = HeaderMap::new();
    for (k, v) in in_headers.iter() {
        let kn = k.as_str().to_ascii_lowercase();
        if HOP_BY_HOP_REQ.contains(&kn.as_str()) {
            continue;
        }
        up_headers.insert(k.clone(), v.clone());
    }
    up_headers.insert(axum::http::header::ACCEPT_ENCODING, HeaderValue::from_static("identity"));
    // A translated Anthropic upstream needs the anthropic-version header; OpenAI-family clients
    // (Codex) never send one.
    if translate.as_ref().map(|t| t.1) == Some(crate::protocol::Wire::Anthropic)
        && !up_headers.contains_key("anthropic-version")
    {
        up_headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
    }
    if !auth_token.is_empty() {
        // Auth via Authorization: Bearer only. Sending both authorization and x-api-key trips
        // providers that reject having the two auth headers present at once (matches provider_test).
        // Both inbound auth headers are already stripped by HOP_BY_HOP_REQ above.
        if let Ok(val) = HeaderValue::from_str(&format!("Bearer {}", auth_token)) {
            up_headers.insert(axum::http::header::AUTHORIZATION, val);
        }
    }

    let ex_id = st.next_id();
    let ex_req_headers = redact_headers(&up_headers);
    let ex_req_body = cap_text(&out_body, 4 * 1024 * 1024);
    let ex_url = target.clone();
    // Client-side view of the exchange — what the gateway RECEIVED, before any translation — so
    // the monitor can show a protocol translation's exact before/after (inbound URL/headers/body
    // vs. the upstream URL/headers/body above). The body is duplicated only when a translation
    // applies; for passthrough, reqBody already IS the client body (modulo the model rewrite).
    let ex_translated = translate.as_ref().map(|t| format!("{} → {}", t.0.label(), t.1.label()));
    let ex_client_req = {
        let mut o = json!({
            "url": uri.path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| req_path.clone()),
            "headers": redact_headers(&in_headers),
        });
        if ex_translated.is_some() {
            o["body"] = cap_text(&body_bytes, 1024 * 1024);
        }
        o
    };

    let insecure = config.get("insecureSkipVerify").and_then(|v| v.as_bool()).unwrap_or(false)
        && target.starts_with("https:");
    let client = if insecure { &st.client_insecure } else { &st.client };

    let rc = config.get("retry429").cloned().unwrap_or(json!({}));
    let retry_enabled = rc.get("enabled").map(|v| v.as_bool().unwrap_or(true)).unwrap_or(true);
    let retry_max = rc.get("max").and_then(|v| v.as_i64()).unwrap_or(3);
    let retry_base = rc.get("baseMs").and_then(|v| v.as_i64()).unwrap_or(500);

    // forward with 429 retry
    let mut attempt = 0i64;
    let resp = loop {
        let r = client
            .request(method.clone(), &target)
            .headers(up_headers.clone())
            .body(out_body.clone())
            .send()
            .await;
        match r {
            Ok(resp) => {
                if retry_enabled && resp.status().as_u16() == 429 && attempt < retry_max {
                    let ra = resp.headers().get("retry-after").and_then(|v| v.to_str().ok()).map(|s| s.to_string());
                    let delay = retry_delay(ra.as_deref(), attempt, retry_base);
                    st.log("warn", format!("upstream 429 — retry {}/{} in {}ms ({})", attempt + 1, retry_max, delay, provider_name));
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                    attempt += 1;
                    continue;
                }
                break resp;
            }
            Err(e) => {
                if is_models_list {
                    st.emit_request(ex_id, started, &method, &req_path, &provider_name, &routing, 200, None);
                    return json_response(StatusCode::OK, &synthesize_models(&config, client_codex));
                }
                if is_count_tokens {
                    let est = crate::counttokens::estimate_input_tokens(parsed.as_ref().unwrap_or(&Value::Null));
                    st.emit_request(ex_id, started, &method, &req_path, &provider_name, &routing, 200, None);
                    return Response::builder()
                        .status(200)
                        .header("content-type", "application/json")
                        .header("x-ccbud-tokens", "estimated")
                        .header("x-ccbud-upstream-status", "error")
                        .body(Body::from(serde_json::to_vec(&json!({ "input_tokens": est })).unwrap_or_default()))
                        .unwrap();
                }
                st.log("error", format!("upstream error: {}", e));
                st.emit_request(ex_id, started, &method, &req_path, &provider_name, &routing, 502, None);
                return error_response(StatusCode::BAD_GATEWAY, &format!("ccbud upstream error: {}", e), "api_error");
            }
        }
    };

    let status = resp.status();
    let ct = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();

    if is_head_root && status.as_u16() == 404 {
        st.log("info", format!("HEAD / fallback: upstream 404 → gateway 200 ({})", provider_name));
        st.emit_request(ex_id, started, &method, &req_path, &provider_name, &routing, 200, None);
        st.record_exchange(json!({
            "id": ex_id, "ts": now_ms(), "ms": started.elapsed().as_millis() as u64,
            "method": method.as_str(), "path": req_path, "url": ex_url,
            "provider": provider_name, "requestedModel": routing.client_facing_model,
            "outgoingModel": routing.outgoing_model, "clientFacingModel": routing.client_facing_model,
            "status": 200, "reqHeaders": ex_req_headers, "reqBody": ex_req_body,
            "clientReq": ex_client_req, "translated": ex_translated,
            "resHeaders": json!({ "x-ccbud-fallback": "head-root-404-to-200", "x-ccbud-upstream-status": "404" }),
            "resBody": json!({ "text": "", "bytes": 0, "truncated": 0 }),
        }))
        .await;
        return Response::builder()
            .status(200)
            .header("x-ccbud-fallback", "head-root-404-to-200")
            .header("x-ccbud-upstream-status", "404")
            .body(Body::empty())
            .unwrap();
    }

    let mut out_headers: Vec<(String, String)> = vec![];
    for (k, v) in resp.headers().iter() {
        let kn = k.as_str().to_ascii_lowercase();
        if HOP_BY_HOP_RES.contains(&kn.as_str()) {
            continue;
        }
        if let Ok(s) = v.to_str() {
            out_headers.push((k.as_str().to_string(), s.to_string()));
        }
    }

    // streaming SSE — rewrite model + sniff usage, line-buffered
    if ct.contains("text/event-stream") {
        // Incremental cross-protocol transcode: feed each upstream SSE line through a stateful
        // transcoder that emits the client protocol's events as they arrive (true token-by-token).
        if let Some((client_wire, provider_wire, mut tc)) = translate
            .as_ref()
            .filter(|t| t.4)
            .and_then(|t| {
                // can_transcode_stream guarded `incremental`, so new() matches a wired pair.
                crate::protocol::stream::Transcoder::new(t.1, t.0, &t.2).map(|tc| (t.0, t.1, tc))
            })
        {
            let st2 = st.clone();
            let status_code = status.as_u16();
            let ex_id2 = ex_id;
            let started2 = started;
            let xlabel = format!("{:?}->{:?}", provider_wire, client_wire);
            let up_res_headers = vec_headers(&out_headers);
            let mut guard = StreamAbortGuard::new(
                st.clone(), ex_id, started, method.clone(), req_path.clone(), provider_name.clone(),
                routing.clone(), status_code,
                json!({
                    "id": ex_id, "ts": now_ms(), "method": method.as_str(), "path": req_path, "url": ex_url,
                    "provider": provider_name, "requestedModel": routing.client_facing_model,
                    "outgoingModel": routing.outgoing_model, "clientFacingModel": routing.client_facing_model,
                    "status": status_code, "reqHeaders": ex_req_headers, "reqBody": ex_req_body,
                    "clientReq": ex_client_req, "translated": ex_translated,
                    "resHeaders": json!({ "content-type": "text/event-stream", "x-ccbud-translated": xlabel }),
                    "resBody": json!({ "text": "", "bytes": 0, "truncated": 0 }),
                }),
                // raw upstream capture (pre-translation), so the monitor can show the exact
                // upstream stream next to the translated one the client received
                Some((status_code, up_res_headers)),
            );
            let method2 = method.clone();
            let path2 = req_path.clone();
            let pname2 = provider_name.clone();
            let routing2 = routing.clone();
            let body_stream = async_stream::stream! {
                let mut s = resp.bytes_stream();
                let mut buf = String::new();
                while let Some(chunk) = s.next().await {
                    match chunk {
                        Ok(bytes) => {
                            let raw = String::from_utf8_lossy(&bytes);
                            guard.push_up(&raw);
                            buf.push_str(&raw);
                            let mut out = String::new();
                            while let Some(idx) = buf.find('\n') {
                                let line: String = buf.drain(..=idx).collect();
                                out.push_str(&tc.push(&line));
                            }
                            // Keep the guard current BEFORE suspending: once the terminal event is
                            // out, Codex closes the socket and the generator is dropped mid-await.
                            guard.push_res(&out);
                            guard.finished = tc.done();
                            guard.usage = Some(UsageAcc {
                                input: tc.input_tokens(), output: tc.output_tokens(), saw: true, ..Default::default()
                            });
                            if !out.is_empty() {
                                yield Ok::<Bytes, std::io::Error>(Bytes::from(out));
                            }
                        }
                        Err(_) => break,
                    }
                }
                let mut tail = String::new();
                if !buf.is_empty() { tail.push_str(&tc.push(&buf)); }
                tail.push_str(&tc.finish());
                if !tail.is_empty() {
                    guard.push_res(&tail);
                    yield Ok(Bytes::from(tail));
                }
                let mut usage = UsageAcc::default();
                usage.input = tc.input_tokens();
                usage.output = tc.output_tokens();
                usage.saw = true;
                st2.emit_request(ex_id2, started2, &method2, &path2, &pname2, &routing2, status_code, Some(&usage));
                let mut ex = guard.complete();
                ex["ms"] = json!(started2.elapsed().as_millis() as u64);
                st2.record_exchange(ex).await;
            };
            let mut builder = Response::builder()
                .status(status.as_u16())
                .header("content-type", "text/event-stream")
                .header("x-ccbud-translated", format!("{:?}->{:?}", provider_wire, client_wire));
            // Forward the upstream request id — clients (Claude Code) persist it as `requestId`,
            // which usage analytics use as half of the de-dup key.
            for (k, v) in &out_headers {
                if k == "request-id" || k == "x-request-id" {
                    builder = builder.header(k, v);
                }
            }
            return builder.body(Body::from_stream(body_stream)).unwrap();
        }
        let rewrite_model = if need_rewrite { routing.client_facing_model.clone() } else { None };
        let st2 = st.clone();
        let status_code = status.as_u16();
        let ex_id2 = ex_id;
        let started2 = started;
        let res_headers = vec_headers(&out_headers);
        let mut guard = StreamAbortGuard::new(
            st.clone(), ex_id, started, method.clone(), req_path.clone(), provider_name.clone(),
            routing.clone(), status_code,
            json!({
                "id": ex_id, "ts": now_ms(), "method": method.as_str(), "path": req_path, "url": ex_url,
                "provider": provider_name, "requestedModel": routing.client_facing_model,
                "outgoingModel": routing.outgoing_model, "clientFacingModel": routing.client_facing_model,
                "status": status_code, "reqHeaders": ex_req_headers, "reqBody": ex_req_body,
                "clientReq": ex_client_req, "translated": ex_translated,
                "resHeaders": res_headers,
                "resBody": json!({ "text": "", "bytes": 0, "truncated": 0 }),
            }),
            None,
        );
        let method2 = method.clone();
        let path2 = req_path.clone();
        let pname2 = provider_name.clone();
        let routing2 = routing.clone();
        let body_stream = async_stream::stream! {
            let mut s = resp.bytes_stream();
            let mut buf = String::new();
            let mut usage = UsageAcc::default();
            while let Some(chunk) = s.next().await {
                match chunk {
                    Ok(bytes) => {
                        buf.push_str(&String::from_utf8_lossy(&bytes));
                        let mut out = String::new();
                        while let Some(idx) = buf.find('\n') {
                            let line: String = buf.drain(..=idx).collect();
                            out.push_str(&process_sse_line(&line, rewrite_model.as_deref(), &mut usage));
                        }
                        guard.push_res(&out);
                        guard.usage = Some(usage.clone());
                        if !out.is_empty() {
                            yield Ok::<Bytes, std::io::Error>(Bytes::from(out));
                        }
                    }
                    Err(_) => break,
                }
            }
            if !buf.is_empty() {
                let line = process_sse_line(&buf, rewrite_model.as_deref(), &mut usage);
                guard.push_res(&line);
                yield Ok(Bytes::from(line));
            }
            st2.emit_request(ex_id2, started2, &method2, &path2, &pname2, &routing2, status_code, Some(&usage));
            let mut ex = guard.complete();
            ex["ms"] = json!(started2.elapsed().as_millis() as u64);
            st2.record_exchange(ex).await;
        };
        let mut builder = Response::builder().status(status.as_u16());
        for (k, v) in &out_headers {
            builder = builder.header(k, v);
        }
        return builder.body(Body::from_stream(body_stream)).unwrap();
    }

    // buffered (reqwest auto-decoded gzip/br/deflate)
    let buf = resp.bytes().await.unwrap_or_default();

    // count_tokens: pass the upstream's real number when it implements the endpoint; otherwise
    // (404 / non-JSON / missing input_tokens) estimate locally so Claude Code's sizing keeps working.
    if is_count_tokens {
        let upstream_ok = status.is_success()
            && serde_json::from_slice::<Value>(&buf)
                .ok()
                .and_then(|o| o.get("input_tokens").and_then(|v| v.as_i64()))
                .is_some();
        if upstream_ok {
            let mut builder = Response::builder().status(200).header("x-ccbud-tokens", "upstream");
            for (k, v) in &out_headers {
                builder = builder.header(k, v);
            }
            st.emit_request(ex_id, started, &method, &req_path, &provider_name, &routing, 200, None);
            return builder.body(Body::from(buf)).unwrap();
        }
        let est = crate::counttokens::estimate_input_tokens(parsed.as_ref().unwrap_or(&Value::Null));
        let ebody = serde_json::to_vec(&json!({ "input_tokens": est })).unwrap_or_default();
        st.emit_request(ex_id, started, &method, &req_path, &provider_name, &routing, 200, None);
        st.record_exchange(json!({
            "id": ex_id, "ts": now_ms(), "ms": started.elapsed().as_millis() as u64,
            "method": method.as_str(), "path": req_path, "url": ex_url,
            "provider": provider_name, "requestedModel": routing.client_facing_model,
            "outgoingModel": routing.outgoing_model, "clientFacingModel": routing.client_facing_model,
            "status": 200, "reqHeaders": ex_req_headers, "reqBody": ex_req_body,
            "clientReq": ex_client_req, "translated": ex_translated,
            "resHeaders": json!({ "x-ccbud-tokens": "estimated", "x-ccbud-upstream-status": status.as_u16().to_string() }),
            "resBody": json!({ "text": String::from_utf8_lossy(&ebody), "bytes": ebody.len(), "truncated": 0 }),
        }))
        .await;
        return Response::builder()
            .status(200)
            .header("content-type", "application/json")
            .header("x-ccbud-tokens", "estimated")
            .header("x-ccbud-upstream-status", status.as_u16().to_string())
            .body(Body::from(ebody))
            .unwrap();
    }

    if is_models_list {
        let mut merged = None;
        if status.is_success() {
            if let Ok(o) = serde_json::from_slice::<Value>(&buf) {
                if let Some(data) = o.get("data").and_then(|d| d.as_array()) {
                    if let Some(pid) = &active_pid {
                        let ids: HashSet<String> = data
                            .iter()
                            .filter_map(|m| m.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))
                            .collect();
                        if !ids.is_empty() {
                            st.known.lock().await.insert(pid.clone(), ids);
                        }
                    }
                    merged = Some(merge_models(&o, &config, client_codex));
                }
            }
        }
        let result = merged.unwrap_or_else(|| synthesize_models(&config, client_codex));
        let rbody = serde_json::to_vec(&result).unwrap_or_default();
        st.emit_request(ex_id, started, &method, &req_path, &provider_name, &routing, 200, None);
        st.record_exchange(json!({
            "id": ex_id, "ts": now_ms(), "ms": started.elapsed().as_millis() as u64,
            "method": method.as_str(), "path": req_path, "url": ex_url,
            "provider": provider_name, "requestedModel": routing.client_facing_model,
            "outgoingModel": routing.outgoing_model, "clientFacingModel": routing.client_facing_model,
            "status": 200, "reqHeaders": ex_req_headers, "reqBody": ex_req_body,
            "clientReq": ex_client_req, "translated": ex_translated,
            "resHeaders": json!({ "content-type": "application/json" }),
            "resBody": json!({ "text": String::from_utf8_lossy(&rbody), "bytes": rbody.len(), "truncated": 0 }),
        }))
        .await;
        return Response::builder()
            .status(200)
            .header("content-type", "application/json")
            .body(Body::from(rbody))
            .unwrap();
    }

    // Translated response: decode the (buffered) upstream reply → IR → re-encode to the client's
    // protocol. We forced stream=false upstream, so the reply is always buffered here.
    if let Some((client_wire, provider_wire, ref client_model, wanted_stream, _incremental)) = translate {
        let text = String::from_utf8_lossy(&buf);
        if !status.is_success() {
            st.log("warn", format!("upstream {} on translated request ({})", status.as_u16(), provider_name));
            st.emit_request(ex_id, started, &method, &req_path, &provider_name, &routing, status.as_u16(), None);
            return error_response(
                StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
                &format!("ccbud upstream error: {}", text.chars().take(400).collect::<String>()),
                "api_error",
            );
        }
        let ir = match crate::protocol::decode_upstream_response(provider_wire, &text) {
            Ok(ir) => ir,
            Err(e) => {
                st.log("error", format!("response translate ({:?}→{:?}) failed: {}", provider_wire, client_wire, e));
                st.emit_request(ex_id, started, &method, &req_path, &provider_name, &routing, 502, None);
                return error_response(StatusCode::BAD_GATEWAY, &format!("ccbud response translation failed: {}", e), "api_error");
            }
        };
        let mut usage = UsageAcc::default();
        if let Some(u) = ir.usage.as_ref() {
            usage.input = u.prompt_tokens as i64;
            usage.output = u.completion_tokens as i64;
            usage.saw = true;
        }
        let (ct_out, body_bytes) = if wanted_stream {
            let sse = crate::protocol::encode_client_response_sse(client_wire, &ir, client_model).unwrap_or_default();
            ("text/event-stream", Bytes::from(sse))
        } else {
            let j = crate::protocol::encode_client_response(client_wire, &ir, client_model).unwrap_or_else(|_| json!({}));
            ("application/json", Bytes::from(serde_json::to_vec(&j).unwrap_or_default()))
        };
        st.emit_request(ex_id, started, &method, &req_path, &provider_name, &routing, status.as_u16(), Some(&usage));
        st.record_exchange(json!({
            "id": ex_id, "ts": now_ms(), "ms": started.elapsed().as_millis() as u64,
            "method": method.as_str(), "path": req_path, "url": ex_url,
            "provider": provider_name, "requestedModel": routing.client_facing_model,
            "outgoingModel": routing.outgoing_model, "clientFacingModel": routing.client_facing_model,
            "status": status.as_u16(), "reqHeaders": ex_req_headers, "reqBody": ex_req_body,
            "clientReq": ex_client_req, "translated": ex_translated,
            "upstreamRes": json!({ "status": status.as_u16(), "headers": vec_headers(&out_headers), "body": cap_text(&buf, 1024 * 1024) }),
            "resHeaders": json!({ "content-type": ct_out, "x-ccbud-translated": format!("{:?}->{:?}", provider_wire, client_wire) }),
            "resBody": cap_text(&body_bytes, 2 * 1024 * 1024),
        }))
        .await;
        let mut builder = Response::builder()
            .status(status.as_u16())
            .header("content-type", ct_out)
            .header("x-ccbud-translated", format!("{:?}->{:?}", provider_wire, client_wire));
        for (k, v) in &out_headers {
            if k == "request-id" || k == "x-request-id" {
                builder = builder.header(k, v);
            }
        }
        return builder.body(Body::from(body_bytes)).unwrap();
    }

    let mut out_buf = buf.clone();
    let mut usage = UsageAcc::default();
    if ct.contains("application/json") {
        if let Ok(mut o) = serde_json::from_slice::<Value>(&buf) {
            if let Some(u) = o.get("usage").cloned() {
                usage.input += u.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                usage.output += u.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                usage.cache_read += u.get("cache_read_input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                usage.cache_creation += u.get("cache_creation_input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                usage.saw = true;
            }
            if need_rewrite {
                if let Some(cf) = &routing.client_facing_model {
                    if o.get("model").and_then(|v| v.as_str()).is_some() {
                        o["model"] = json!(cf);
                        if let Ok(b) = serde_json::to_vec(&o) {
                            out_buf = Bytes::from(b);
                        }
                    }
                }
            }
        }
    }
    st.emit_request(ex_id, started, &method, &req_path, &provider_name, &routing, status.as_u16(), Some(&usage));
    st.record_exchange(json!({
        "id": ex_id, "ts": now_ms(), "ms": started.elapsed().as_millis() as u64,
        "method": method.as_str(), "path": req_path, "url": ex_url,
        "provider": provider_name, "requestedModel": routing.client_facing_model,
        "outgoingModel": routing.outgoing_model, "clientFacingModel": routing.client_facing_model,
        "status": status.as_u16(), "reqHeaders": ex_req_headers, "reqBody": ex_req_body,
        "clientReq": ex_client_req, "translated": ex_translated,
        "resHeaders": vec_headers(&out_headers), "resBody": cap_text(&out_buf, 2 * 1024 * 1024),
    }))
    .await;

    let mut builder = Response::builder().status(status.as_u16());
    for (k, v) in &out_headers {
        builder = builder.header(k, v);
    }
    builder.body(Body::from(out_buf)).unwrap()
}

// ---- mock upstream + end-to-end gateway selftest (debug only) ----

/// Spawn an in-process mock Anthropic-style upstream on a random port. Echoes back the model
/// the gateway forwarded (proving the outgoing rewrite), with usage, as JSON or SSE.
pub async fn start_mock_upstream() -> Option<u16> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.ok()?;
    let port = listener.local_addr().ok()?.port();
    let app: Router = Router::new().fallback(mock_handler);
    tauri::async_runtime::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Some(port)
}

async fn mock_handler(req: axum::extract::Request) -> Response {
    let (parts, body) = req.into_parts();
    let path = parts.uri.path().to_string();
    let bytes = to_bytes(body, 1024 * 1024).await.unwrap_or_default();
    if path.ends_with("/count_tokens") || path == "/" {
        // Simulate a provider that implements neither count_tokens nor `HEAD /` → the gateway
        // estimates locally / serves the health-probe fallback.
        return Response::builder()
            .status(404)
            .header("content-type", "application/json")
            .body(Body::from("{\"error\":\"not found\"}"))
            .unwrap();
    }
    let v: Value = serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({}));
    let stream = v.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
    let model = v.get("model").and_then(|m| m.as_str()).unwrap_or("upstream-model").to_string();
    // OpenAI Chat endpoint: answer in Chat Completions shape so the gateway's protocol translation
    // (Anthropic→chat request, chat→Anthropic response) can be exercised end-to-end. The gateway
    // forces stream=false upstream when translating, so we only need the buffered form here.
    if path.contains("/chat/completions") {
        if stream {
            // OpenAI Chat streaming chunks (text split across two chunks + a usage-bearing final
            // chunk), so the incremental transcoder is exercised end-to-end.
            let sse = format!(
                "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\"}}}}]}}\n\n\
                 data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"hi \"}}}}]}}\n\n\
                 data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"from chat\"}}}}]}}\n\n\
                 data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}],\"usage\":{{\"prompt_tokens\":12,\"completion_tokens\":5}}}}\n\n\
                 data: [DONE]\n\n"
            );
            let _ = &model;
            return Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(Body::from(sse))
                .unwrap();
        }
        return json_response(
            StatusCode::OK,
            &json!({
                "id": "chatcmpl-mock", "object": "chat.completion", "created": 1, "model": model,
                "choices": [{ "index": 0, "finish_reason": "stop",
                    "message": { "role": "assistant", "content": "hi from chat" } }],
                "usage": { "prompt_tokens": 12, "completion_tokens": 5, "total_tokens": 17 },
            }),
        );
    }
    // OpenAI Responses endpoint: reply in Responses shape (buffered) so messages→responses can be
    // exercised end-to-end. (The gateway forces stream=false upstream for the responses direction.)
    if path.ends_with("/responses") || path.ends_with("/responses/") {
        return json_response(
            StatusCode::OK,
            &json!({
                "id": "resp-mock", "object": "response", "created_at": 1, "model": model, "status": "completed",
                "output": [{ "type": "message", "role": "assistant",
                    "content": [{ "type": "output_text", "text": "hi from responses" }] }],
                "output_text": "hi from responses",
                "usage": { "input_tokens": 14, "output_tokens": 6, "total_tokens": 20 },
            }),
        );
    }
    if stream {
        // Anthropic streaming with a real text block, so the Anthropic→Responses incremental
        // transcoder (Codex client) has content to carry, not just usage bookkeeping.
        let sse = format!(
            "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_mock\",\"model\":\"{m}\",\"usage\":{{\"input_tokens\":10,\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0}}}}}}\n\nevent: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\nevent: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"hi from anthropic\"}}}}\n\nevent: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\nevent: message_delta\ndata: {{\"type\":\"message_delta\",\"usage\":{{\"output_tokens\":7}}}}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n",
            m = model
        );
        Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .body(Body::from(sse))
            .unwrap()
    } else {
        json_response(
            StatusCode::OK,
            &json!({ "id":"msg_mock", "type":"message", "role":"assistant", "model":model, "content":[{"type":"text","text":"hi"}], "stop_reason":"end_turn", "usage":{"input_tokens":10,"output_tokens":7} }),
        )
    }
}

/// End-to-end gateway test against the mock upstream: routing + response model rewrite for both
/// buffered JSON and streaming SSE. Mutates CCBUD_HOME config (only called in a throwaway run).
pub async fn gateway_selftest(gport: u16) -> Value {
    if gport == 0 {
        return json!({ "err": "gateway not running" });
    }
    let mock = match start_mock_upstream().await {
        Some(p) => p,
        None => return json!({ "err": "mock failed to start" }),
    };
    let cfg = json!({ "port": gport, "activeProviderId":"mock", "providers":[
        { "id":"mock","name":"Mock","baseUrl":format!("http://127.0.0.1:{}", mock),"authToken":"k","defaultModel":"upstream-model","smallFastModel":"upstream-model","mapDefaultModels":true,"models":[{"alias":"test-alias","upstream":"upstream-model"}] }
    ]});
    store::write_config(cfg);
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;

    let client = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{}/v1/messages", gport);

    let ns = client
        .post(&base)
        .json(&json!({ "model":"test-alias","max_tokens":8,"messages":[{"role":"user","content":"hi"}] }))
        .send()
        .await;
    let (ns_status, ns_model) = match ns {
        Ok(r) => {
            let s = r.status().as_u16();
            let j: Value = r.json().await.unwrap_or_else(|_| json!({}));
            (s, j.get("model").and_then(|m| m.as_str()).unwrap_or("").to_string())
        }
        Err(e) => (0, format!("ERR:{}", e)),
    };

    let stm = client
        .post(&base)
        .json(&json!({ "model":"test-alias","stream":true,"max_tokens":8,"messages":[{"role":"user","content":"hi"}] }))
        .send()
        .await;
    let (st_status, st_text) = match stm {
        Ok(r) => (r.status().as_u16(), r.text().await.unwrap_or_default()),
        Err(e) => (0, format!("ERR:{}", e)),
    };

    // count_tokens — mock 404s, so the gateway must estimate locally
    let ct = client
        .post(format!("http://127.0.0.1:{}/v1/messages/count_tokens", gport))
        .json(&json!({ "model":"test-alias","messages":[{"role":"user","content":"hello world this is a token counting test"}] }))
        .send()
        .await;
    let (ct_status, ct_tokens, ct_estimated) = match ct {
        Ok(r) => {
            let s = r.status().as_u16();
            let estimated = r.headers().get("x-ccbud-tokens").and_then(|v| v.to_str().ok()) == Some("estimated");
            let j: Value = r.json().await.unwrap_or_else(|_| json!({}));
            (s, j.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(-1), estimated)
        }
        Err(_) => (0, -1, false),
    };

    // ---- protocol translation: Claude Code (Anthropic /v1/messages) → an OpenAI-Chat provider ----
    // Reconfigure the mock provider to speak openai-chat, then hit /v1/messages and prove the
    // response comes back Anthropic-shaped (non-stream) and as a valid Anthropic SSE (stream).
    let cfg2 = json!({ "port": gport, "activeProviderId":"mockoa", "providers":[
        { "id":"mockoa","name":"MockOpenAI","baseUrl":format!("http://127.0.0.1:{}", mock),"authToken":"k","protocol":"openai-chat","defaultModel":"gpt-mock","smallFastModel":"gpt-mock","mapDefaultModels":true,"models":[{"alias":"test-alias","upstream":"gpt-mock"}] }
    ]});
    store::write_config(cfg2.clone());
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let tx_ns = client
        .post(&base)
        .json(&json!({ "model":"test-alias","max_tokens":8,"messages":[{"role":"user","content":"hi"}] }))
        .send()
        .await;
    let (tx_ns_status, tx_ns_anthropic, tx_ns_text, tx_ns_model) = match tx_ns {
        Ok(r) => {
            let s = r.status().as_u16();
            let j: Value = r.json().await.unwrap_or_else(|_| json!({}));
            let is_msg = j.get("type").and_then(|v| v.as_str()) == Some("message");
            let text = j.get("content").and_then(|c| c.as_array()).and_then(|a| a.first())
                .and_then(|b| b.get("text")).and_then(|v| v.as_str()).unwrap_or("").to_string();
            let model = j.get("model").and_then(|v| v.as_str()).unwrap_or("").to_string();
            (s, is_msg, text, model)
        }
        Err(e) => (0, false, format!("ERR:{}", e), String::new()),
    };

    let tx_st = client
        .post(&base)
        .json(&json!({ "model":"test-alias","stream":true,"max_tokens":8,"messages":[{"role":"user","content":"hi"}] }))
        .send()
        .await;
    let (tx_st_status, tx_st_text) = match tx_st {
        Ok(r) => (r.status().as_u16(), r.text().await.unwrap_or_default()),
        Err(e) => (0, format!("ERR:{}", e)),
    };

    // ---- protocol translation: Claude Code (Anthropic /v1/messages) → an OpenAI-Responses provider ----
    let cfg3 = json!({ "port": gport, "activeProviderId":"mockre", "providers":[
        { "id":"mockre","name":"MockResponses","baseUrl":format!("http://127.0.0.1:{}", mock),"authToken":"k","protocol":"openai-responses","defaultModel":"gpt-mock","smallFastModel":"gpt-mock","mapDefaultModels":true,"models":[{"alias":"test-alias","upstream":"gpt-mock"}] }
    ]});
    store::write_config(cfg3);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let rx = client
        .post(&base)
        .json(&json!({ "model":"test-alias","max_tokens":8,"messages":[{"role":"user","content":"hi"}] }))
        .send()
        .await;
    let (rx_status, rx_anthropic, rx_text) = match rx {
        Ok(r) => {
            let s = r.status().as_u16();
            let j: Value = r.json().await.unwrap_or_else(|_| json!({}));
            let is_msg = j.get("type").and_then(|v| v.as_str()) == Some("message");
            let text = j.get("content").and_then(|c| c.as_array()).and_then(|a| a.first())
                .and_then(|b| b.get("text")).and_then(|v| v.as_str()).unwrap_or("").to_string();
            (s, is_msg, text)
        }
        Err(e) => (0, false, format!("ERR:{}", e)),
    };

    // ---- reverse: an OpenAI-Chat client (/v1/chat/completions) → an Anthropic provider ----
    let cfg4 = json!({ "port": gport, "activeProviderId":"mockan", "providers":[
        { "id":"mockan","name":"MockAnthropic","baseUrl":format!("http://127.0.0.1:{}", mock),"authToken":"k","protocol":"anthropic","defaultModel":"claude-mock","smallFastModel":"claude-mock","mapDefaultModels":true,"models":[{"alias":"test-alias","upstream":"claude-mock"}] }
    ]});
    store::write_config(cfg4);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let rev = client
        .post(format!("http://127.0.0.1:{}/v1/chat/completions", gport))
        .json(&json!({ "model":"test-alias","messages":[{"role":"user","content":"hi"}] }))
        .send()
        .await;
    let (rev_status, rev_is_chat, rev_text) = match rev {
        Ok(r) => {
            let s = r.status().as_u16();
            let j: Value = r.json().await.unwrap_or_else(|_| json!({}));
            let is_chat = j.get("object").and_then(|v| v.as_str()) == Some("chat.completion");
            let text = j.get("choices").and_then(|c| c.as_array()).and_then(|a| a.first())
                .and_then(|c| c.get("message")).and_then(|m| m.get("content")).and_then(|v| v.as_str()).unwrap_or("").to_string();
            (s, is_chat, text)
        }
        Err(e) => (0, false, format!("ERR:{}", e)),
    };

    // ---- Codex (OpenAI-Responses client, /v1/responses) → an Anthropic provider ----
    // The shape Codex sends with wire_api="responses": instructions + item-based input + flattened
    // function tools. Non-stream proves the buffered translate; stream proves the incremental
    // Anthropic→Responses transcoder (item done events + terminal response.completed).
    let codex_body = json!({ "model":"test-alias", "instructions":"be nice",
        "input":[{ "type":"message","role":"user","content":[{ "type":"input_text","text":"hi" }] }],
        "tools":[{ "type":"function","name":"shell","description":"run","parameters":{ "type":"object" } }],
        "tool_choice":"auto", "store": false });
    let cdx = client
        .post(format!("http://127.0.0.1:{}/v1/responses", gport))
        .json(&codex_body)
        .send()
        .await;
    let (cdx_status, cdx_is_response, cdx_text) = match cdx {
        Ok(r) => {
            let s = r.status().as_u16();
            let j: Value = r.json().await.unwrap_or_else(|_| json!({}));
            let is_resp = j.get("object").and_then(|v| v.as_str()) == Some("response");
            let text = j.get("output_text").and_then(|v| v.as_str()).unwrap_or("").to_string();
            (s, is_resp, text)
        }
        Err(e) => (0, false, format!("ERR:{}", e)),
    };
    let mut codex_stream_body = codex_body.clone();
    codex_stream_body["stream"] = json!(true);
    let cdx_st = client
        .post(format!("http://127.0.0.1:{}/v1/responses", gport))
        .json(&codex_stream_body)
        .send()
        .await;
    let (cdx_st_status, cdx_st_text) = match cdx_st {
        Ok(r) => (r.status().as_u16(), r.text().await.unwrap_or_default()),
        Err(e) => (0, format!("ERR:{}", e)),
    };

    // ---- Codex → an OpenAI-Chat provider (incremental chat→Responses transcoding) ----
    store::write_config(cfg2.clone());
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let cdx_chat = client
        .post(format!("http://127.0.0.1:{}/v1/responses", gport))
        .json(&codex_stream_body)
        .send()
        .await;
    let (cdx_chat_status, cdx_chat_text) = match cdx_chat {
        Ok(r) => (r.status().as_u16(), r.text().await.unwrap_or_default()),
        Err(e) => (0, format!("ERR:{}", e)),
    };

    json!({
        "nonStreamStatus": ns_status,
        "nonStreamModel": ns_model,
        "nonStreamRewritten": ns_model == "test-alias",
        "xlateResponsesStatus": rx_status,
        "xlateResponsesIsAnthropic": rx_anthropic,
        "xlateResponsesText": rx_text,
        "revChatStatus": rev_status,
        "revChatIsChatCompletion": rev_is_chat,
        "revChatText": rev_text,
        "streamStatus": st_status,
        "streamHasStart": st_text.contains("message_start"),
        "streamRewritten": st_text.contains("\"test-alias\"") && !st_text.contains("upstream-model"),
        "countTokensStatus": ct_status,
        "countTokensEstimated": ct_estimated,
        "countTokens": ct_tokens,
        // protocol translation (messages→chat)
        "xlateNonStreamStatus": tx_ns_status,
        "xlateNonStreamIsAnthropic": tx_ns_anthropic,
        "xlateNonStreamText": tx_ns_text,
        "xlateNonStreamModel": tx_ns_model,
        "xlateStreamStatus": tx_st_status,
        "xlateStreamHasStart": tx_st_text.contains("message_start"),
        "xlateStreamHasStop": tx_st_text.contains("message_stop"),
        // incremental transcode: OpenAI chunks → Anthropic text_delta events (text split across
        // chunks), a real content_block_delta, and end_turn stop.
        "xlateStreamIncremental": tx_st_text.contains("content_block_delta") && tx_st_text.contains("text_delta"),
        "xlateStreamText": tx_st_text.contains("from chat"),
        "xlateStreamStop": tx_st_text.contains("\"stop_reason\":\"end_turn\""),
        // Codex (Responses client): buffered translate + incremental stream transcoders. Codex
        // materializes items from response.output_item.done and requires response.completed.
        "codexNonStreamStatus": cdx_status,
        "codexNonStreamIsResponse": cdx_is_response,
        "codexNonStreamText": cdx_text,
        "codexAnthropicStreamStatus": cdx_st_status,
        "codexAnthropicStreamDelta": cdx_st_text.contains("response.output_text.delta"),
        "codexAnthropicStreamItemDone": cdx_st_text.contains("response.output_item.done"),
        "codexAnthropicStreamCompleted": cdx_st_text.contains("response.completed"),
        "codexAnthropicStreamText": cdx_st_text.contains("hi from anthropic"),
        "codexChatStreamStatus": cdx_chat_status,
        "codexChatStreamDelta": cdx_chat_text.contains("response.output_text.delta"),
        "codexChatStreamItemDone": cdx_chat_text.contains("response.output_item.done"),
        "codexChatStreamCompleted": cdx_chat_text.contains("response.completed"),
        "codexChatStreamText": cdx_chat_text.contains("from chat"),
    })
}

/// In-binary equivalent of test/selftest.js's 8 routing unit checks.
pub fn routing_selftest() -> Value {
    let config = json!({ "port":0, "activeProviderId":"glm", "providers":[
        { "id":"glm","name":"GLM","baseUrl":"https://x","authToken":"","defaultModel":"glm-5.1","smallFastModel":"glm-5.1","mapDefaultModels":true,"models":[{"alias":"claude-opus-4.8[1m]","upstream":"glm-5.1"}] }
    ]});
    let cfg2 = json!({ "port":0, "activeProviderId":"main", "providers":[
        { "id":"main","name":"Main","baseUrl":"http://127.0.0.1:1","authToken":"k","defaultModel":"big-model","smallFastModel":"small-model","mapDefaultModels":true,"models":[{"alias":"my-alias","upstream":"aliased-up"}] },
        { "id":"other","name":"Other","baseUrl":"http://127.0.0.1:2","authToken":"k","defaultModel":"other-big","smallFastModel":"other-small","mapDefaultModels":true,"models":[{"alias":"other-alias","upstream":"other-up"}] }
    ]});
    let off = json!({ "port":0, "activeProviderId":"m", "providers":[
        { "id":"m","name":"M","baseUrl":"http://127.0.0.1:1","authToken":"k","defaultModel":"big","smallFastModel":"small","mapDefaultModels":false,"models":[] }
    ]});

    let out = |r: &Option<Routing>| r.as_ref().and_then(|x| x.outgoing_model.clone());
    let cf = |r: &Option<Routing>| r.as_ref().and_then(|x| x.client_facing_model.clone());
    let pidf = |r: &Option<Routing>| r.as_ref().map(|x| x.provider_id.clone());

    let mut fails: Vec<String> = vec![];
    let mut n = 0;
    let mut chk = |name: &str, cond: bool| {
        n += 1;
        if !cond {
            fails.push(name.to_string());
        }
    };

    let r = resolve_routing(Some("claude-opus-4.8[1m]"), &config, None);
    chk("1 alias→upstream", out(&r).as_deref() == Some("glm-5.1") && cf(&r).as_deref() == Some("claude-opus-4.8[1m]"));
    let r = resolve_routing(Some("glm-5.1"), &config, None);
    chk("2 real passthrough", out(&r).as_deref() == Some("glm-5.1") && cf(&r).as_deref() == Some("glm-5.1"));
    let r = resolve_routing(Some("claude-3-5-haiku-20241022"), &cfg2, None);
    chk("3 haiku→light", out(&r).as_deref() == Some("small-model"));
    let r = resolve_routing(Some("claude-sonnet-4-6"), &cfg2, None);
    chk("4 sonnet→primary", out(&r).as_deref() == Some("big-model"));
    let r = resolve_routing(Some("gpt-4-turbo"), &cfg2, None);
    chk("5 foreign→light", out(&r).as_deref() == Some("small-model"));
    let mut known = HashSet::new();
    known.insert("glm-5.2".to_string());
    let r = resolve_routing(Some("glm-5.2"), &cfg2, Some(&known));
    chk("6 known passthrough", out(&r).as_deref() == Some("glm-5.2"));
    let r = resolve_routing(Some("other-alias"), &cfg2, None);
    chk("7 stays on active", pidf(&r).as_deref() == Some("main") && out(&r).as_deref() == Some("small-model"));
    let r = resolve_routing(Some("whatever-x"), &off, None);
    chk("8 mapoff passthrough", out(&r).as_deref() == Some("whatever-x"));
    let r = resolve_routing(Some("gpt-5.5-ccbud"), &cfg2, None);
    chk(
        "9 codex sentinel→primary",
        out(&r).as_deref() == Some("big-model") && cf(&r).as_deref() == Some("gpt-5.5-ccbud"),
    );

    json!({ "total": n, "passed": n - fails.len(), "failed": fails.len(), "fails": fails })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn routing_parity_with_proxy_js() {
        let r = routing_selftest();
        assert_eq!(r.get("failed").and_then(|v| v.as_i64()), Some(0), "routing mismatch: {:?}", r);
        assert_eq!(r.get("passed").and_then(|v| v.as_i64()), Some(9));
    }
    #[test]
    fn synthesize_models_includes_claude_tiers() {
        let cfg = json!({ "providers": [{ "id": "p", "defaultModel": "m", "smallFastModel": "m" }], "activeProviderId": "p" });
        let s = synthesize_models(&cfg, false);
        let ids: Vec<&str> = s["data"].as_array().unwrap().iter().filter_map(|m| m["id"].as_str()).collect();
        assert!(ids.contains(&"claude-sonnet-5"));
        assert!(ids.contains(&"claude-fable-5"));
        assert!(!ids.iter().any(|id| id.starts_with("gpt-")));
    }
    #[test]
    fn synthesize_models_codex_returns_gpt_tiers() {
        let cfg = json!({ "providers": [{ "id": "p", "defaultModel": "m", "smallFastModel": "m" }], "activeProviderId": "p" });
        let s = synthesize_models(&cfg, true);
        let ids: Vec<&str> = s["data"].as_array().unwrap().iter().filter_map(|m| m["id"].as_str()).collect();
        assert!(ids.contains(&"gpt-5.6-sol"));
        assert!(ids.contains(&"gpt-5.6-luna"));
        assert!(!ids.iter().any(|id| id.starts_with("claude-")));
    }
    #[test]
    fn routing_classifies_by_family() {
        let cfg = json!({ "providers": [{ "id": "p", "baseUrl": "http://127.0.0.1:1", "authToken": "k",
            "defaultModel": "big", "smallFastModel": "small", "mapDefaultModels": true, "models": [] }], "activeProviderId": "p" });
        let out = |r: Option<Routing>| r.and_then(|x| x.outgoing_model);
        // Claude: haiku → fast, fable/opus/sonnet → primary.
        assert_eq!(out(resolve_routing(Some("claude-haiku-4-5"), &cfg, None)).as_deref(), Some("small"));
        assert_eq!(out(resolve_routing(Some("claude-fable-5"), &cfg, None)).as_deref(), Some("big"));
        assert_eq!(out(resolve_routing(Some("claude-opus-4-8"), &cfg, None)).as_deref(), Some("big"));
        // Codex: -sol/-terra → primary, -luna (and others) → fast.
        assert_eq!(out(resolve_routing(Some("gpt-5.6-sol"), &cfg, None)).as_deref(), Some("big"));
        assert_eq!(out(resolve_routing(Some("gpt-5.6-terra"), &cfg, None)).as_deref(), Some("big"));
        assert_eq!(out(resolve_routing(Some("gpt-5.6-luna"), &cfg, None)).as_deref(), Some("small"));
    }
    #[test]
    fn retry_delay_honors_seconds_and_backoff() {
        assert_eq!(retry_delay(Some("2"), 0, 500), 2000);
        assert_eq!(retry_delay(None, 0, 500), 500);
        assert_eq!(retry_delay(None, 1, 500), 1000);
        // HTTP-date in the past → no wait (clamped to 0), NOT a fall-through to backoff.
        assert_eq!(retry_delay(Some("Wed, 21 Oct 2015 07:28:00 GMT"), 3, 500), 0);
        // Unparseable Retry-After → exponential backoff (base * 2^attempt).
        assert_eq!(retry_delay(Some("soon"), 2, 500), 2000);
    }
}
