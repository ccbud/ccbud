'use strict';

/**
 * Usage analytics computed from on-disk history (.jsonl) — aggregation semantics ported from
 * ccusage (github.com/ccusage/ccusage), scoped to the two agents ccbud fronts. Per work dir:
 *
 *   Claude Code `projects/`+`**` (recursive, any depth — nested session dirs and subagent
 *   transcripts included by construction): every line whose `message.usage` carries numeric
 *   input/output tokens counts (no `type=="assistant"` gate); lines without a parseable
 *   timestamp are DROPPED; cache-creation prefers the nested ephemeral breakdown; `<synthetic>`
 *   keeps tokens but gets no model attribution; `usage.speed=="fast"` appends `-fast`; global
 *   de-dup by (message.id, requestId) with a sidechain fallback on message.id alone.
 *
 *   Codex `sessions/` + `archived_sessions/` (archived copy of the same relative path loses):
 *   `token_count` events — prefer `info.last_token_usage`, else diff consecutive
 *   `total_token_usage` snapshots (baseline always advances); `thread_spawn` subagent files skip
 *   the leading parent-history replay burst; identical (timestamp, model, tokens) events across
 *   files de-dup; model from payload/info, else last `turn_context`, else "gpt-5"; input is
 *   INCLUSIVE of cached (cached splits into cacheRead, remainder into input).
 *
 * - getDirs() supplies the ACTIVE set of `projects` directories; getSessionDirs() the matching
 *   Codex work dirs' roots are derived from them in main.js.
 * - Per-file parse results are cached by (mtime,size); only changed files re-parse. De-dup runs
 *   globally on every build (it must see all files at once).
 */

const fs = require('fs');
const path = require('path');
const { queryUsage, rangeTokens, bump } = require('./usage');

/* ---------------- Claude Code ---------------- */

function parseClaudeUsage(raw) {
  const recs = [];
  for (const line of raw.split('\n')) {
    const s = line.trim();
    if (!s || !s.includes('"usage"')) continue;
    let r;
    try { r = JSON.parse(s); } catch (_) { continue; }
    const m = r.message;
    const u = m && m.usage;
    if (!u || typeof u.input_tokens !== 'number' || typeof u.output_tokens !== 'number') continue;
    const inputTokens = u.input_tokens;
    const outputTokens = u.output_tokens;
    const cacheRead = u.cache_read_input_tokens || 0;
    // nested ephemeral breakdown wins over the flat cache_creation_input_tokens
    const cacheCreation = u.cache_creation && typeof u.cache_creation === 'object'
      ? (u.cache_creation.ephemeral_5m_input_tokens || 0) + (u.cache_creation.ephemeral_1h_input_tokens || 0)
      : (u.cache_creation_input_tokens || 0);
    if (inputTokens + outputTokens + cacheRead + cacheCreation <= 0) continue; // zero rows carry no info
    const ts = r.timestamp ? Date.parse(r.timestamp) : NaN;
    if (isNaN(ts)) continue; // undated lines are dropped, never guessed
    const fast = u.speed === 'fast';
    let model = typeof m.model === 'string' && m.model && m.model !== '<synthetic>' ? m.model : null;
    if (model && fast) model += '-fast';
    recs.push({
      id: m.id || null,
      requestId: r.requestId || null,
      sidechain: r.isSidechain === true,
      ts, model, inputTokens, outputTokens, cacheRead, cacheCreation,
    });
  }
  return recs;
}

const recTotal = (r) => r.inputTokens + r.outputTokens + r.cacheRead + r.cacheCreation;

/** Global de-dup, ccusage semantics: key (message.id, requestId); no id → always kept; an exact
 *  miss falls back to the id-only bucket when either side is a sidechain (a replay reuses the
 *  parent's message.id under a new requestId). Non-sidechain wins, then higher token total. */
function dedupClaude(recs) {
  const kept = [];
  const byExact = new Map();
  const byId = new Map();
  for (const cand of recs) {
    if (!cand.id) { kept.push(cand); continue; }
    const exact = `${cand.id}\u0000${cand.requestId || ''}`;
    let i = byExact.get(exact);
    if (i === undefined) {
      const j = byId.get(cand.id);
      if (j !== undefined && (cand.sidechain || kept[j].sidechain)) i = j;
    }
    if (i !== undefined) {
      const cur = kept[i];
      if ((cur.sidechain && !cand.sidechain) || (cur.sidechain === cand.sidechain && recTotal(cand) > recTotal(cur))) {
        kept[i] = cand;
      }
      byExact.set(exact, i);
    } else {
      const idx = kept.length;
      byExact.set(exact, idx);
      if (!byId.has(cand.id)) byId.set(cand.id, idx);
      kept.push(cand);
    }
  }
  return kept;
}

/* ---------------- Codex ---------------- */

function codexUsageOf(v) {
  if (!v || typeof v !== 'object') return null;
  const g = (...keys) => { for (const k of keys) if (typeof v[k] === 'number') return v[k]; return 0; };
  const input = g('input_tokens', 'prompt_tokens', 'input');
  const cached = g('cached_input_tokens', 'cache_read_input_tokens', 'cached_tokens');
  const output = g('output_tokens', 'completion_tokens', 'output');
  const reasoning = g('reasoning_output_tokens', 'reasoning_tokens');
  let total = typeof v.total_tokens === 'number' ? v.total_tokens : null;
  if (total === null || (total === 0 && input + output + reasoning > 0)) total = input + output + reasoning;
  return { input, cached, output, reasoning, total };
}

const codexSub = (cur, prev) => ({
  input: Math.max(0, cur.input - (prev ? prev.input : 0)),
  cached: Math.max(0, cur.cached - (prev ? prev.cached : 0)),
  output: Math.max(0, cur.output - (prev ? prev.output : 0)),
  reasoning: Math.max(0, cur.reasoning - (prev ? prev.reasoning : 0)),
  total: Math.max(0, cur.total - (prev ? prev.total : 0)),
});

function codexModelOf(v) {
  if (!v || typeof v !== 'object') return null;
  const m = v.model || v.model_name || (v.metadata && v.metadata.model);
  return typeof m === 'string' && m ? m : null;
}

/** Parse one Codex rollout's content into per-turn usage events (ccusage semantics). */
function parseCodexUsage(raw) {
  const events = [];
  const lines = raw.split('\n');
  // thread_spawn subagent files replay the parent history as a leading burst sharing one
  // timestamp-second — find that second so the burst is skipped (baseline still advances).
  let replaySecond = null;
  if (raw.slice(0, 16384).includes('thread_spawn')) {
    let first = null;
    for (const line of lines) {
      const t = tokenCountOf(line);
      if (!t || !t.info || (!t.info.last_token_usage && !t.info.total_token_usage)) continue;
      const second = String(t.ts).slice(0, 19);
      if (first === null) { first = second; continue; }
      replaySecond = first === second ? second : null;
      break;
    }
  }
  let skipReplay = replaySecond !== null;
  let currentModel = null;
  let prevTotals = null;
  for (const line of lines) {
    const s = line.trim();
    if (!s) continue;
    if (s.includes('turn_context')) {
      let r;
      try { r = JSON.parse(s); } catch (_) { r = null; }
      if (r && r.type === 'turn_context') {
        const m = codexModelOf(r.payload);
        if (m) currentModel = m;
        continue;
      }
    }
    const t = tokenCountOf(s);
    if (!t) continue;
    const info = t.info && typeof t.info === 'object' ? t.info : null;
    const total = info ? codexUsageOf(info.total_token_usage) : null;
    const last = info ? codexUsageOf(info.last_token_usage) : null;
    if (skipReplay) {
      if (String(t.ts).slice(0, 19) === replaySecond) {
        if (total) prevTotals = total;
        continue;
      }
      skipReplay = false;
    }
    const usage = last || (total ? codexSub(total, prevTotals) : null);
    if (total) prevTotals = total;
    if (!usage || usage.input + usage.cached + usage.output + usage.reasoning === 0) continue;
    const ts = Date.parse(t.ts);
    if (isNaN(ts)) continue;
    usage.cached = Math.min(usage.cached, usage.input); // input is INCLUSIVE of cached
    const model = codexModelOf(t.payload) || codexModelOf(info) || currentModel || 'gpt-5';
    events.push({ ts, model, ...usage });
  }
  return events;
}

function tokenCountOf(line) {
  const s = line.trim();
  if (!s || !s.includes('token_count')) return null;
  let r;
  try { r = JSON.parse(s); } catch (_) { return null; }
  if (r.type !== 'event_msg' || !r.payload || r.payload.type !== 'token_count') return null;
  if (typeof r.timestamp !== 'string') return null;
  return { ts: r.timestamp, payload: r.payload, info: r.payload.info };
}

/* ---------------- discovery + build ---------------- */

const MAX_WALK_DEPTH = 8; // symlink-loop guard

function collectJsonl(dir, depth, out) {
  if (depth > MAX_WALK_DEPTH) return;
  let entries;
  try { entries = fs.readdirSync(dir, { withFileTypes: true }); } catch (_) { return; }
  for (const e of entries) {
    const p = path.join(dir, e.name);
    if (e.isDirectory()) collectJsonl(p, depth + 1, out);
    else if (e.isFile() && e.name.endsWith('.jsonl')) out.push(p);
  }
}

function createInsights(opts) {
  const getDirs = (opts && opts.getDirs) || (() => []);
  const getSessionDirs = (opts && opts.getSessionDirs) || (() => []);
  const fileCache = new Map(); // absolute file path -> { mtime, size, recs }
  let memo = null, memoAt = 0; // short-TTL cache so back-to-back query()/rangeTokens() share one scan

  // Claude: every *.jsonl under each projects dir, any depth.
  function claudeFiles() {
    const files = [];
    for (const root of getDirs() || []) collectJsonl(root, 0, files);
    files.sort();
    return files;
  }

  // Codex: sessions/ then archived_sessions/ of each work dir; an archived copy of the same
  // relative path loses to the active sessions/ copy.
  function codexFiles() {
    const out = [];
    for (const sessionsDir of getSessionDirs() || []) {
      const root = path.dirname(sessionsDir);
      const seenRel = new Set();
      for (const sub of ['sessions', 'archived_sessions']) {
        const dir = path.join(root, sub);
        const files = [];
        collectJsonl(dir, 0, files);
        files.sort();
        for (const f of files) {
          const rel = path.relative(dir, f);
          if (!seenRel.has(rel)) { seenRel.add(rel); out.push(f); }
        }
      }
    }
    return out;
  }

  async function loadRecs(file, parse) {
    let st;
    try { st = await fs.promises.stat(file); } catch (_) { return null; }
    let entry = fileCache.get(file);
    if (!entry || entry.mtime !== st.mtimeMs || entry.size !== st.size) {
      let raw;
      try { raw = await fs.promises.readFile(file, 'utf8'); } catch (_) { return null; }
      entry = { mtime: st.mtimeMs, size: st.size, recs: parse(raw) };
      fileCache.set(file, entry);
    }
    return entry.recs;
  }

  async function buildData() {
    const data = { days: {} };
    const live = new Set();

    const claude = [];
    for (const file of claudeFiles()) {
      live.add(file);
      const recs = await loadRecs(file, parseClaudeUsage);
      if (recs) claude.push(...recs);
    }
    for (const r of dedupClaude(claude)) bump(data, r);

    const seen = new Set(); // (ts, model, tokens) — resumed/forked session copies collapse
    for (const file of codexFiles()) {
      live.add(file);
      const events = await loadRecs(file, parseCodexUsage);
      if (!events) continue;
      for (const e of events) {
        const key = `${e.ts}|${e.model}|${e.input}|${e.cached}|${e.output}|${e.reasoning}|${e.total}`;
        if (seen.has(key)) continue;
        seen.add(key);
        bump(data, {
          ts: e.ts,
          model: e.model,
          inputTokens: Math.max(0, e.input - e.cached),
          outputTokens: e.output,
          cacheRead: e.cached,
          cacheCreation: 0,
        });
      }
    }

    // evict cache entries for files that disappeared / dirs deselected
    for (const f of [...fileCache.keys()]) if (!live.has(f)) fileCache.delete(f);
    return data;
  }

  async function buildDataCached() {
    const now = Date.now();
    if (memo && now - memoAt < 1500) return memo;
    memo = await buildData();
    memoAt = now;
    return memo;
  }

  return {
    query: async (range, now) => queryUsage(await buildDataCached(), range, now),
    rangeTokens: async (range, now) => rangeTokens(await buildDataCached(), range, now),
    invalidate: (file) => { if (file) fileCache.delete(file); else fileCache.clear(); memo = null; },
    _buildData: buildData,
  };
}

module.exports = { createInsights, parseClaudeUsage, parseCodexUsage, dedupClaude };
