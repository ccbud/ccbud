'use strict';

/**
 * Standalone conversation export → a single self-contained .html "viewer app".
 *
 * Built MAIN-process side because it needs fs access to the on-disk subagent files
 * (`<session>/subagents/agent-<id>.jsonl` + `.meta.json`), which the renderer never loads.
 * The output embeds the conversation as JSON plus a Claude-design skin (light/dark) and a
 * client-side runtime (render + theme toggle + search + sidebar + expandable tools/subagents).
 *
 * Subagent correlation: each `agent-<id>.meta.json` carries `{ agentType, description, toolUseId }`;
 * `toolUseId` equals the `Agent`/`Task` tool_use `id` in the parent timeline, so the runtime can
 * nest a subagent's full timeline directly under the call that spawned it.
 */

const fs = require('fs');
const path = require('path');

const CAP = { text: 24000, thinking: 16000, result: 24000, prompt: 9000, content: 14000 };
function cap(s, n) {
  s = s == null ? '' : String(s);
  return s.length > n ? s.slice(0, n) + '\n…[truncated ' + (s.length - n) + ' chars]' : s;
}

function parseJsonl(file) {
  let raw;
  try { raw = fs.readFileSync(file, 'utf8'); } catch (_) { return []; }
  const out = [];
  for (const line of raw.split('\n')) {
    const s = line.trim();
    if (!s) continue;
    try { out.push(JSON.parse(s)); } catch (_) {}
  }
  return out;
}

function usageOf(u) {
  if (!u) return null;
  return { in: u.input_tokens || 0, out: u.output_tokens || 0, cacheRead: u.cache_read_input_tokens || 0, cacheCreation: u.cache_creation_input_tokens || 0 };
}

// Cap the heavy fields inside a content array so the embedded JSON stays bounded.
function capContent(content) {
  if (typeof content === 'string') return cap(content, CAP.text);
  if (!Array.isArray(content)) return content;
  return content.map((b) => {
    if (!b || typeof b !== 'object') return b;
    if (b.type === 'text') return { type: 'text', text: cap(b.text, CAP.text) };
    if (b.type === 'thinking') return { type: 'thinking', thinking: cap(b.thinking, CAP.thinking) };
    if (b.type === 'tool_use') {
      const input = Object.assign({}, b.input || {});
      if (typeof input.prompt === 'string') input.prompt = cap(input.prompt, CAP.prompt);
      if (typeof input.content === 'string') input.content = cap(input.content, CAP.content);
      return { type: 'tool_use', id: b.id, name: b.name, input };
    }
    if (b.type === 'tool_result') {
      let c = b.content;
      if (typeof c === 'string') c = cap(c, CAP.result);
      else if (Array.isArray(c)) c = c.map((x) => (x && x.type === 'text' ? { type: 'text', text: cap(x.text, CAP.result) } : x));
      return { type: 'tool_result', tool_use_id: b.tool_use_id, is_error: !!b.is_error, content: c };
    }
    if (b.type === 'image') {
      // keep small inline images; drop very large base64 to a placeholder to bound size
      const data = b.source && b.source.data;
      if (data && data.length > 600000) return { type: 'image', source: { media_type: (b.source && b.source.media_type) || 'image/png', oversized: true } };
      return b;
    }
    return b;
  });
}

function lineToMsg(rec) {
  if (!rec || (rec.type !== 'user' && rec.type !== 'assistant') || !rec.message) return null;
  const m = rec.message;
  if (!m.role) return null;
  const out = { role: m.role, content: capContent(m.content), ts: rec.timestamp || null, meta: !!rec.isMeta };
  if (rec.type === 'assistant') {
    out.model = m.model || null;
    out.usage = usageOf(m.usage);
    out.stop = m.stop_reason || null;
  }
  return out;
}

function contentText(content) {
  if (typeof content === 'string') return content;
  if (Array.isArray(content)) return content.filter((b) => b && b.type === 'text').map((b) => b.text || '').join(' ');
  return '';
}
function firstUserText(messages) {
  for (const m of messages) {
    if (!m || m.role !== 'user' || m.meta) continue;
    const t = contentText(m.content).trim().replace(/\s+/g, ' ');
    if (!t || t.startsWith('<') || /^(\[Request interrupted|Caveat:)/.test(t)) continue;
    return t.slice(0, 100);
  }
  return '';
}
const baseName = (p) => (p ? String(p).split('/').filter(Boolean).pop() : null);

function shapeSession(recs) {
  const metaRec = recs.find((r) => r.cwd) || recs.find((r) => r.sessionId) || {};
  const messages = [];
  const totals = { in: 0, out: 0, cacheRead: 0, turns: 0 };
  let model = null, firstTs = null, lastTs = null;
  for (const r of recs) {
    const lm = lineToMsg(r);
    if (!lm || lm.meta) continue;
    if (lm.ts) { if (!firstTs) firstTs = lm.ts; lastTs = lm.ts; }
    if (lm.model) model = lm.model;
    if (lm.usage) { totals.in += lm.usage.in; totals.out += lm.usage.out; totals.cacheRead += lm.usage.cacheRead; totals.turns += 1; }
    messages.push(lm);
  }
  return { messages, model, totals, firstTs, lastTs, metaRec };
}

// Read the subagent dialogues for a session file: <projectDir>/<sessionId>/subagents/agent-<id>.{jsonl,meta.json}
function readSubagents(file) {
  const dir = path.join(path.dirname(file), path.basename(file, '.jsonl'), 'subagents');
  let entries;
  try { entries = fs.readdirSync(dir); } catch (_) { return {}; }
  const byTool = {};
  for (const name of entries) {
    if (!/^agent-.*\.jsonl$/.test(name)) continue;
    const agentId = name.replace(/^agent-/, '').replace(/\.jsonl$/, '');
    let meta = {};
    try { meta = JSON.parse(fs.readFileSync(path.join(dir, 'agent-' + agentId + '.meta.json'), 'utf8')); } catch (_) {}
    const shaped = shapeSession(parseJsonl(path.join(dir, name)));
    const sub = {
      agentId,
      type: meta.agentType || meta.subagent_type || 'agent',
      description: meta.description || '',
      count: shaped.messages.length,
      totals: shaped.totals,
      messages: shaped.messages,
    };
    const key = meta.toolUseId || ('agent:' + agentId);
    byTool[key] = sub;
  }
  return byTool;
}

function buildData(file) {
  const recs = parseJsonl(file);
  const s = shapeSession(recs);
  const cwd = s.metaRec.cwd || null;
  const subagents = readSubagents(file);
  return {
    meta: {
      title: firstUserText(s.messages) || '(conversation)',
      model: s.model,
      project: baseName(cwd),
      cwd,
      branch: s.metaRec.gitBranch || null,
      sessionId: s.metaRec.sessionId || path.basename(file, '.jsonl'),
      version: s.metaRec.version || null,
      count: s.messages.length,
      turns: s.totals.turns,
      inTok: s.totals.in,
      outTok: s.totals.out,
      cacheTok: s.totals.cacheRead,
      subagentCount: Object.keys(subagents).length,
      firstTs: s.firstTs,
      lastTs: s.lastTs,
    },
    messages: s.messages,
    subagents,
  };
}

function readAsset(rel) {
  try { return fs.readFileSync(path.join(__dirname, rel), 'utf8'); } catch (_) { return ''; }
}

/** Build the full self-contained HTML document from an already-parsed data object. */
function htmlFromData(data) {
  const skin = readAsset('export-assets/skin.css');
  const runtime = readAsset('export-assets/runtime.js');
  const marked = readAsset(path.join('..', 'renderer', 'vendor', 'marked.umd.js'));
  const hljs = readAsset(path.join('..', 'renderer', 'vendor', 'highlight.min.js'));
  const hljsCss = readAsset(path.join('..', 'renderer', 'vendor', 'hljs-dark.css')); // code blocks are dark in both themes
  const json = JSON.stringify(data).replace(/</g, '\\u003c');
  const title = (data.meta.title || 'Conversation').replace(/[<>]/g, '');
  return '<!doctype html><html lang="zh" data-theme="light"><head><meta charset="utf-8">'
    + '<meta name="viewport" content="width=device-width,initial-scale=1">'
    + '<title>' + title + ' · ccbud</title>'
    + '<style>' + skin + '\n' + hljsCss + '</style>'
    + '</head><body><div id="app"></div>'
    + '<script>' + marked + '</script>'
    + '<script>' + hljs + '</script>'
    + '<script>window.__CONV__=' + json + ';</script>'
    + '<script>' + runtime + '</script>'
    + '</body></html>';
}

/** Convenience: parse a session file and build its standalone HTML in one call. */
function buildExportHtml(file) { return htmlFromData(buildData(file)); }

module.exports = { buildExportHtml, buildData, htmlFromData };
