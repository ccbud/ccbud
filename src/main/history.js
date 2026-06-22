'use strict';

/**
 * Reads Claude Code's on-disk session history across ONE OR MORE config directories
 * (each a `<configDir>/projects/<enc-cwd>/<uuid>.jsonl` tree). The default config dir is
 * ~/.claude, but Claude Code can run against others (CLAUDE_CONFIG_DIR / --config); the user
 * registers those in settings and ccbud aggregates / switches between them.
 *
 *  - getDirs() supplies [{ id, label, projectsDir }]; the watcher watches ALL of them, while
 *    listSessions/listProjects can be filtered to one (the directory switcher) or 'all'.
 *  - BROWSE: listProjects()/getSession() — project→session tree + rich message model.
 *  - WATCH: fs.watch each projects dir, emit 'changed' { files } on growth so the renderer
 *    live-follows; 'correlate' for each new assistant line (tests / future use).
 *
 * Test override: CCBUD_HISTORY_DIR points the default single dir at a temp projects tree.
 */

const fs = require('fs');
const path = require('path');
const os = require('os');
const { EventEmitter } = require('events');

function defaultDirs() {
  const root = process.env.CCBUD_HISTORY_DIR || path.join(os.homedir(), '.claude', 'projects');
  return [{ id: 'default', label: '~/.claude', projectsDir: root }];
}

/** Best-effort decode of an encoded project dir name → cwd (lossy fallback; record cwd wins). */
function decodeDirName(name) {
  if (!name) return null;
  return '/' + String(name).replace(/^-+/, '').replace(/-/g, '/');
}

function baseName(p) {
  if (!p) return null;
  const parts = String(p).split('/').filter(Boolean);
  return parts.length ? parts[parts.length - 1] : p;
}

function usageOf(u) {
  if (!u) return null;
  return {
    inputTokens: u.input_tokens || 0,
    outputTokens: u.output_tokens || 0,
    cacheRead: u.cache_read_input_tokens || 0,
    cacheCreation: u.cache_creation_input_tokens || 0,
  };
}

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

function firstUserText(messages) {
  for (const m of messages) {
    if (!m || m.role !== 'user' || m._meta) continue;
    const t = contentText(m.content).trim().replace(/\s+/g, ' ');
    if (!t || t.startsWith('<') || /^(\[Request interrupted|Caveat:)/.test(t)) continue;
    return t.slice(0, 90);
  }
  const u = messages.find((m) => m && m.role === 'user');
  const t = u ? contentText(u.content).trim().replace(/\s+/g, ' ') : '';
  return t ? t.slice(0, 90) : ''; // empty → renderer substitutes a localized "(conversation)"
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

function createHistoryWatcher(opts) {
  const getDirs = (opts && opts.getDirs) || defaultDirs;
  const emitter = new EventEmitter();
  const offsets = new Map();   // file -> bytes already tailed
  const watchers = [];         // [{ poll, w }]
  const metaCache = new Map(); // file -> { mtime, size, meta }
  let debounce = null;
  let started = false;

  function dirs() { try { return getDirs() || []; } catch (e) { console.error('[history] getDirs() failed:', (e && e.message) || e); return []; } }

  function eachSessionFile(cb) {
    for (const dm of dirs()) {
      const root = dm && dm.projectsDir;
      if (!root) continue;
      let entries;
      try { entries = fs.readdirSync(root, { withFileTypes: true }); } catch (_) { continue; }
      for (const d of entries) {
        if (!d.isDirectory()) continue;
        const pdir = path.join(root, d.name);
        let files;
        try { files = fs.readdirSync(pdir, { withFileTypes: true }); } catch (_) { continue; }
        for (const f of files) {
          if (f.isFile() && f.name.endsWith('.jsonl')) {
            cb(path.join(pdir, f.name), d.name, false, dm);
          } else if (f.isDirectory() && f.name === 'subagents') {
            const sdir = path.join(pdir, f.name);
            let sfiles;
            try { sfiles = fs.readdirSync(sdir); } catch (_) { continue; }
            for (const sf of sfiles) if (sf.endsWith('.jsonl')) cb(path.join(sdir, sf), d.name, true, dm);
          }
        }
      }
    }
  }

  /* ---------- browse ---------- */
  function sessionMeta(file, dirName, isSub, dm) {
    let st;
    try { st = fs.statSync(file); } catch (_) { return null; }
    let entry = metaCache.get(file);
    if (!entry || entry.mtime !== st.mtimeMs || entry.size !== st.size) {
      const head = readChunk(file, st.size, 131072);
      const recs = parseLines(head);
      const metaRec = recs.find((r) => r.cwd) || recs.find((r) => r.sessionId) || {};
      const agentRec = recs.find((r) => r.agentId) || {};
      const msgs = recs.map(lineToMessage).filter(Boolean);
      let model = null;
      for (const r of recs) { if (r.type === 'assistant' && r.message && r.message.model) model = r.message.model; }
      const subagent = isSub || !!agentRec.agentId;
      const cwd = metaRec.cwd || decodeDirName(dirName);
      const baseId = metaRec.sessionId || path.basename(file, '.jsonl');
      const sessId = subagent && agentRec.agentId ? `${baseId}-${agentRec.agentId}` : baseId;
      entry = {
        mtime: st.mtimeMs,
        size: st.size,
        meta: {
          id: 'disk:' + path.basename(file, '.jsonl') + (subagent ? ':sub' : ''),
          file,
          source: 'disk',
          dirId: dm ? dm.id : 'default',
          dirLabel: dm ? dm.label : null,
          sessionId: sessId,
          cwd,
          project: baseName(cwd),
          gitBranch: metaRec.gitBranch || null,
          title: firstUserText(msgs),
          model,
          isSubagent: subagent,
          lastActivity: st.mtimeMs,
          sizeKB: Math.round(st.size / 1024),
        }
      };
      metaCache.set(file, entry);
    }
    return entry.meta;
  }

  function listSessions(activeId, limit) {
    const files = [];
    const liveFiles = new Set();
    eachSessionFile((file, dirName, isSub, dm) => {
      liveFiles.add(file);
      if (activeId && activeId !== 'all' && dm && dm.id !== activeId) return;
      let st; try { st = fs.statSync(file); } catch (_) { return; }
      files.push({ file, dirName, isSub, dm, mtime: st.mtimeMs });
    });
    for (const f of [...metaCache.keys()]) {
      if (!liveFiles.has(f)) metaCache.delete(f);
    }
    files.sort((a, b) => b.mtime - a.mtime);
    return files.slice(0, limit || 400).map((s) => sessionMeta(s.file, s.dirName, s.isSub, s.dm)).filter(Boolean);
  }

  function listProjects(activeId, limit) {
    const sessions = listSessions(activeId, limit || 600);
    const groups = new Map();
    for (const s of sessions) {
      const key = s.cwd || '(unknown)';
      if (!groups.has(key)) groups.set(key, { cwd: s.cwd, name: s.project || baseName(key) || '', sessions: [], lastActivity: 0 });
      const g = groups.get(key);
      g.sessions.push(s);
      if (s.lastActivity > g.lastActivity) g.lastActivity = s.lastActivity;
    }
    const arr = [...groups.values()];
    arr.forEach((g) => g.sessions.sort((a, b) => b.lastActivity - a.lastActivity));
    arr.sort((a, b) => b.lastActivity - a.lastActivity);
    return arr;
  }

  /** Per-directory session counts (for the settings list + directory switcher). */
  function dirStats() {
    const counts = {};
    eachSessionFile((file, dirName, isSub, dm) => { const id = dm ? dm.id : 'default'; counts[id] = (counts[id] || 0) + 1; });
    return dirs().map((dm) => {
      let exists = false;
      try { exists = fs.statSync(dm.projectsDir).isDirectory(); } catch (_) {}
      return { id: dm.id, label: dm.label, projectsDir: dm.projectsDir, sessions: counts[dm.id] || 0, exists };
    });
  }

  function getSession(file) {
    let raw;
    try { raw = fs.readFileSync(file, 'utf8'); } catch (_) { return null; }
    const recs = parseLines(raw);
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
      if (lm._meta) continue;
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
        title: firstUserText(messages),
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
        if (started) changed.push(file);
        return;
      }
      if (st.size <= prev) { offsets.set(file, st.size); if (st.size < prev) changed.push(file); return; }
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

  function watchDir(root) {
    try {
      const w = fs.watch(root, { recursive: true }, () => { clearTimeout(debounce); debounce = setTimeout(tailNew, 250); });
      return { poll: false, w };
    } catch (_) {
      const iv = setInterval(tailNew, 2000);
      if (iv.unref) iv.unref();
      return { poll: true, w: iv };
    }
  }
  function clearWatchers() {
    for (const x of watchers) { try { if (x.poll) clearInterval(x.w); else x.w.close(); } catch (_) {} }
    watchers.length = 0;
  }
  function primeOffsets() {
    const live = new Set();
    eachSessionFile((file) => { live.add(file); if (!offsets.has(file)) { try { offsets.set(file, fs.statSync(file).size); } catch (_) {} } });
    // drop offsets for files whose directory was removed, so a later re-add re-primes cleanly
    for (const f of [...offsets.keys()]) if (!live.has(f)) offsets.delete(f);
  }
  function syncWatches() {
    clearWatchers();
    for (const dm of dirs()) {
      let exists = false;
      try { exists = fs.statSync(dm.projectsDir).isDirectory(); } catch (_) {}
      if (exists) watchers.push(watchDir(dm.projectsDir));
    }
    primeOffsets();
  }

  function start() {
    if (started) return;
    started = true;
    syncWatches();
  }
  function stop() {
    started = false;
    clearTimeout(debounce);
    debounce = null;
    clearWatchers();
  }
  /** Re-establish watches after the configured directory list changes. */
  function refresh() { if (started) syncWatches(); }

  return {
    on: emitter.on.bind(emitter),
    off: emitter.off.bind(emitter),
    start,
    stop,
    refresh,
    tailNew,
    listSessions,
    listProjects,
    dirStats,
    getSession,
  };
}

module.exports = { createHistoryWatcher, lineToMessage, firstUserText, decodeDirName, defaultDirs };
