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
      if (typeof input.patch === 'string') input.patch = cap(input.patch, CAP.content); // codex ApplyPatch envelopes can be huge
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
// A slash-command turn is stored as XML tags, e.g.
//   <command-name>/model</command-name> <command-args>fable-5</command-args>
// Surface it as a readable "/model fable-5" label instead of leaking the raw tags.
function commandLabel(raw) {
  const name = (raw.match(/<command-name>([^<]*)<\/command-name>/) || [])[1];
  if (!name) return '';
  const args = (raw.match(/<command-args>([^<]*)<\/command-args>/) || [])[1] || '';
  return (name.trim() + ' ' + args.trim()).trim();
}
function firstUserText(messages) {
  let fallbackCmd = '';
  for (const m of messages) {
    if (!m || m.role !== 'user' || m.meta) continue;
    const raw = contentText(m.content).trim();
    if (!raw) continue;
    if (raw.startsWith('<')) { if (!fallbackCmd) fallbackCmd = commandLabel(raw); continue; }
    const t = raw.replace(/\s+/g, ' ');
    if (/^(\[Request interrupted|Caveat:)/.test(t)) continue;
    return t.slice(0, 100);
  }
  return fallbackCmd.slice(0, 100);
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
  const { skillFromRecs } = require('./history');
  const byTool = {};
  for (const name of entries) {
    if (!/^agent-.*\.jsonl$/.test(name)) continue;
    const agentId = name.replace(/^agent-/, '').replace(/\.jsonl$/, '');
    let meta = {};
    try { meta = JSON.parse(fs.readFileSync(path.join(dir, 'agent-' + agentId + '.meta.json'), 'utf8')); } catch (_) {}
    const recs = parseJsonl(path.join(dir, name));
    const shaped = shapeSession(recs);
    const sub = {
      agentId,
      type: meta.agentType || meta.subagent_type || 'agent',
      description: meta.description || '',
      skill: skillFromRecs(recs),
      count: shaped.messages.length,
      totals: shaped.totals,
      messages: shaped.messages,
    };
    const key = meta.toolUseId || ('agent:' + agentId);
    byTool[key] = sub;
  }
  return byTool;
}

// Codex rollout → the same export data shape (messages re-capped + field names the viewer
// runtime reads: model / usage{in,out,cacheRead}), with meta.assistant = "Codex" so the
// exported page labels turns correctly.
function buildCodexData(file, recs) {
  const sess = require('./codex').sessionFromRecs(file, recs);
  const m = sess.meta || {};
  const messages = (sess.messages || []).map((msg) => {
    const out = { role: msg.role, content: capContent(msg.content), ts: msg.ts || null, meta: false };
    if (msg.modelActual) out.model = msg.modelActual;
    if (msg.usage) {
      out.usage = {
        in: msg.usage.inputTokens || 0,
        out: msg.usage.outputTokens || 0,
        cacheRead: msg.usage.cacheRead || 0,
        cacheCreation: msg.usage.cacheCreation || 0,
      };
    }
    return out;
  });
  const t = m.totals || {};
  return {
    meta: {
      title: m.title || '(conversation)',
      assistant: 'Codex',
      model: m.model || null,
      project: m.project || null,
      cwd: m.cwd || null,
      branch: m.gitBranch || null,
      sessionId: m.sessionId || null,
      version: m.version || null,
      count: messages.length,
      turns: t.turns || 0,
      inTok: t.in || 0,
      outTok: t.out || 0,
      cacheTok: t.cacheRead || 0,
      subagentCount: 0,
      firstTs: m.firstTs || null,
      lastTs: m.lastTs || null,
    },
    messages,
    subagents: {},
  };
}

function buildData(file) {
  const recs = parseJsonl(file);
  const codex = require('./codex');
  if (codex.looksCodex(recs)) return buildCodexData(file, recs);
  const s = shapeSession(recs);
  const cwd = s.metaRec.cwd || null;
  const subagents = readSubagents(file);
  require('./history').applySkillNames(s.messages, subagents); // spawning Skill tool_use overrides the sentinel fallback
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

function appVersion() {
  try { return require('../../package.json').version || ''; } catch (_) { return ''; }
}

/** Build the full self-contained HTML document from an already-parsed data object. */
function htmlFromData(data) {
  const skin = readAsset('export-assets/skin.css');
  const runtime = readAsset('export-assets/runtime.js');
  const marked = readAsset(path.join('..', 'renderer', 'vendor', 'marked.umd.js'));
  const hljs = readAsset(path.join('..', 'renderer', 'vendor', 'highlight.min.js'));
  const hljsCss = readAsset(path.join('..', 'renderer', 'vendor', 'hljs-dark.css')); // code blocks are dark in both themes
  const json = JSON.stringify(data).replace(/</g, '\\u003c');
  // Tab title uses the project name (already public via the export's filename), NOT the
  // conversation title: Clarity reports document.title as page metadata that masking can't
  // reach, and the conversation title is first-message text. The full title still renders
  // in the viewer header, inside the Clarity-masked #app.
  const title = (data.meta.project || 'Conversation').replace(/[<>]/g, '');
  // Nonce-based CSP so the standalone exported file (opened in a plain browser, no app CSP) runs
  // ONLY these four generator scripts — an injected <img onerror> / javascript: link carries no
  // nonce and can't execute. The clarity.ms origins additionally allow the Clarity analytics tag
  // the runtime injects. Mirrors exporthtml.rs. (Kept in sync for parity; the shipped export
  // is the Rust path.)
  const csp = "default-src 'none'; script-src 'nonce-ccbudexport' https://www.clarity.ms https://*.clarity.ms; connect-src https://*.clarity.ms https://c.bing.com; style-src 'unsafe-inline'; img-src data:; base-uri 'none'";
  return '<!doctype html><html lang="zh" data-theme="light"><head><meta charset="utf-8">'
    + '<meta http-equiv="Content-Security-Policy" content="' + csp + '">'
    + '<meta name="viewport" content="width=device-width,initial-scale=1">'
    + '<title>' + title + ' · CC Buddy</title>'
    + '<style>' + skin + '\n' + hljsCss + '</style>'
    + '</head><body><div id="app" data-clarity-mask="true"></div>'
    + '<script nonce="ccbudexport">' + marked + '</script>'
    + '<script nonce="ccbudexport">' + hljs + '</script>'
    + '<script nonce="ccbudexport">window.__CONV__=' + json + ';window.__CCBUD_VERSION__=' + JSON.stringify(appVersion()) + ';</script>'
    + '<script nonce="ccbudexport">' + runtime + '</script>'
    + '</body></html>';
}

/** Convenience: parse a session file and build its standalone HTML in one call. */
function buildExportHtml(file) { return htmlFromData(buildData(file)); }

module.exports = { buildExportHtml, buildData, htmlFromData };
