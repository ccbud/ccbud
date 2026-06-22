'use strict';

/**
 * Local token estimator for the `POST /v1/messages/count_tokens` fallback.
 *
 * Claude Code calls count_tokens BEFORE sending, to size the context (when to
 * auto-compact, etc.). Many Anthropic-compatible providers don't implement the
 * endpoint and answer 404, which breaks that accounting. When the gateway can't get
 * a real count it estimates one here.
 *
 * Approach (see research notes): o200k_base (the closest publicly-available tokenizer
 * to Claude 3/4 — the official @anthropic-ai/tokenizer is a stale Claude-2 vocab that
 * over-counts CJK) for the text, plus a calibrated STRUCTURAL overhead that count_tokens
 * adds for message framing / system / tools. We deliberately round UP a little: a slight
 * over-count just makes Claude Code compact a touch early, whereas under-counting could
 * let a request overflow the real upstream limit.
 */

let _enc = null; // null = not yet tried, false = unavailable, else a Tiktoken instance
function encoder() {
  if (_enc === null) {
    try {
      const { Tiktoken } = require('js-tiktoken/lite');
      const rank = require('js-tiktoken/ranks/o200k_base');
      _enc = new Tiktoken(rank && rank.default ? rank.default : rank);
    } catch (_) {
      _enc = false; // fall back to a char heuristic below
    }
  }
  return _enc || null;
}

function safeJson(v) {
  try { return v == null ? '' : JSON.stringify(v); } catch (_) { return ''; }
}

// Calibrated against the real count_tokens endpoint. o200k text + these overheads land
// a touch above the true count across single/multi-message, +system and +tools requests.
const BASE = 5;        // per-request framing (BOS / wrapper)
const PER_MSG = 4;     // per-message wrapper (role + delimiters)
const SYS = 4;         // system-prompt framing
const TOOLS = 15;      // fixed tools→system injection framing (NOT per-tool)
const IMAGE = 1600;    // images are size-priced; flat conservative estimate (rarely hit here)
const SAFETY = 1.06;   // round a little high, never under-count

/**
 * Estimate the input_tokens for an Anthropic Messages request body.
 * Mirrors what count_tokens charges: system + every message's text/tool_use/tool_result
 * + the tool definitions, plus structural overhead.
 */
function estimateInputTokens(body) {
  const enc = encoder();
  const count = enc
    ? (s) => (s ? enc.encode(String(s)).length : 0)
    : (s) => (s ? Math.ceil(String(s).length / 4) : 0); // crude fallback if tokenizer missing

  body = body || {};
  let t = 0;

  const sys = body.system;
  if (typeof sys === 'string') t += count(sys);
  else if (Array.isArray(sys)) for (const b of sys) if (b && b.type === 'text') t += count(b.text);

  const msgs = Array.isArray(body.messages) ? body.messages : [];
  for (const m of msgs) {
    const c = m && m.content;
    if (typeof c === 'string') { t += count(c); continue; }
    if (!Array.isArray(c)) continue;
    for (const b of c) {
      if (!b || typeof b !== 'object') continue;
      if (b.type === 'text') t += count(b.text);
      else if (b.type === 'tool_use') t += count(b.name) + count(safeJson(b.input));
      else if (b.type === 'tool_result') {
        if (typeof b.content === 'string') t += count(b.content);
        else if (Array.isArray(b.content)) for (const x of b.content) if (x && x.type === 'text') t += count(x.text);
      } else if (b.type === 'image') t += IMAGE;
    }
  }

  const tools = Array.isArray(body.tools) ? body.tools : [];
  for (const tool of tools) {
    if (!tool) continue;
    t += count(tool.name) + count(tool.description) + count(safeJson(tool.input_schema));
  }

  const overhead = BASE + PER_MSG * msgs.length + (sys ? SYS : 0) + (tools.length ? TOOLS : 0);
  return Math.max(1, Math.ceil((t + overhead) * SAFETY));
}

// Is the local tokenizer actually loaded (vs the crude char fallback)? For diagnostics/tests.
function tokenizerReady() {
  return encoder() != null;
}

module.exports = { estimateInputTokens, tokenizerReady };
