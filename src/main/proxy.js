'use strict';

/**
 * ccbud Gateway — pure Node proxy core (no Electron dependency, fully testable).
 *
 * Responsibilities:
 *  - Listen on 127.0.0.1:<port>
 *  - Forward every request to the matched upstream provider (baseUrl + token)
 *  - Replace the client Authorization/x-api-key with the upstream's real token
 *  - Resolve model routing:
 *      * explicit alias (alias -> upstream), routed to the owning provider
 *      * pass-through of a provider's real model
 *      * automatic mapping of Claude default model names to the active provider
 *  - Rewrite the response `model` field back to what the client asked for
 *    (covers both buffered JSON and streaming SSE message_start)
 */

const http = require('http');
const https = require('https');
const zlib = require('zlib');
const presidio = require('./presidio');
const { estimateInputTokens } = require('./countTokens');
const { URL } = require('url');
const { Transform, Writable, pipeline } = require('stream');
const { EventEmitter } = require('events');

function errorBody(message, type) {
  return JSON.stringify({
    type: 'error',
    error: { type: type || 'api_error', message },
  });
}

function respondJson(res, status, obj) {
  const buf = Buffer.from(typeof obj === 'string' ? obj : JSON.stringify(obj), 'utf8');
  try {
    res.writeHead(status, {
      'content-type': 'application/json',
      'content-length': Buffer.byteLength(buf),
    });
    res.end(buf);
  } catch (_) {
    /* socket already gone */
  }
}

/** Heuristic: is this model name a "small / fast" tier? */
function looksSmall(name) {
  return /haiku|small|fast|mini|air|flash|lite|nano|tiny|turbo/i.test(name || '');
}
/** Is this one of Claude's own default model names (the only names we auto-remap)? */
function isClaudeDefault(name) {
  return /^claude[-_]/i.test(name || '');
}

/**
 * Decide how to route a request and translate its model name.
 * Returns { provider, outgoingModel, clientFacingModel } or null.
 *
 * Unified rule (issue #10): EVERY request goes to the single active provider — we no
 * longer hop to whichever provider happens to own a matching alias. Against that active
 * provider the requested model id is resolved, in order:
 *
 *   1. a Custom alias of the active provider        -> map alias -> the user's upstream name
 *   2. the active provider's PRIMARY / LIGHTWEIGHT   -> passthrough untouched
 *   3. a model the provider really has              -> passthrough untouched
 *        ("really has" = the upstream side of a configured alias, or present in the
 *         provider's live /v1/models list captured into `knownModels`)
 *   4. any other unconfigured id (default mapping on):
 *        · Claude's main tiers (opus/sonnet/mythos/fable, …) -> PRIMARY
 *        · everything else unmatched (haiku, foreign names)  -> LIGHTWEIGHT
 *      With per-provider default mapping turned off, the name is forwarded untouched.
 *
 * clientFacingModel === outgoingModel  => pure passthrough (do NOT touch the response)
 * clientFacingModel !== outgoingModel  => we changed the model, so rewrite the response back
 *
 * @param {Set<string>} [knownModels] real upstream model ids for the active provider.
 */
function resolveRouting(requestedModel, config, knownModels) {
  const providers = (config && config.providers) || [];
  if (providers.length === 0) return null;

  const active = providers.find((p) => p.id === config.activeProviderId) || providers[0];
  if (!active) return null;

  const pass = (m) => ({ provider: active, outgoingModel: m, clientFacingModel: m });

  // No model on the request (e.g. a non-/v1/messages call) -> forward as-is.
  if (!requestedModel) return { provider: active, outgoingModel: null, clientFacingModel: null };

  const primary = active.defaultModel || '';
  const light = active.smallFastModel || '';

  // 1) Custom alias of the ACTIVE provider -> rewrite to the user's upstream model.
  for (const m of active.models || []) {
    if (m && m.alias && m.alias === requestedModel && m.upstream) {
      return { provider: active, outgoingModel: m.upstream, clientFacingModel: requestedModel };
    }
  }

  // 2) Already the provider's PRIMARY or LIGHTWEIGHT model -> passthrough.
  if (requestedModel === primary || requestedModel === light) return pass(requestedModel);

  // 3) A model the active provider really has -> passthrough.
  for (const m of active.models || []) {
    if (m && m.upstream === requestedModel) return pass(requestedModel);
  }
  if (knownModels && typeof knownModels.has === 'function' && knownModels.has(requestedModel)) {
    return pass(requestedModel);
  }

  // 4) Unconfigured id. With default mapping off, forward untouched (escape hatch).
  if (active.mapDefaultModels === false) return pass(requestedModel);

  // Otherwise map onto the active provider's own models: Claude's main tiers -> PRIMARY,
  // everything else unmatched (Claude small tiers + any foreign name) -> LIGHTWEIGHT.
  const big = primary || light;
  const small = light || primary;
  let target;
  if (isClaudeDefault(requestedModel)) target = looksSmall(requestedModel) ? small : big;
  else target = small; // unknown foreign model -> route to the known-good lightweight model
  if (target) return { provider: active, outgoingModel: target, clientFacingModel: requestedModel };

  // Nothing configured to map onto -> last-resort passthrough.
  return pass(requestedModel);
}

/**
 * How long to wait before retrying an upstream 429. Honors a `Retry-After` header
 * (delta-seconds or an HTTP-date) when present, otherwise exponential backoff from
 * `base` (attempt 0,1,2 -> base, 2x, 4x). Always clamped so a hostile/huge value
 * can't stall the request indefinitely.
 */
function retryDelay(retryAfter, attempt, base) {
  const cap = 30000;
  if (retryAfter != null) {
    const s = String(retryAfter).trim();
    if (/^\d+$/.test(s)) return Math.min(parseInt(s, 10) * 1000, cap);
    const when = Date.parse(s);
    if (!Number.isNaN(when)) return Math.min(Math.max(when - Date.now(), 0), cap);
  }
  return Math.min((base || 500) * Math.pow(2, attempt), 8000);
}

// Headers whose VALUES must never surface in the monitor inspector (real upstream key etc).
const REDACT_RE = /^(authorization|x-api-key|cookie|set-cookie|proxy-authorization|x-goog-api-key)$/i;
function redactHeaders(h) {
  const o = {};
  for (const k of Object.keys(h || {})) o[k] = REDACT_RE.test(k) ? '••••••（已隐藏）' : h[k];
  return o;
}
// Cap a captured body so the in-memory inspector stays bounded; keep the true byte count.
// Request bodies get a generous cap so a full Claude Code request (entire history) is shown
// un-truncated for debugging; response/SSE bodies stay bounded since streams can be huge.
const REQ_CAP = 4 * 1024 * 1024;
const RES_CAP = 2 * 1024 * 1024;
function capText(buf, cap) {
  const limit = cap || REQ_CAP;
  if (!buf || !buf.length) return { text: '', bytes: 0, truncated: 0 };
  const total = buf.length;
  if (total <= limit) return { text: buf.toString('utf8'), bytes: total, truncated: 0 };
  // Slicing at a byte boundary can split a multi-byte char → drop the trailing replacement char.
  return { text: buf.slice(0, limit).toString('utf8').replace(/�+$/, ''), bytes: total, truncated: total - limit };
}

/* ---- /v1/models augmentation ----
 * Some providers don't implement /v1/models, and even those that do never list the user's
 * configured aliases. So when a provider HAS the endpoint we pass its list through and ADD
 * the alias models; when it doesn't (404 / unreachable) we synthesize the list from aliases. */
function modelEntry(id) {
  return { type: 'model', id, display_name: id, created_at: '2025-01-01T00:00:00Z' };
}
function aliasModelEntries(config) {
  const out = [];
  const seen = new Set();
  for (const p of (config && config.providers) || []) {
    for (const m of p.models || []) {
      if (m && m.alias && !seen.has(m.alias)) { seen.add(m.alias); out.push(modelEntry(m.alias)); }
    }
  }
  return out;
}
function mergeModels(upstream, config) {
  const data = Array.isArray(upstream && upstream.data) ? upstream.data.slice() : [];
  const have = new Set(data.map((m) => m && m.id));
  const adds = aliasModelEntries(config).filter((a) => !have.has(a.id));
  const merged = Object.assign({}, upstream || {});
  merged.data = adds.concat(data); // aliases first so they stand out
  return merged;
}
function synthesizeModels(config) {
  let out = aliasModelEntries(config);
  if (!out.length) {
    // no aliases configured → fall back to the active provider's real models so it isn't empty
    const providers = (config && config.providers) || [];
    const active = providers.find((p) => p.id === config.activeProviderId) || providers[0];
    const seen = new Set();
    for (const id of [active && active.defaultModel, active && active.smallFastModel]) {
      if (id && !seen.has(id)) { seen.add(id); out.push(modelEntry(id)); }
    }
  }
  return { data: out, has_more: false, first_id: out[0] ? out[0].id : null, last_id: out.length ? out[out.length - 1].id : null };
}

/** Normalize an Anthropic `usage` object into our token shape. */
function extractUsage(u) {
  if (!u) return null;
  return {
    inputTokens: u.input_tokens || 0,
    outputTokens: u.output_tokens || 0,
    cacheRead: u.cache_read_input_tokens || 0,
    cacheCreation: u.cache_creation_input_tokens || 0,
  };
}

/**
 * SSE transform: passes the stream through unchanged except optionally rewriting the
 * `model` field value (when `model` is non-null), while ALSO sniffing token usage from
 * `message_start` (input/cache) and `message_delta` (cumulative output). Calls onUsage(u)
 * at end-of-stream if any usage was seen. Line-buffered so JSON fields are never split.
 */
function createSseTransform(model, opts) {
  const onUsage = typeof opts === 'function' ? opts : opts && opts.onUsage;
  const replacement = model != null ? String(model).replace(/\$/g, '$$$$') : null;
  const re = /("model"\s*:\s*")[^"]*(")/g;
  let buffer = '';
  const usage = { inputTokens: 0, outputTokens: 0, cacheRead: 0, cacheCreation: 0 };
  let saw = false;

  function absorbUsage(obj) {
    if (obj && obj.type === 'message_start' && obj.message && obj.message.usage) {
      const u = obj.message.usage;
      usage.inputTokens += u.input_tokens || 0;
      usage.cacheRead += u.cache_read_input_tokens || 0;
      usage.cacheCreation += u.cache_creation_input_tokens || 0;
      saw = true;
    } else if (obj && obj.type === 'message_delta' && obj.usage) {
      if (typeof obj.usage.output_tokens === 'number') usage.outputTokens = obj.usage.output_tokens;
      saw = true;
    }
  }
  function parseData(line) {
    const i = line.indexOf('{');
    if (i < 0) return null;
    try { return JSON.parse(line.slice(i)); } catch (_) { return null; }
  }
  function handleLine(line) {
    // Sniff usage from the upstream payload BEFORE rewriting model names for the client.
    if (line.indexOf('"usage"') !== -1) {
      const obj = parseData(line);
      if (obj) absorbUsage(obj);
    }
    if (replacement != null && line.indexOf('"model"') !== -1) line = line.replace(re, `$1${replacement}$2`);
    return line;
  }

  return new Transform({
    transform(chunk, _enc, cb) {
      buffer += chunk.toString('utf8');
      let out = '';
      let idx;
      while ((idx = buffer.indexOf('\n')) !== -1) {
        out += handleLine(buffer.slice(0, idx + 1));
        buffer = buffer.slice(idx + 1);
      }
      cb(null, out);
    },
    flush(cb) {
      const line = buffer ? handleLine(buffer) : '';
      buffer = '';
      if (saw && typeof onUsage === 'function') onUsage(usage);
      cb(null, line);
    },
  });
}

// Redact PII from an Anthropic /v1/messages request body (system prompt + each message's text and
// tool_result text), mutating it in place. All text fields go through Presidio concurrently.
async function redactRequestBody(parsed, px) {
  const opts = {
    language: px.language || 'en',
    ner: !!px.ner,                              // NER tier (opt-in)
    llm: !!(px.llm && px.ollamaUrl),            // LLM tier (opt-in, needs Ollama)
    ollamaUrl: px.ollamaUrl,
    ollamaModel: px.ollamaModel,
    threshold: typeof px.threshold === 'number' ? px.threshold : undefined,  // acceptance threshold
    deidentify: px.deidentify || 'replace',     // replace | redact | mask | hash
  };
  const tasks = [];
  const red = (s) => presidio.redactText(s, opts);
  const onText = (obj, key) => {
    const v = obj[key];
    if (typeof v === 'string') { if (v.trim()) tasks.push(red(v).then((r) => { obj[key] = r; })); return; }
    if (!Array.isArray(v)) return;
    for (const b of v) {
      if (!b || typeof b !== 'object') continue;
      if (b.type === 'text' && typeof b.text === 'string') tasks.push(red(b.text).then((r) => { b.text = r; }));
      else if (b.type === 'tool_result') {
        if (typeof b.content === 'string') tasks.push(red(b.content).then((r) => { b.content = r; }));
        else if (Array.isArray(b.content)) for (const c of b.content) if (c && c.type === 'text' && typeof c.text === 'string') tasks.push(red(c.text).then((r) => { c.text = r; }));
      }
    }
  };
  if (parsed.system != null) onText(parsed, 'system');
  if (Array.isArray(parsed.messages)) for (const m of parsed.messages) if (m && m.content != null) onText(m, 'content');
  await Promise.all(tasks);
}

function createGateway({ getConfig }) {
  const emitter = new EventEmitter();
  let server = null;
  let currentPort = null;
  let exchangeSeq = 0;
  // Real upstream model ids per provider, captured opportunistically from any /v1/models
  // response we proxy (Claude Code probes that endpoint on startup). Lets routing recognize
  // a model the provider genuinely has — e.g. a not-yet-configured `glm-5.2` — and pass it
  // through instead of remapping it. Best-effort: empty until the first models probe.
  const modelsCache = new Map(); // providerId -> Set<string>
  function recordModels(providerId, list) {
    if (!providerId || !Array.isArray(list)) return;
    const ids = new Set();
    for (const m of list) { if (m && typeof m.id === 'string' && m.id) ids.add(m.id); }
    if (ids.size) modelsCache.set(providerId, ids);
  }

  function log(level, msg, extra) {
    emitter.emit('log', Object.assign({ level, msg }, extra || {}));
  }

  async function handle(req, res, startedAt) {
    const config = getConfig() || {};

    // Optional local access token (defense in depth; we already bind to localhost).
    if (config.requireToken && config.gatewayToken) {
      const auth = req.headers['authorization'] || '';
      const presented = auth.replace(/^Bearer\s+/i, '') || req.headers['x-api-key'] || '';
      if (presented !== config.gatewayToken) {
        respondJson(res, 401, JSON.parse(errorBody('ccbud: invalid gateway token', 'authentication_error')));
        return;
      }
    }

    const isJson = (req.headers['content-type'] || '').includes('application/json');
    let parsed = null;
    let requestedModel = null;
    if (req.body && req.body.length && isJson) {
      try {
        parsed = JSON.parse(req.body.toString('utf8'));
        if (parsed && typeof parsed.model === 'string') requestedModel = parsed.model;
      } catch (_) {
        parsed = null;
      }
    }

    // Look up the active provider's captured model list so routing can recognize a model
    // the provider really has (mirrors resolveRouting's own active-provider selection).
    const providersList = config.providers || [];
    const activeForCache = providersList.find((p) => p.id === config.activeProviderId) || providersList[0];
    const knownModels = activeForCache ? modelsCache.get(activeForCache.id) : null;

    const routing = resolveRouting(requestedModel, config, knownModels);
    if (!routing || !routing.provider) {
      respondJson(res, 502, JSON.parse(errorBody('ccbud: no provider configured. Add one in the app.', 'api_error')));
      log('warn', 'request rejected: no provider configured');
      return;
    }
    const provider = routing.provider;

    // Presidio: redact PII from the outbound request body before it leaves the machine.
    let redactedBody = false;
    const px = config.presidio || {};
    if (px.enabled && parsed && (Array.isArray(parsed.messages) || parsed.system != null)) {
      try {
        await redactRequestBody(parsed, px);
        redactedBody = true;
      } catch (e) {
        if (!px.failOpen) {
          respondJson(res, 503, JSON.parse(errorBody('ccbud: Presidio content filter not ready — request blocked to prevent leaks. Check Presidio in Settings or turn it off.', 'api_error')));
          log('warn', 'presidio redact failed (fail-closed): ' + (e && e.message));
          return;
        }
        log('warn', 'presidio redact failed (fail-open, forwarding raw): ' + (e && e.message));
      }
    }

    let outBody = req.body || Buffer.alloc(0);
    if (parsed && (redactedBody || (routing.outgoingModel && routing.outgoingModel !== requestedModel))) {
      if (routing.outgoingModel && routing.outgoingModel !== requestedModel) parsed.model = routing.outgoingModel;
      outBody = Buffer.from(JSON.stringify(parsed), 'utf8');
    }
    const needRewriteResponse =
      routing.clientFacingModel != null &&
      routing.outgoingModel != null &&
      routing.clientFacingModel !== routing.outgoingModel;

    // Session/agent ids (for the request log + usage attribution).
    const sessionId = req.headers['x-claude-code-session-id'] || req.headers['x-claude-session-id'] || req.headers['x-session-id'] || req.headers['anthropic-client-session-id'] || null;
    const agentId = req.headers['x-claude-code-agent-id'] || null;

    // GET /v1/models — pass the upstream list through but augment with the user's aliases,
    // or synthesize it from aliases when the provider has no (working) models endpoint.
    const reqPath = req.url.split('?')[0];
    const isModelsList = req.method === 'GET' && /\/v1\/models\/?$/.test(reqPath);
    // Claude Desktop/Code probes the endpoint with `HEAD /` as a liveness check. Some
    // Anthropic-compatible upstreams don't implement it and answer 404, which makes the
    // client treat the endpoint as down. We still forward the probe honestly, but if it
    // 404s we substitute a 200 from the gateway (see the upstream-response handler).
    const isHeadRoot = req.method === 'HEAD' && reqPath === '/';
    // Claude Code calls POST /v1/messages/count_tokens before sending, to size context.
    // Many providers don't implement it (404) → forward honestly, estimate locally on miss.
    const isCountTokens = req.method === 'POST' && /\/v1\/messages\/count_tokens\/?$/.test(reqPath);

    let target;
    try {
      const base = new URL(provider.baseUrl);
      const basePath = base.pathname.replace(/\/+$/, '');
      target = new URL(base.protocol + '//' + base.host + basePath + req.url);
    } catch (e) {
      respondJson(res, 502, JSON.parse(errorBody('ccbud: invalid provider baseUrl: ' + provider.baseUrl, 'api_error')));
      return;
    }

    const headers = Object.assign({}, req.headers);
    delete headers['host'];
    delete headers['content-length'];
    delete headers['authorization'];
    delete headers['x-api-key'];
    delete headers['accept-encoding'];
    // do not leak local-client state / hop-by-hop headers to the third-party upstream
    delete headers['cookie'];
    delete headers['proxy-authorization'];
    delete headers['connection'];
    delete headers['proxy-connection'];
    delete headers['transfer-encoding'];
    headers['host'] = target.host;
    headers['accept-encoding'] = 'identity';
    if (provider.authToken) {
      headers['authorization'] = 'Bearer ' + provider.authToken;
      headers['x-api-key'] = provider.authToken;
    }
    if (outBody.length) headers['content-length'] = Buffer.byteLength(outBody);

    // Bounded, redacted capture of the full exchange so the monitor can inspect any request
    // (headers + bodies). Emitted once on completion as 'exchange'; the lightweight 'request'
    // event carries the same id so a list row can fetch its detail on click.
    const exId = ++exchangeSeq;
    const exchange = {
      id: exId,
      ts: Date.now(),
      method: req.method,
      path: req.url.split('?')[0],
      url: target.href,
      provider: provider.name || provider.id,
      requestedModel,
      outgoingModel: routing.outgoingModel,
      clientFacingModel: routing.clientFacingModel,
      rewritten: needRewriteResponse,
      sessionId,
      agentId,
      reqHeaders: redactHeaders(headers),
      reqBody: capText(outBody && outBody.length ? outBody : (req.body || Buffer.alloc(0)), REQ_CAP),
    };
    let exchangeDone = false;
    function emitExchange(status, resHeaders, capObj, errMsg) {
      if (exchangeDone) return;
      exchangeDone = true;
      exchange.status = status;
      exchange.ms = Date.now() - startedAt;
      exchange.error = errMsg || null;
      exchange.resHeaders = resHeaders ? redactHeaders(resHeaders) : {};
      exchange.resBody = capObj || { text: '', bytes: 0, truncated: 0 };
      emitter.emit('exchange', exchange);
    }

    const lib = target.protocol === 'http:' ? http : https;
    // Issue #12 — optionally skip TLS verification (self-signed / corporate MITM chains).
    const insecure = !!config.insecureSkipVerify && target.protocol === 'https:';
    // Issue #13 — retry upstream 429s a few times before surfacing them to the client.
    const rc = config.retry429 || {};
    const retryEnabled = rc.enabled !== false;
    const retryMax = Number.isFinite(rc.max) ? rc.max : 3;
    const retryBase = Number.isFinite(rc.baseMs) ? rc.baseMs : 500;

    // One upstream attempt. Re-invoked (with the same buffered body) on a retryable 429.
    function sendUpstream(attempt) {
      const opts = {
        protocol: target.protocol,
        hostname: target.hostname,
        port: target.port || (target.protocol === 'https:' ? 443 : 80),
        path: target.pathname + target.search,
        method: req.method,
        headers,
      };
      if (insecure) opts.rejectUnauthorized = false;
      const upReq = lib.request(opts, (upRes) => {
        // 429: the upstream rate-limited us (common with low-concurrency providers). Drain
        // it and retry after a short wait; only once attempts run out does the 429 reach the
        // client. Safe to retry — a rate-limited request was never processed upstream.
        if (retryEnabled && upRes.statusCode === 429 && attempt < retryMax && !res.headersSent) {
          const delay = retryDelay(upRes.headers['retry-after'], attempt, retryBase);
          upRes.resume();
          log('warn', `upstream 429 — retry ${attempt + 1}/${retryMax} in ${delay}ms (${provider.name || provider.id})`);
          const t = setTimeout(() => sendUpstream(attempt + 1), delay);
          if (t.unref) t.unref();
          return;
        }
        const ct = upRes.headers['content-type'] || '';
        const outHeaders = Object.assign({}, upRes.headers);
        delete outHeaders['content-length'];
        delete outHeaders['transfer-encoding'];
        // hop-by-hop / state-bearing headers must not cross back to the local client
        delete outHeaders['connection'];
        delete outHeaders['keep-alive'];
        delete outHeaders['proxy-authenticate'];
        delete outHeaders['proxy-connection'];
        delete outHeaders['set-cookie'];

        // Decompress if the upstream ignored our `accept-encoding: identity`.
        const enc = String(upRes.headers['content-encoding'] || '').trim().toLowerCase();
        const stages = [upRes];
        if (enc === 'gzip' || enc === 'x-gzip') stages.push(zlib.createGunzip());
        else if (enc === 'deflate') stages.push(zlib.createInflate());
        else if (enc === 'br') stages.push(zlib.createBrotliDecompress());
        delete outHeaders['content-encoding']; // body is always identity downstream now

        let logged = false;
        let capturedUsage = null;
        const finishLog = (errMsg, statusOverride) => {
          if (logged) return;
          logged = true;
          const u = capturedUsage || {};
          emitter.emit('request', {
            id: exId,
            method: req.method,
            path: req.url.split('?')[0],
            provider: provider.name || provider.id,
            requestedModel,
            outgoingModel: routing.outgoingModel,
            clientFacingModel: routing.clientFacingModel,
            rewritten: needRewriteResponse,
            sessionId,
            agentId,
            status: statusOverride != null ? statusOverride : upRes.statusCode,
            ms: Date.now() - startedAt,
            error: errMsg,
            inputTokens: u.inputTokens || 0,
            outputTokens: u.outputTokens || 0,
            cacheRead: u.cacheRead || 0,
            cacheCreation: u.cacheCreation || 0,
          });
        };

        // `HEAD /` liveness probe that the upstream rejected with 404: answer 200 from the
        // gateway (no body) so the client sees the endpoint as healthy. We flag the bypass
        // in the response headers so it's never mistaken for a genuine upstream 200.
        if (isHeadRoot && upRes.statusCode === 404) {
          upRes.resume(); // drain the upstream so its socket can be freed/reused
          const fbHeaders = Object.assign({}, outHeaders);
          fbHeaders['content-length'] = '0';
          fbHeaders['x-ccbud-fallback'] = 'head-root-404-to-200';
          fbHeaders['x-ccbud-upstream-status'] = '404';
          res.writeHead(200, fbHeaders);
          res.end();
          log('info', 'HEAD / fallback: upstream 404 → gateway 200 (' + (provider.name || provider.id) + ')');
          emitExchange(200, fbHeaders, { text: '', bytes: 0, truncated: 0 });
          finishLog(null, 200);
          return;
        }

        // Streaming SSE: pass through (rewriting model if needed) while sniffing usage,
        // and tee a capped copy of the downstream bytes for the monitor inspector.
        if (ct.includes('text/event-stream')) {
          res.writeHead(upRes.statusCode, outHeaders);
          const t = createSseTransform(needRewriteResponse ? routing.clientFacingModel : null, (u) => { capturedUsage = u; });
          const resChunks = [];
          let resCapLen = 0;
          let resTotal = 0;
          const tap = new Transform({
            transform(chunk, _enc, cb) {
              resTotal += chunk.length;
              if (resCapLen < RES_CAP) {
                const room = RES_CAP - resCapLen;
                const piece = chunk.length <= room ? chunk : chunk.slice(0, room);
                resChunks.push(piece);
                resCapLen += piece.length;
              }
              cb(null, chunk);
            },
          });
          pipeline(...stages, t, tap, res, (err) => {
            const capped = resTotal > RES_CAP;
            emitExchange(upRes.statusCode, outHeaders, {
              text: Buffer.concat(resChunks).toString('utf8').replace(capped ? /�+$/ : /(?!)/, ''),
              bytes: resTotal,
              truncated: capped ? resTotal - RES_CAP : 0,
            }, err && err.message);
            finishLog(err && err.message);
          });
          return;
        }

        // Everything else: buffer, then read usage from JSON and rewrite the model if needed.
        const cs = [];
        const collector = new Writable({ write(chunk, _enc, cb) { cs.push(chunk); cb(); } });
        pipeline(...stages, collector, (err) => {
          if (err) {
            if (isModelsList && !res.headersSent) {
              const mbuf = Buffer.from(JSON.stringify(synthesizeModels(config)), 'utf8');
              outHeaders['content-type'] = 'application/json';
              outHeaders['content-length'] = Buffer.byteLength(mbuf);
              res.writeHead(200, outHeaders);
              res.end(mbuf);
              emitExchange(200, outHeaders, capText(mbuf, RES_CAP));
              finishLog();
              return;
            }
            if (!res.headersSent) respondJson(res, 502, JSON.parse(errorBody('ccbud upstream stream error: ' + err.message, 'api_error')));
            else { try { res.destroy(); } catch (_) {} }
            emitExchange(upRes.statusCode, outHeaders, capText(Buffer.concat(cs), RES_CAP), err.message);
            finishLog(err.message);
            return;
          }
          let buf = Buffer.concat(cs);
          // count_tokens: pass the upstream's real number through when it implements the
          // endpoint; otherwise (404 / non-JSON / missing input_tokens) estimate locally so
          // Claude Code's context sizing keeps working. Flagged in headers; never under-counted.
          if (isCountTokens) {
            let upstreamOk = null;
            if (upRes.statusCode >= 200 && upRes.statusCode < 300) {
              try { const o = JSON.parse(buf.toString('utf8')); if (o && typeof o.input_tokens === 'number') upstreamOk = o; } catch (_) {}
            }
            if (upstreamOk) {
              outHeaders['x-ccbud-tokens'] = 'upstream';
              outHeaders['content-length'] = Buffer.byteLength(buf);
              res.writeHead(200, outHeaders);
              res.end(buf);
              emitExchange(200, outHeaders, capText(buf, RES_CAP));
              finishLog();
              return;
            }
            const est = estimateInputTokens(parsed || {});
            const ebuf = Buffer.from(JSON.stringify({ input_tokens: est }), 'utf8');
            const eh = Object.assign({}, outHeaders);
            eh['content-type'] = 'application/json';
            eh['content-length'] = Buffer.byteLength(ebuf);
            eh['x-ccbud-tokens'] = 'estimated';
            eh['x-ccbud-upstream-status'] = String(upRes.statusCode);
            res.writeHead(200, eh);
            res.end(ebuf);
            log('info', `count_tokens estimated locally (upstream ${upRes.statusCode}): ${est}`);
            emitExchange(200, eh, capText(ebuf, RES_CAP));
            finishLog(null, 200);
            return;
          }
          // /v1/models: merge aliases into a working upstream list, else synthesize from aliases.
          if (isModelsList) {
            let upstreamObj = null;
            if (upRes.statusCode >= 200 && upRes.statusCode < 300) {
              try { const o = JSON.parse(buf.toString('utf8')); if (o && Array.isArray(o.data)) upstreamObj = o; } catch (_) {}
            }
            if (upstreamObj) recordModels(provider.id, upstreamObj.data); // feed real-model routing
            const result = upstreamObj ? mergeModels(upstreamObj, config) : synthesizeModels(config);
            buf = Buffer.from(JSON.stringify(result), 'utf8');
            outHeaders['content-type'] = 'application/json';
            outHeaders['content-length'] = Buffer.byteLength(buf);
            res.writeHead(200, outHeaders);
            res.end(buf);
            emitExchange(200, outHeaders, capText(buf, RES_CAP));
            finishLog();
            return;
          }
          if (ct.includes('application/json')) {
            try {
              const o = JSON.parse(buf.toString('utf8'));
              if (o) {
                if (o.usage) capturedUsage = extractUsage(o.usage);
                if (needRewriteResponse && typeof o.model === 'string') {
                  o.model = routing.clientFacingModel;
                  buf = Buffer.from(JSON.stringify(o), 'utf8');
                }
              }
            } catch (_) { /* leave as-is */ }
          }
          outHeaders['content-length'] = Buffer.byteLength(buf);
          res.writeHead(upRes.statusCode, outHeaders);
          res.end(buf);
          emitExchange(upRes.statusCode, outHeaders, capText(buf, RES_CAP));
          finishLog();
        });
      }
    );

    upReq.on('error', (err) => {
      // Provider unreachable / no models endpoint → still answer /v1/models from the aliases.
      if (isModelsList && !res.headersSent) {
        const mbuf = Buffer.from(JSON.stringify(synthesizeModels(config)), 'utf8');
        try {
          res.writeHead(200, { 'content-type': 'application/json', 'content-length': Buffer.byteLength(mbuf) });
          res.end(mbuf);
        } catch (_) {}
        log('info', '/v1/models synthesized from aliases (upstream unreachable: ' + err.message + ')');
        emitExchange(200, { 'content-type': 'application/json' }, capText(mbuf, RES_CAP));
        emitter.emit('request', {
          id: exId, method: req.method, path: reqPath, provider: provider.name || provider.id,
          requestedModel, outgoingModel: routing.outgoingModel, clientFacingModel: routing.clientFacingModel,
          rewritten: needRewriteResponse, sessionId, agentId, status: 200, ms: Date.now() - startedAt, error: null,
        });
        return;
      }
      // count_tokens with the provider unreachable → estimate locally instead of erroring,
      // so Claude Code still gets a usable number.
      if (isCountTokens && !res.headersSent) {
        const est = estimateInputTokens(parsed || {});
        const ebuf = Buffer.from(JSON.stringify({ input_tokens: est }), 'utf8');
        const eh = { 'content-type': 'application/json', 'content-length': Buffer.byteLength(ebuf), 'x-ccbud-tokens': 'estimated', 'x-ccbud-upstream-status': 'error' };
        try { res.writeHead(200, eh); res.end(ebuf); } catch (_) {}
        log('info', 'count_tokens estimated locally (upstream unreachable: ' + err.message + '): ' + est);
        emitExchange(200, eh, capText(ebuf, RES_CAP));
        emitter.emit('request', {
          id: exId, method: req.method, path: reqPath, provider: provider.name || provider.id,
          requestedModel, outgoingModel: routing.outgoingModel, clientFacingModel: routing.clientFacingModel,
          rewritten: needRewriteResponse, sessionId, agentId, status: 200, ms: Date.now() - startedAt, error: null,
        });
        return;
      }
      if (!res.headersSent) {
        respondJson(res, 502, JSON.parse(errorBody('ccbud upstream error: ' + err.message, 'api_error')));
      } else {
        try {
          res.destroy();
        } catch (_) {}
      }
      log('error', 'upstream error: ' + err.message, { provider: provider.name });
      emitExchange(502, null, null, err.message);
      emitter.emit('request', {
        id: exId,
        method: req.method,
        path: req.url.split('?')[0],
        provider: provider.name || provider.id,
        requestedModel,
        outgoingModel: routing.outgoingModel,
        clientFacingModel: routing.clientFacingModel,
        rewritten: needRewriteResponse,
        sessionId,
        agentId,
        status: 502,
        ms: Date.now() - startedAt,
        error: err.message,
      });
      });

      if (outBody.length) upReq.write(outBody);
      upReq.end();
    }

    sendUpstream(0);
  }

  function onRequest(req, res) {
    const startedAt = Date.now();
    const chunks = [];
    req.on('data', (c) => chunks.push(c));
    req.on('end', () => {
      req.body = Buffer.concat(chunks);
      Promise.resolve()
        .then(() => handle(req, res, startedAt))
        .catch((e) => {
          try {
            if (!res.headersSent) respondJson(res, 500, JSON.parse(errorBody('ccbud internal error: ' + (e && e.message ? e.message : e), 'api_error')));
            else res.destroy();
          } catch (_) {}
        });
    });
    req.on('error', () => {
      try {
        res.end();
      } catch (_) {}
    });
  }

  function _start(port) {
    return new Promise((resolve, reject) => {
      if (server) return resolve(currentPort);
      const srv = http.createServer(onRequest);
      // One-shot handler for bind failures only; removed once listening so a later
      // runtime error cannot corrupt lifecycle state (null out a live server).
      const onBindError = (e) => {
        server = null;
        reject(e);
      };
      srv.once('error', onBindError);
      srv.listen(port, '127.0.0.1', () => {
        srv.removeListener('error', onBindError);
        srv.on('error', (e) => log('error', 'gateway server error: ' + (e && e.message ? e.message : e)));
        server = srv;
        currentPort = srv.address().port;
        log('info', `gateway listening on http://127.0.0.1:${currentPort}`);
        resolve(currentPort);
      });
    });
  }

  function _stop() {
    return new Promise((resolve) => {
      if (!server) return resolve();
      const srv = server;
      let done = false;
      const finish = () => {
        if (done) return;
        done = true;
        clearTimeout(timer);
        server = null;
        currentPort = null;
        log('info', 'gateway stopped');
        resolve();
      };
      srv.close(finish);
      // Free idle keep-alive sockets, and force-close active (streaming) sockets so
      // close() can actually complete instead of hanging on a long-lived SSE stream.
      if (typeof srv.closeIdleConnections === 'function') srv.closeIdleConnections();
      if (typeof srv.closeAllConnections === 'function') srv.closeAllConnections();
      // Bounded fallback so stop() always resolves (older runtimes / lingering sockets).
      const timer = setTimeout(finish, 2000);
      if (timer.unref) timer.unref();
    });
  }

  // Serialize all lifecycle ops so start/stop never interleave (no double-bind /
  // EADDRINUSE clobber, no stale early-return) regardless of which IPC path calls them.
  let lifecycleChain = Promise.resolve();
  function serialize(fn) {
    const run = lifecycleChain.then(fn, fn);
    lifecycleChain = run.catch(() => {});
    return run;
  }
  function start(port) {
    return serialize(() => _start(port));
  }
  function stop() {
    return serialize(() => _stop());
  }

  function status() {
    return { running: !!server, port: currentPort };
  }

  return {
    on: emitter.on.bind(emitter),
    off: emitter.off.bind(emitter),
    start,
    stop,
    status,
    // exported for testing
    _resolveRouting: (m, c, k) => resolveRouting(m, c, k),
  };
}

module.exports = { createGateway, resolveRouting, createSseTransform, extractUsage, retryDelay };
