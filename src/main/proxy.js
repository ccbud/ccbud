'use strict';

/**
 * Clawdy Gateway — pure Node proxy core (no Electron dependency, fully testable).
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

/**
 * Decide which provider to use and how to translate the model name.
 * Returns { provider, outgoingModel, clientFacingModel } or null.
 *
 * clientFacingModel === outgoingModel  => pure passthrough (do NOT touch response)
 * clientFacingModel !== outgoingModel  => we changed the model, so rewrite response back
 */
function resolveRouting(requestedModel, config) {
  const providers = (config && config.providers) || [];
  if (providers.length === 0) return null;

  const active = providers.find((p) => p.id === config.activeProviderId) || providers[0];

  if (!requestedModel) {
    return active ? { provider: active, outgoingModel: null, clientFacingModel: null } : null;
  }

  // 1) Explicit alias match across ALL providers -> route to the owning provider.
  for (const p of providers) {
    for (const m of p.models || []) {
      if (m.alias && m.alias === requestedModel && m.upstream) {
        return { provider: p, outgoingModel: m.upstream, clientFacingModel: requestedModel };
      }
    }
  }

  if (!active) return null;

  // 2) The client used a real upstream model name of the active provider -> passthrough.
  if (requestedModel === active.defaultModel || requestedModel === active.smallFastModel) {
    return { provider: active, outgoingModel: requestedModel, clientFacingModel: requestedModel };
  }
  for (const m of active.models || []) {
    if (m.upstream === requestedModel) {
      return { provider: active, outgoingModel: requestedModel, clientFacingModel: requestedModel };
    }
  }

  // 3) Automatic mapping of Claude DEFAULT model names (claude-*) to the active
  //    provider's models. Gated on the claude-* naming convention so that a
  //    deliberately-requested non-claude model is never silently substituted.
  if (active.mapDefaultModels !== false && /^claude[-_]/i.test(requestedModel)) {
    const small = active.smallFastModel || active.defaultModel;
    const big = active.defaultModel || active.smallFastModel;
    // Per the goal: haiku -> small fast model, everything else -> main model.
    const target = looksSmall(requestedModel) ? small : big;
    if (target) {
      return { provider: active, outgoingModel: target, clientFacingModel: requestedModel };
    }
  }

  // 4) Unknown / explicitly-requested model, no mapping available -> passthrough as-is.
  return { provider: active, outgoingModel: requestedModel, clientFacingModel: requestedModel };
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
function createSseTransform(model, onUsage) {
  const replacement = model != null ? String(model).replace(/\$/g, '$$$$') : null;
  const re = /("model"\s*:\s*")[^"]*(")/g;
  let buffer = '';
  const usage = { inputTokens: 0, outputTokens: 0, cacheRead: 0, cacheCreation: 0 };
  let saw = false;

  function absorb(line) {
    const i = line.indexOf('{');
    if (i < 0) return;
    let obj;
    try { obj = JSON.parse(line.slice(i)); } catch (_) { return; }
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
  function handleLine(line) {
    if (replacement != null && line.indexOf('"model"') !== -1) line = line.replace(re, `$1${replacement}$2`);
    if (line.indexOf('"usage"') !== -1) absorb(line);
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

function createGateway({ getConfig }) {
  const emitter = new EventEmitter();
  let server = null;
  let currentPort = null;

  function log(level, msg, extra) {
    emitter.emit('log', Object.assign({ level, msg }, extra || {}));
  }

  function handle(req, res, startedAt) {
    const config = getConfig() || {};

    // Optional local access token (defense in depth; we already bind to localhost).
    if (config.requireToken && config.gatewayToken) {
      const auth = req.headers['authorization'] || '';
      const presented = auth.replace(/^Bearer\s+/i, '') || req.headers['x-api-key'] || '';
      if (presented !== config.gatewayToken) {
        respondJson(res, 401, JSON.parse(errorBody('Clawdy: invalid gateway token', 'authentication_error')));
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

    const routing = resolveRouting(requestedModel, config);
    if (!routing || !routing.provider) {
      respondJson(res, 502, JSON.parse(errorBody('Clawdy: no provider configured. Add one in the app.', 'api_error')));
      log('warn', 'request rejected: no provider configured');
      return;
    }
    const provider = routing.provider;

    let outBody = req.body || Buffer.alloc(0);
    if (parsed && routing.outgoingModel && routing.outgoingModel !== requestedModel) {
      parsed.model = routing.outgoingModel;
      outBody = Buffer.from(JSON.stringify(parsed), 'utf8');
    }
    const needRewriteResponse =
      routing.clientFacingModel != null &&
      routing.outgoingModel != null &&
      routing.clientFacingModel !== routing.outgoingModel;

    let target;
    try {
      const base = new URL(provider.baseUrl);
      const basePath = base.pathname.replace(/\/+$/, '');
      target = new URL(base.protocol + '//' + base.host + basePath + req.url);
    } catch (e) {
      respondJson(res, 502, JSON.parse(errorBody('Clawdy: invalid provider baseUrl: ' + provider.baseUrl, 'api_error')));
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

    const lib = target.protocol === 'http:' ? http : https;
    const upReq = lib.request(
      {
        protocol: target.protocol,
        hostname: target.hostname,
        port: target.port || (target.protocol === 'https:' ? 443 : 80),
        path: target.pathname + target.search,
        method: req.method,
        headers,
      },
      (upRes) => {
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
        const finishLog = (errMsg) => {
          if (logged) return;
          logged = true;
          const u = capturedUsage || {};
          emitter.emit('request', {
            method: req.method,
            path: req.url.split('?')[0],
            provider: provider.name || provider.id,
            requestedModel,
            outgoingModel: routing.outgoingModel,
            rewritten: needRewriteResponse,
            status: upRes.statusCode,
            ms: Date.now() - startedAt,
            error: errMsg,
            inputTokens: u.inputTokens || 0,
            outputTokens: u.outputTokens || 0,
            cacheRead: u.cacheRead || 0,
            cacheCreation: u.cacheCreation || 0,
          });
        };

        // Streaming SSE: pass through (rewriting model if needed) while sniffing usage.
        if (ct.includes('text/event-stream')) {
          res.writeHead(upRes.statusCode, outHeaders);
          const t = createSseTransform(needRewriteResponse ? routing.clientFacingModel : null, (u) => { capturedUsage = u; });
          pipeline(...stages, t, res, (err) => finishLog(err && err.message));
          return;
        }

        // Everything else: buffer, then read usage from JSON and rewrite the model if needed.
        const cs = [];
        const collector = new Writable({ write(chunk, _enc, cb) { cs.push(chunk); cb(); } });
        pipeline(...stages, collector, (err) => {
          if (err) {
            if (!res.headersSent) respondJson(res, 502, JSON.parse(errorBody('Clawdy upstream stream error: ' + err.message, 'api_error')));
            else { try { res.destroy(); } catch (_) {} }
            finishLog(err.message);
            return;
          }
          let buf = Buffer.concat(cs);
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
          finishLog();
        });
      }
    );

    upReq.on('error', (err) => {
      if (!res.headersSent) {
        respondJson(res, 502, JSON.parse(errorBody('Clawdy upstream error: ' + err.message, 'api_error')));
      } else {
        try {
          res.destroy();
        } catch (_) {}
      }
      log('error', 'upstream error: ' + err.message, { provider: provider.name });
      emitter.emit('request', {
        method: req.method,
        path: req.url.split('?')[0],
        provider: provider.name || provider.id,
        requestedModel,
        outgoingModel: routing.outgoingModel,
        rewritten: needRewriteResponse,
        status: 502,
        ms: Date.now() - startedAt,
        error: err.message,
      });
    });

    if (outBody.length) upReq.write(outBody);
    upReq.end();
  }

  function onRequest(req, res) {
    const startedAt = Date.now();
    const chunks = [];
    req.on('data', (c) => chunks.push(c));
    req.on('end', () => {
      req.body = Buffer.concat(chunks);
      try {
        handle(req, res, startedAt);
      } catch (e) {
        respondJson(res, 500, JSON.parse(errorBody('Clawdy internal error: ' + e.message, 'api_error')));
      }
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
    _resolveRouting: (m, c) => resolveRouting(m, c),
  };
}

module.exports = { createGateway, resolveRouting, createSseTransform, extractUsage };
