'use strict';

/**
 * Reads Claude Code's on-disk session history (~/.claude/projects/<enc-cwd>/<uuid>.jsonl)
 * and exposes it the way claude-code-history-viewer does — the authoritative data source
 * for Clawdy's "对话" view (no longer reconstructed from gateway traffic).
 *
 * Jobs:
 *  - BROWSE: listProjects()/listSessions() group sessions by project (cwd) and surface
 *    cheap meta (title/model/branch/time); getSession() fully parses one .jsonl into the
 *    rich message model the renderer draws (text/thinking/tool_use/tool_result/image, with
 *    per-turn model + token usage).
 *  - WATCH: fs.watch the projects root and, on change, emit 'changed' { files } so the
 *    renderer can refresh the list and live-follow an open session. Also emits 'correlate'
 *    for each new assistant line (kept for tests / future use).
 *
 * Path overridable via CLAWDY_HISTORY_DIR (tests). Node fs only — no native deps.
 */

const fs = require('fs');
const path = require('path');
const os = require('os');
const { EventEmitter } = require('events');

function projectsDir() {
  return process.env.CLAWDY_HISTORY_DIR || path.join(os.homedir(), '.claude', 'projects');
}

/** Best-effort decode of an encoded project dir name back to a cwd (lossy: '-' was both
 *  the separator and any literal '-' in the path). Only a display fallback — the in-record
 *  `cwd` field is authoritative and always preferred when present. */
function decodeDirName(name) {
  if (!name) return null;
  return '/' + String(name).replace(/^-+/, '').replace(/-/g, '/');
}

function baseName(p) {
  if (!p) return null;
  const parts = String(p).split('/').filter(Boolean);
  return parts.length ? parts[parts.length - 1] : p;
}

/** Normalize an Anthropic `usage` object into our token shape. */
function usageOf(u) {
  if (!u) return null;
  return {
    inputTokens: u.input_tokens || 0,
    outputTokens: u.output_tokens || 0,
    cacheRead: u.cache_read_input_tokens || 0,
    cacheCreation: u.cache_creation_input_tokens || 0,
  };
}

/** Map a raw JSONL record to a {role, content, ...meta} message, or null if not a chat turn. */
function lineToMessage(rec) {
  if (!rec || (rec.type !== 'user' && rec.type !== 'assistant') || !rec.message) return null;
  const m = rec.message;
  if (!m.role) return null;
  const out = {
    role: m.role,
    content: m.content,
    _id: m.id || null,
    _ts: rec.timestamp || null,
    _uuid: rec.uuid || null,
    _parent: rec.parentUuid || null,
    _sidechain: !!rec.isSidechain,
    _meta: !!rec.isMeta,
  };
  if (rec.type === 'assistant') {
    out._model = m.model || null;
    out._usage = usageOf(m.usage);
    out._stopReason = m.stop_reason || null;
  }
  return out;
}

function contentText(content) {
  if (typeof content === 'string') return content;
  if (Array.isArray(content)) return content.filter((b) => b && b.type === 'text').map((b) => b.text || '').join(' ');
  return '';
}

/** First real user message text → session title (skips meta/plumbing + tool-result-only turns). */
function firstUserText(messages) {
  for (const m of messages) {
    if (!m || m.role !== 'user' || m._meta) continue;
    const t = contentText(m.content).trim().replace(/\s+/g, ' ');
    if (!t || t.startsWith('<') || /^(\[Request interrupted|Caveat:)/.test(t)) continue;
    return t.slice(0, 90);
  }
  // fall back to any user text
  const u = messages.find((m) => m && m.role === 'user');
  const t = u ? contentText(u.content).trim().replace(/\s+/g, ' ') : '';
  return t ? t.slice(0, 90) : '(会话)';
}

function parseLines(buf) {
  const out = [];
  for (const line of buf.split('\n')) {
    const s = line.trim();
    if (!s) continue;
    try { out.push(JSON.parse(s)); } catch (_) {}
  }
  return out;
}

function readChunk(file, size, max) {
  try {
    const fd = fs.openSync(file, 'r');
    const len = Math.min(size, max);
    const b = Buffer.alloc(len);
    fs.readSync(fd, b, 0, len, 0);
    fs.closeSync(fd);
    return b.toString('utf8');
  } catch (_) { return ''; }
}

function createHistoryWatcher() {
  const emitter = new EventEmitter();
  const offsets = new Map(); // file -> bytes already tailed
  let watcher = null;
  let debounce = null;
  let started = false;

  function eachSessionFile(cb) {
    const root = projectsDir();
    let dirs;
    try { dirs = fs.readdirSync(root, { withFileTypes: true }); } catch (_) { return; }
    for (const d of dirs) {
      if (!d.isDirectory()) continue;
      const pdir = path.join(root, d.name);
      let files;
      try { files = fs.readdirSync(pdir, { withFileTypes: true }); } catch (_) { continue; }
      for (const f of files) {
        if (f.isFile() && f.name.endsWith('.jsonl')) {
          cb(path.join(pdir, f.name), d.name, false);
        } else if (f.isDirectory() && f.name === 'subagents') {
          const sdir = path.join(pdir, f.name);
          let sfiles;
          try { sfiles = fs.readdirSync(sdir); } catch (_) { continue; }
          for (const sf of sfiles) {
            if (sf.endsWith('.jsonl')) cb(path.join(sdir, sf), d.name, true);
          }
        }
      }
    }
  }

  /* ---------- browse ---------- */
  function sessionMeta(file, dirName, isSub) {
    let st;
    try { st = fs.statSync(file); } catch (_) { return null; }
    const head = readChunk(file, st.size, 131072);
    const recs = parseLines(head);
    // Prefer a record that actually carries cwd — the leading `mode`/`permission-mode`
    // records have sessionId but NO cwd, so matching sessionId first would wrongly fall
    // back to the lossy decodeDirName(). cwd is authoritative; only then sessionId.
    const metaRec = recs.find((r) => r.cwd) || recs.find((r) => r.sessionId) || {};
    const agentRec = recs.find((r) => r.agentId) || {};
    const msgs = recs.map(lineToMessage).filter(Boolean);
    let model = null;
    for (const r of recs) { if (r.type === 'assistant' && r.message && r.message.model) model = r.message.model; }
    const subagent = isSub || !!agentRec.agentId;
    const cwd = metaRec.cwd || decodeDirName(dirName);
    const baseId = metaRec.sessionId || path.basename(file, '.jsonl');
    const sessId = subagent && agentRec.agentId ? `${baseId}-${agentRec.agentId}` : baseId;
    return {
      id: 'disk:' + path.basename(file, '.jsonl') + (subagent ? ':sub' : ''),
      file,
      source: 'disk',
      sessionId: sessId,
      cwd,
      project: baseName(cwd),
      gitBranch: metaRec.gitBranch || null,
      title: (subagent ? '[子代理] ' : '') + firstUserText(msgs),
      model,
      isSubagent: subagent,
      lastActivity: st.mtimeMs,
      sizeKB: Math.round(st.size / 1024),
    };
  }

  function listSessions(limit) {
    const files = [];
    eachSessionFile((file, dirName, isSub) => {
      let st; try { st = fs.statSync(file); } catch (_) { return; }
      files.push({ file, dirName, isSub, mtime: st.mtimeMs });
    });
    files.sort((a, b) => b.mtime - a.mtime);
    const top = files.slice(0, limit || 300);
    return top.map((s) => sessionMeta(s.file, s.dirName, s.isSub)).filter(Boolean);
  }

  /** Sessions grouped into projects (by cwd), each sorted by recency. */
  function listProjects(limit) {
    const sessions = listSessions(limit || 500);
    const groups = new Map();
    for (const s of sessions) {
      const key = s.cwd || '(unknown)';
      if (!groups.has(key)) groups.set(key, { cwd: s.cwd, name: s.project || baseName(key) || '(未知项目)', sessions: [], lastActivity: 0 });
      const g = groups.get(key);
      g.sessions.push(s);
      if (s.lastActivity > g.lastActivity) g.lastActivity = s.lastActivity;
    }
    const arr = [...groups.values()];
    arr.forEach((g) => g.sessions.sort((a, b) => b.lastActivity - a.lastActivity));
    arr.sort((a, b) => b.lastActivity - a.lastActivity);
    return arr;
  }

  function getSession(file) {
    let raw;
    try { raw = fs.readFileSync(file, 'utf8'); } catch (_) { return null; }
    const recs = parseLines(raw);
    // Prefer a record that actually carries cwd — the leading `mode`/`permission-mode`
    // records have sessionId but NO cwd, so matching sessionId first would wrongly fall
    // back to the lossy decodeDirName(). cwd is authoritative; only then sessionId.
    const metaRec = recs.find((r) => r.cwd) || recs.find((r) => r.sessionId) || {};
    const agentRec = recs.find((r) => r.agentId) || {};
    const summaryRec = recs.find((r) => r.type === 'summary' && r.summary);

    const messages = [];
    const totals = { in: 0, out: 0, cacheRead: 0, cacheCreation: 0, turns: 0 };
    let model = null;
    let firstTs = null, lastTs = null;
    for (const r of recs) {
      const lm = lineToMessage(r);
      if (!lm) continue;
      if (lm._meta) continue; // CLI plumbing (command echoes, caveats) — not part of the chat
      if (lm._ts) { if (!firstTs) firstTs = lm._ts; lastTs = lm._ts; }
      const msg = { role: lm.role, content: lm.content };
      if (lm._sidechain) msg.isSidechain = true;
      if (lm._ts) msg.ts = lm._ts;
      if (r.type === 'assistant') {
        if (lm._model) { msg.modelActual = lm._model; model = lm._model; }
        if (lm._usage) msg.usage = lm._usage;
        if (lm._stopReason) msg.stopReason = lm._stopReason;
        const u = lm._usage;
        if (u) {
          totals.in += u.inputTokens; totals.out += u.outputTokens;
          totals.cacheRead += u.cacheRead; totals.cacheCreation += u.cacheCreation;
          totals.turns += 1;
        }
      }
      messages.push(msg);
    }

    const subagent = !!agentRec.agentId;
    const cwd = metaRec.cwd || null;
    const baseId = metaRec.sessionId || path.basename(file, '.jsonl');
    const sessId = subagent && agentRec.agentId ? `${baseId}-${agentRec.agentId}` : baseId;
    return {
      meta: {
        id: 'disk:' + path.basename(file, '.jsonl') + (subagent ? ':sub' : ''),
        file,
        source: 'disk',
        title: (subagent ? '[子代理] ' : '') + firstUserText(messages),
        summary: summaryRec ? summaryRec.summary : null,
        sessionId: sessId,
        cwd,
        project: baseName(cwd),
        gitBranch: metaRec.gitBranch || null,
        version: metaRec.version || null,
        isSubagent: subagent,
        model,
        totals,
        messages: messages.length,
        firstTs,
        lastTs,
      },
      messages,
    };
  }

  /* ---------- watch / live tail ---------- */
  function tailNew() {
    const changed = [];
    eachSessionFile((file) => {
      let st;
      try { st = fs.statSync(file); } catch (_) { return; }
      const prev = offsets.get(file);
      if (prev === undefined) {
        offsets.set(file, st.size);
        if (started) changed.push(file); // a brand-new session appeared while running
        return;
      }
      if (st.size <= prev) { offsets.set(file, st.size); if (st.size < prev) changed.push(file); return; } // rotated/truncated → refresh
      let chunk = '';
      try {
        const fd = fs.openSync(file, 'r');
        const len = st.size - prev;
        const b = Buffer.alloc(len);
        fs.readSync(fd, b, 0, len, prev);
        fs.closeSync(fd);
        chunk = b.toString('utf8');
      } catch (_) { offsets.set(file, st.size); return; }
      offsets.set(file, st.size);
      for (const rec of parseLines(chunk)) {
        if (rec.type === 'assistant' && rec.message && rec.message.id) {
          const sid = rec.sessionId || null;
          const sessId = sid && rec.agentId ? `${sid}-${rec.agentId}` : sid;
          emitter.emit('correlate', { messageId: rec.message.id, sessionId: sessId, cwd: rec.cwd, gitBranch: rec.gitBranch });
        }
        emitter.emit('record', { file, rec });
      }
      changed.push(file);
    });
    if (changed.length) emitter.emit('changed', { files: changed });
  }

  function start() {
    if (started) return;
    started = true;
    const root = projectsDir();
    try { fs.mkdirSync(root, { recursive: true }); } catch (_) {}
    // prime offsets to current sizes (skip historical backlog for the live tail)
    eachSessionFile((file) => {
      try { offsets.set(file, fs.statSync(file).size); } catch (_) {}
    });
    try {
      watcher = fs.watch(root, { recursive: true }, () => {
        clearTimeout(debounce);
        debounce = setTimeout(tailNew, 250);
      });
    } catch (_) {
      // fs.watch recursive unsupported here — fall back to polling
      watcher = setInterval(tailNew, 2000);
      if (watcher.unref) watcher.unref();
    }
  }

  function stop() {
    started = false;
    clearTimeout(debounce); // a pending fs.watch debounce must not run tailNew after stop
    debounce = null;
    try { if (watcher && watcher.close) watcher.close(); else if (watcher) clearInterval(watcher); } catch (_) {}
    watcher = null;
  }

  return {
    on: emitter.on.bind(emitter),
    off: emitter.off.bind(emitter),
    start,
    stop,
    tailNew,
    listSessions,
    listProjects,
    getSession,
  };
}

module.exports = { createHistoryWatcher, lineToMessage, firstUserText, decodeDirName };
