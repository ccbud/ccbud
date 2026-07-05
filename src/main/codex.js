'use strict';

/**
 * Codex CLI session support — reads OpenAI Codex's on-disk rollout logs
 * (`~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`) and normalizes them into the SAME
 * session/message shape the renderer consumes for Claude Code history, so the 对话 view
 * (list / detail / search / live-follow / export) browses both without renderer forks.
 * JS twin of src-tauri/src/codex.rs — keep the two in lockstep.
 *
 * A rollout line is `{timestamp, type, payload}` with type ∈ {session_meta, turn_context,
 * response_item, event_msg, compacted}. Conversation content lives in response_item payloads;
 * event_msg mostly duplicates it (ignored) but its token_count records carry per-turn usage.
 * Very old Codex builds wrote payload objects directly per line (no envelope) — handled by
 * treating such a line as its own payload.
 *
 * Tool calls map onto the tool vocabulary the renderer already draws natively:
 * shell/exec_command/local_shell_call → Bash, update_plan → TodoWrite, view_image → Read,
 * web_search → WebSearch, apply_patch → ApplyPatch (a codex-specific card).
 *
 * Title/tags/soft-delete: Codex files belong to another tool, so per-conversation
 * customization never rewrites them — it lives in a sidecar map at ~/.ccbud/codex-meta.json
 * (shared with the Tauri build), keyed by the rollout file stem.
 */

const fs = require('fs');
const path = require('path');
const os = require('os');

/** Codex's DEFAULT sessions tree (CODEX_HOME-aware, like the codex CLI). Only the auto-add
 *  migration keys off this — browsing walks `<dir>/sessions` of every configured work dir. */
function sessionsRoot() {
  if (process.env.CCBUD_CODEX_DIR) return process.env.CCBUD_CODEX_DIR; // test override
  const ch = (process.env.CODEX_HOME || '').trim();
  return ch ? path.join(ch, 'sessions') : path.join(os.homedir(), '.codex', 'sessions');
}
// The DEFAULT config dir as a history-dir entry string (`~/.codex`), used by the one-time
// startup migration that adds it to historyDirs.
function codexLabel() {
  const dir = path.dirname(sessionsRoot());
  const home = os.homedir();
  if (dir === home) return '~';
  return dir.startsWith(home + path.sep) ? '~' + dir.slice(home.length) : dir;
}
function rootExists() {
  try { return fs.statSync(sessionsRoot()).isDirectory(); } catch (_) { return false; }
}

/** Walk every rollout .jsonl under a sessions tree (date-sharded, walked generically). */
function walkSessions(root, cb) {
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
  walk(root, 0);
}

/**
 * Format sniff on parsed records — routes files that LOOK like Codex rollouts (incl. copies
 * imported into the app store). Claude Code records never use these type tags, and old-format
 * bare Codex items lack Claude's `.message` wrapper.
 */
function looksCodex(recs) {
  return (recs || []).slice(0, 8).some((r) => {
    if (!r || typeof r !== 'object') return false;
    switch (r.type) {
      case 'session_meta': case 'turn_context': case 'event_msg': case 'compacted': return true;
      case 'response_item': return r.payload !== undefined;
      case 'message': case 'function_call': case 'function_call_output':
      case 'reasoning': case 'local_shell_call': return r.message === undefined; // old envelope-less rollout
      default: return r.record_type !== undefined;
    }
  });
}

/** (type, payload, timestamp) of a rollout line, tolerating the old envelope-less format. */
function splitLine(rec) {
  const ts = rec.timestamp || null;
  const t = rec.type || '';
  if (rec.payload !== undefined) return { t, p: rec.payload || {}, ts };
  if (['message', 'function_call', 'function_call_output', 'reasoning', 'local_shell_call',
    'custom_tool_call', 'custom_tool_call_output', 'web_search_call'].includes(t)) {
    return { t: 'response_item', p: rec, ts };
  }
  if (!t && rec.id !== undefined && rec.timestamp !== undefined) return { t: 'session_meta', p: rec, ts };
  return { t, p: rec, ts };
}

// Harness-injected user turns (environment/permissions/instructions wrappers) that aren't
// human prose — hidden from the timeline, exactly like Claude's isMeta records.
function isMetaUserText(t) {
  t = String(t || '').replace(/^\s+/, '');
  return ['<environment_context>', '<user_instructions>', '<permissions', '<ide_', '<turn_context', '<AGENTS', '<workspace_']
    .some((p) => t.startsWith(p));
}

function joinedText(content, kinds) {
  if (typeof content === 'string') return content;
  if (!Array.isArray(content)) return '';
  return content
    .filter((b) => b && kinds.includes(b.type))
    .map((b) => b.text || '')
    .join('\n');
}

/** argv → display command: unwrap the ["bash","-lc", script] convention, else shell-ish join. */
function joinArgv(cmd) {
  if (typeof cmd === 'string') return cmd;
  if (!Array.isArray(cmd)) return '';
  const parts = cmd.map((x) => (typeof x === 'string' ? x : String(x == null ? '' : x)));
  if (parts.length === 3 && ['bash', 'sh', 'zsh', 'dash'].includes(parts[0]) && ['-lc', '-c'].includes(parts[1])) {
    return parts[2];
  }
  return parts.map((p) => (!p || /[\s"']/.test(p) ? JSON.stringify(p) : p)).join(' ');
}

/** Codex tool name + parsed arguments → [renderer tool name, renderer input]. */
function mapTool(name, args) {
  args = args && typeof args === 'object' ? args : {};
  const s = (k) => (typeof args[k] === 'string' ? args[k] : '');
  switch (name) {
    case 'shell': case 'local_shell': case 'container.exec': {
      const input = { command: joinArgv(args.command) };
      const desc = s('justification') || s('workdir');
      if (desc) input.description = desc;
      return ['Bash', input];
    }
    case 'shell_command': return ['Bash', { command: s('command') }];
    case 'exec_command': return ['Bash', { command: s('cmd') || s('command') }];
    case 'apply_patch': return ['ApplyPatch', { patch: s('input') || s('patch') }];
    case 'update_plan':
      return ['TodoWrite', {
        todos: (Array.isArray(args.plan) ? args.plan : []).map((st) => ({
          content: (st && st.step) || '',
          status: (st && st.status) || 'pending',
        })),
      }];
    case 'view_image': return ['Read', { file_path: s('path') }];
    case 'web_search': return ['WebSearch', { query: s('query') }];
    default: return [name, args];
  }
}

/**
 * Tool output payload → { text, err }. Unwraps codex's JSON-wrapped shell output
 * ({"output","metadata":{exit_code}}) and reads exec_command's "exited with code N" header.
 */
function shapeOutput(out) {
  if (out && typeof out === 'object') {
    const text = typeof out.content === 'string' ? out.content : JSON.stringify(out, null, 2);
    return { text, err: out.success === false };
  }
  const s = typeof out === 'string' ? out : '';
  try {
    const v = JSON.parse(s);
    if (v && typeof v === 'object') {
      if (typeof v.output === 'string') {
        const code = (v.metadata && typeof v.metadata.exit_code === 'number') ? v.metadata.exit_code : 0;
        return { text: v.output, err: code !== 0 };
      }
      if (typeof v.content === 'string') return { text: v.content, err: v.success === false };
    }
  } catch (_) {}
  const m = s.slice(0, 240).match(/exited with code (\d+)/);
  if (m) return { text: s, err: m[1] !== '0' };
  return { text: s, err: false };
}

/** data-URL input_image → Claude-style image source block, else null. */
function imageBlock(url) {
  const m = /^data:([^;]+);base64,(.*)$/s.exec(String(url || ''));
  if (!m) return null;
  return { type: 'image', source: { type: 'base64', media_type: m[1], data: m[2] } };
}

/** Normalize parsed rollout records into the renderer's message model. */
function normalize(recs) {
  const messages = [];
  const totals = { in: 0, out: 0, cacheRead: 0, cacheCreation: 0, turns: 0 };
  let model = null, cwd = null, sessionId = null, gitBranch = null, version = null;

  for (const rec of recs || []) {
    if (!rec || typeof rec !== 'object') continue;
    const { t, p, ts } = splitLine(rec);
    const withTs = (m) => { if (ts) m.ts = ts; return m; };
    if (t === 'session_meta') {
      if (!sessionId) sessionId = p.session_id || p.id || null;
      if (!cwd) cwd = p.cwd || null;
      if (!version) version = p.cli_version || null;
      if (!gitBranch) gitBranch = (p.git && p.git.branch) || null;
    } else if (t === 'turn_context') {
      if (p.model) model = p.model;
      if (!cwd) cwd = p.cwd || null;
    } else if (t === 'compacted') {
      const text = String(p.message || '').trim();
      if (text) messages.push(withTs({ role: 'user', content: [{ type: 'text', text }] }));
    } else if (t === 'event_msg') {
      if (p.type === 'token_count') {
        const u = p.info && p.info.last_token_usage;
        if (u) {
          const input = u.input_tokens || 0, cached = u.cached_input_tokens || 0, output = u.output_tokens || 0;
          if (input + cached + output > 0) {
            const usage = {
              inputTokens: Math.max(input - cached, 0),
              outputTokens: output,
              cacheRead: cached,
              cacheCreation: 0,
            };
            totals.in += usage.inputTokens; totals.out += output; totals.cacheRead += cached; totals.turns += 1;
            // Per-turn usage rides the turn's last assistant message (one token_count per turn).
            for (let i = messages.length - 1; i >= 0; i--) {
              if (messages[i].role === 'assistant' && !messages[i].usage) { messages[i].usage = usage; break; }
            }
          }
        }
      } else if (p.type === 'turn_aborted') {
        messages.push(withTs({ role: 'user', content: [{ type: 'text', text: '[Request interrupted by user]' }] }));
      }
    } else if (t === 'response_item') {
      const it = p.type;
      if (it === 'message') {
        const content = p.content;
        if (p.role === 'assistant') {
          const text = joinedText(content, ['output_text', 'text']);
          if (text.trim()) {
            const m = { role: 'assistant', content: [{ type: 'text', text }] };
            if (model) m.modelActual = model;
            messages.push(withTs(m));
          }
        } else if (p.role === 'user') {
          const text = joinedText(content, ['input_text', 'text']);
          if (isMetaUserText(text)) continue;
          const blocks = [];
          if (text.trim()) blocks.push({ type: 'text', text });
          if (Array.isArray(content)) {
            for (const b of content) {
              if (b && b.type === 'input_image') { const img = imageBlock(b.image_url); if (img) blocks.push(img); }
            }
          }
          if (blocks.length) messages.push(withTs({ role: 'user', content: blocks }));
        } // system / developer turns: harness plumbing, not conversation
      } else if (it === 'reasoning') {
        let txt = joinedText(p.summary, ['summary_text', 'text']);
        const extra = joinedText(p.content, ['reasoning_text', 'text']);
        if (extra.trim()) txt = txt.trim() ? txt + '\n\n' + extra : extra;
        if (txt.trim()) {
          const m = { role: 'assistant', content: [{ type: 'thinking', thinking: txt }] };
          if (model) m.modelActual = model;
          messages.push(withTs(m));
        }
      } else if (it === 'function_call') {
        let args = {};
        if (typeof p.arguments === 'string') { try { args = JSON.parse(p.arguments); } catch (_) {} }
        else if (p.arguments && typeof p.arguments === 'object') args = p.arguments;
        const [tname, input] = mapTool(p.name || 'tool', args);
        const m = { role: 'assistant', content: [{ type: 'tool_use', id: p.call_id || p.id || '', name: tname, input }] };
        if (model) m.modelActual = model;
        messages.push(withTs(m));
      } else if (it === 'local_shell_call') {
        const cmd = (p.action && p.action.command) || null;
        const m = { role: 'assistant', content: [{ type: 'tool_use', id: p.call_id || p.id || '', name: 'Bash', input: { command: joinArgv(cmd) } }] };
        if (model) m.modelActual = model;
        messages.push(withTs(m));
      } else if (it === 'custom_tool_call') {
        const inputS = typeof p.input === 'string' ? p.input : '';
        const [tname, input] = p.name === 'apply_patch'
          ? ['ApplyPatch', { patch: inputS }]
          : [p.name || 'tool', { input: inputS }];
        const m = { role: 'assistant', content: [{ type: 'tool_use', id: p.call_id || p.id || '', name: tname, input }] };
        if (model) m.modelActual = model;
        messages.push(withTs(m));
      } else if (it === 'function_call_output' || it === 'custom_tool_call_output') {
        const { text, err } = shapeOutput(p.output);
        const tr = { type: 'tool_result', tool_use_id: p.call_id || '', content: text };
        if (err) tr.is_error = true;
        messages.push(withTs({ role: 'user', content: [tr] }));
      } else if (it === 'web_search_call') {
        const q = (p.action && p.action.query) || '';
        const m = { role: 'assistant', content: [{ type: 'tool_use', id: p.id || p.call_id || '', name: 'WebSearch', input: { query: q } }] };
        if (model) m.modelActual = model;
        messages.push(withTs(m));
      }
    }
  }

  const firstTs = (messages.find((m) => m.ts) || {}).ts || null;
  let lastTs = null;
  for (let i = messages.length - 1; i >= 0; i--) if (messages[i].ts) { lastTs = messages[i].ts; break; }
  return { messages, totals, model, firstTs, lastTs, cwd, sessionId, gitBranch, version };
}

/** (cwd, sessionId) from a codex head — used by the import path to lay out the store copy. */
function headIds(recs) {
  for (const rec of recs || []) {
    if (!rec || typeof rec !== 'object') continue;
    const { t, p } = splitLine(rec);
    if (t === 'session_meta') return { cwd: p.cwd || null, sessionId: p.session_id || p.id || null };
  }
  return { cwd: null, sessionId: null };
}

/* ---------- sidecar customization (~/.ccbud/codex-meta.json): { "<stem>": {title?, tagList?, delete?} } ---------- */

function ccbudHome() { return process.env.CCBUD_HOME || path.join(os.homedir(), '.ccbud'); }
function sidecarPath() { return path.join(ccbudHome(), 'codex-meta.json'); }

let sidecarCache = null; // { mtime, map }
function sidecarMtime() {
  try { return fs.statSync(sidecarPath()).mtimeMs; } catch (_) { return 0; }
}
function readSidecar() {
  const mt = sidecarMtime();
  if (sidecarCache && sidecarCache.mtime === mt) return sidecarCache.map;
  let map = {};
  try {
    const v = JSON.parse(fs.readFileSync(sidecarPath(), 'utf8'));
    if (v && typeof v === 'object' && !Array.isArray(v)) map = v;
  } catch (_) {}
  sidecarCache = { mtime: mt, map };
  return map;
}
function writeSidecar(map) {
  try {
    fs.mkdirSync(ccbudHome(), { recursive: true });
    const tmp = sidecarPath() + '.tmp';
    fs.writeFileSync(tmp, JSON.stringify(map, null, 2), 'utf8');
    fs.renameSync(tmp, sidecarPath());
    sidecarCache = { mtime: sidecarMtime(), map };
    return true;
  } catch (_) { return false; }
}
function stemOf(file) { return path.basename(String(file || ''), '.jsonl'); }
function sidecarMeta(file) {
  const c = readSidecar()[stemOf(file)];
  if (!c || typeof c !== 'object') return { title: null, tags: [], deleted: false };
  return {
    title: typeof c.title === 'string' && c.title.trim() ? c.title.trim() : null,
    tags: Array.isArray(c.tagList) ? c.tagList.filter((t) => typeof t === 'string' && t.trim()).map((t) => t.trim()) : [],
    deleted: c.delete === true,
  };
}
function isDeleted(file) { return sidecarMeta(file).deleted; }

/**
 * setCcbud-equivalent for codex sessions: same patch semantics ({title?, tags?, delete?}),
 * persisted to the sidecar instead of the rollout file (never mutate another tool's data).
 */
function setMeta(file, patch) {
  patch = patch || {};
  const stem = stemOf(file);
  if (!stem) return { ok: false, reason: 'empty' };
  const map = Object.assign({}, readSidecar());
  const next = Object.assign({}, map[stem] || {});
  if ('title' in patch) { const t = String(patch.title || '').trim(); if (t) next.title = t; else delete next.title; }
  if ('tags' in patch) {
    const arr = [];
    for (const x of (patch.tags || [])) { const t = typeof x === 'string' ? x.trim() : ''; if (t && arr.indexOf(t) < 0) arr.push(t); }
    if (arr.length) next.tagList = arr; else delete next.tagList;
  }
  if ('delete' in patch) { if (patch.delete) next.delete = true; else delete next.delete; }
  if (Object.keys(next).length) map[stem] = next; else delete map[stem];
  return writeSidecar(map) ? { ok: true } : { ok: false, reason: 'write' };
}

/** Drop a session's sidecar entry (after its rollout file is deleted forever). */
function removeMeta(file) {
  const stem = stemOf(file);
  const map = Object.assign({}, readSidecar());
  if (stem in map) { delete map[stem]; writeSidecar(map); }
}

/* ---------- list/detail shapes (codex flavors of history.js sessionMeta / getSession) ---------- */

function firstUserText(messages) { return require('./history').firstUserText(messages); }
function baseName(p) {
  if (!p) return null;
  const parts = String(p).split('/').filter(Boolean);
  return parts.length ? parts[parts.length - 1] : p;
}

/**
 * List-row meta from already-parsed head records. `dm` is the dir descriptor
 * ({ id:'__codex__', … } for the live tree, the imported dir for store snapshots).
 */
// In-file __ccbud__ for imported codex COPIES (our own files; history.setCcbud writes it there).
// The Electron readCcbud carries no delete flag (recycle bin is Tauri-side), hence deleted:false.
function fileCcbud(recs) {
  const cc = require('./history').readCcbud(recs);
  return { title: cc.title, tags: cc.tags, deleted: false };
}
// Imported snapshots are marked by their provenance sidecar; live rollouts have none.
function hasImportSidecar(file) {
  try { return fs.statSync(String(file).replace(/\.jsonl$/, '.import.json')).isFile(); } catch (_) { return false; }
}

function sessionMetaFrom(file, recs, dm, st) {
  const n = normalize(recs);
  // Live rollouts customize via the sidecar (never rewrite another tool's files); imported
  // COPIES are our own files, where the in-file __ccbud__ applies.
  const cc = hasImportSidecar(file) ? fileCcbud(recs) : sidecarMeta(file);
  const autoTitle = firstUserText(n.messages);
  const stem = stemOf(file);
  return {
    id: 'codex:' + stem,
    file,
    source: 'codex',
    dirId: dm ? dm.id : null,
    dirLabel: dm ? dm.label : null,
    sessionId: n.sessionId || stem,
    cwd: n.cwd,
    project: baseName(n.cwd),
    gitBranch: n.gitBranch,
    title: cc.title || autoTitle,
    autoTitle,
    tags: cc.tags,
    model: n.model,
    isSubagent: false,
    imported: !!(dm && dm.imported),
    deleted: cc.deleted || false,
    lastActivity: st ? st.mtimeMs : 0,
    sizeKB: st ? Math.round(st.size / 1024) : 0,
  };
}

/** Full-detail shape from already-parsed records (history.getSession routes here). */
function sessionFromRecs(file, recs) {
  const n = normalize(recs);
  let imported = null;
  try { imported = JSON.parse(fs.readFileSync(String(file).replace(/\.jsonl$/, '.import.json'), 'utf8')); } catch (_) {}
  // Same sidecar-vs-in-file split as sessionMetaFrom.
  const cc = imported ? fileCcbud(recs) : sidecarMeta(file);
  const autoTitle = firstUserText(n.messages);
  const stem = stemOf(file);
  return {
    meta: {
      id: 'codex:' + stem,
      file,
      source: 'codex',
      assistant: 'Codex',
      title: cc.title || autoTitle,
      autoTitle,
      tags: cc.tags,
      summary: null,
      sessionId: n.sessionId || stem,
      cwd: n.cwd,
      project: baseName(n.cwd),
      gitBranch: n.gitBranch,
      version: n.version,
      isSubagent: false,
      deleted: cc.deleted || false,
      imported: !!imported,
      importedFrom: imported ? imported.originalPath : null,
      importedAt: imported ? imported.importedAt : null,
      model: n.model,
      totals: n.totals,
      messages: n.messages.length,
      subagentCount: 0,
      firstTs: n.firstTs,
      lastTs: n.lastTs,
    },
    messages: n.messages,
    subagents: {},
  };
}

module.exports = {
  codexLabel,
  sessionsRoot,
  rootExists,
  walkSessions,
  hasImportSidecar,
  looksCodex,
  normalize,
  headIds,
  sidecarMeta,
  isDeleted,
  setMeta,
  removeMeta,
  sessionMetaFrom,
  sessionFromRecs,
};
