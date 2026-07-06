'use strict';

/**
 * Usage analytics computed from on-disk history (.jsonl), across one or more configured config
 * directories. Two session trees contribute per work dir: Claude Code `projects/` (assistant
 * records' message.usage, incl. per-session subagent transcripts) and Codex `sessions/` rollouts
 * (`token_count` events; model from the preceding `turn_context`). Aggregated into the same
 * per-day bucket shape usage.js exposes, so the tray heatmap / stats / per-model panels are
 * driven by the authoritative on-disk data rather than only what passed through the gateway live.
 *
 * - getDirs() supplies the ACTIVE set of `projects` directories to aggregate (honors the
 *   directory switcher: 'all' → every configured dir, or a single selected one);
 *   getSessionDirs() supplies the matching Codex `sessions` directories.
 * - Per-file results are cached by (mtime,size); only changed files are re-parsed.
 * - Claude records are de-duplicated by assistant message.id so a session copied across files
 *   (resume/fork) is never double-counted. Codex rollouts append in place (a fork re-persists
 *   only conversation items, not token_count events), so their lines are counted as-is.
 */

const fs = require('fs');
const path = require('path');
const { queryUsage, rangeTokens, bump } = require('./usage');

function parseAssistantUsage(raw) {
  const recs = [];
  for (const line of raw.split('\n')) {
    const s = line.trim();
    if (!s) continue;
    let r;
    try { r = JSON.parse(s); } catch (_) { continue; }
    if (r.type !== 'assistant' || !r.message) continue;
    const u = r.message.usage;
    if (!u) continue;
    const inputTokens = u.input_tokens || 0;
    const outputTokens = u.output_tokens || 0;
    const cacheRead = u.cache_read_input_tokens || 0;
    const cacheCreation = u.cache_creation_input_tokens || 0;
    // Skip synthetic / empty turns (e.g. model "<synthetic>" with all-zero usage) — they are
    // not real requests and would only add noise to the per-model breakdown.
    if (inputTokens + outputTokens + cacheRead + cacheCreation === 0) continue;
    const ts = r.timestamp ? Date.parse(r.timestamp) : NaN;
    recs.push({
      id: r.message.id || null,
      ts: isNaN(ts) ? null : ts,
      model: r.message.model || 'unknown',
      inputTokens,
      outputTokens,
      cacheRead,
      cacheCreation,
    });
  }
  return recs;
}

/** Codex rollout .jsonl → per-turn usage recs. `token_count` events carry
 *  `info.last_token_usage` (input includes the cached portion — split out, mirroring codex.js);
 *  the active model rides the preceding `turn_context` record. */
function parseCodexUsage(raw) {
  const recs = [];
  let model = 'codex';
  for (const line of raw.split('\n')) {
    const s = line.trim();
    if (!s) continue;
    let r;
    try { r = JSON.parse(s); } catch (_) { continue; }
    const p = r.payload || {};
    if (r.type === 'turn_context') { if (p.model) model = p.model; continue; }
    if (r.type !== 'event_msg' || p.type !== 'token_count') continue;
    const u = p.info && p.info.last_token_usage; // info is null on rate-limit-only updates
    if (!u) continue;
    const input = u.input_tokens || 0, cached = u.cached_input_tokens || 0, output = u.output_tokens || 0;
    if (input + cached + output === 0) continue;
    const ts = r.timestamp ? Date.parse(r.timestamp) : NaN;
    recs.push({
      id: null, // rollouts append in place; no cross-file duplication to de-dup
      ts: isNaN(ts) ? null : ts,
      model,
      inputTokens: Math.max(0, input - cached),
      outputTokens: output,
      cacheRead: cached,
      cacheCreation: 0,
    });
  }
  return recs;
}

const MAX_FILE = 64 * 1024 * 1024; // skip pathologically large files so buildData can't OOM

function createInsights(opts) {
  const getDirs = (opts && opts.getDirs) || (() => []);
  const getSessionDirs = (opts && opts.getSessionDirs) || (() => []);
  const fileCache = new Map(); // absolute file path -> { mtime, size, recs }
  let memo = null, memoAt = 0; // short-TTL cache so back-to-back query()/rangeTokens() share one scan

  function eachFile(cb) {
    for (const root of getDirs() || []) {
      let projDirs;
      try { projDirs = fs.readdirSync(root, { withFileTypes: true }); } catch (_) { continue; }
      for (const d of projDirs) {
        if (!d.isDirectory()) continue;
        const pdir = path.join(root, d.name);
        let files;
        try { files = fs.readdirSync(pdir, { withFileTypes: true }); } catch (_) { continue; }
        for (const f of files) {
          if (f.isFile() && f.name.endsWith('.jsonl')) cb(path.join(pdir, f.name));
          else if (f.isDirectory()) {
            // Subagent transcripts live one level deeper, per session:
            // <proj>/<session>/subagents/agent-*.jsonl. (A bare <proj>/subagents dir is
            // tolerated too for older layouts.)
            const sub = f.name === 'subagents' ? path.join(pdir, f.name) : path.join(pdir, f.name, 'subagents');
            let sfiles;
            try { sfiles = fs.readdirSync(sub); } catch (_) { continue; }
            for (const sf of sfiles) if (sf.endsWith('.jsonl')) cb(path.join(sub, sf));
          }
        }
      }
    }
  }

  /** Codex rollouts live under sessions/YYYY/MM/DD/rollout-*.jsonl — walk depth-capped. */
  function eachCodexFile(cb) {
    const walk = (dir, depth) => {
      if (depth > 6) return;
      let entries;
      try { entries = fs.readdirSync(dir, { withFileTypes: true }); } catch (_) { return; }
      for (const e of entries) {
        const p = path.join(dir, e.name);
        if (e.isDirectory()) walk(p, depth + 1);
        else if (e.isFile() && e.name.endsWith('.jsonl')) cb(p);
      }
    };
    for (const root of getSessionDirs() || []) walk(root, 0);
  }

  async function buildData() {
    const data = { days: {} };
    const seen = new Set();
    const live = new Set();
    const files = [];
    eachFile((file) => files.push([file, parseAssistantUsage]));
    eachCodexFile((file) => files.push([file, parseCodexUsage]));

    for (const [file, parse] of files) {
      live.add(file);
      let st;
      try { st = await fs.promises.stat(file); } catch (_) { continue; }
      let entry = fileCache.get(file);
      if (!entry || entry.mtime !== st.mtimeMs || entry.size !== st.size) {
        if (st.size > MAX_FILE) {
          entry = { mtime: st.mtimeMs, size: st.size, recs: [] };
        } else {
          try {
            const raw = await fs.promises.readFile(file, 'utf8');
            entry = { mtime: st.mtimeMs, size: st.size, recs: parse(raw) };
          } catch (_) {
            continue;
          }
        }
        fileCache.set(file, entry);
      }
      // Undated assistant turns: fall back to the file's mtime (the session day) rather than
      // letting bump() default to today — which would inflate today's heatmap/stats.
      const fallbackTs = st.mtimeMs;
      for (const r of entry.recs) {
        if (r.id) { if (seen.has(r.id)) continue; seen.add(r.id); }
        bump(data, r.ts != null ? r : Object.assign({}, r, { ts: fallbackTs }));
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

module.exports = { createInsights, parseAssistantUsage, parseCodexUsage };
