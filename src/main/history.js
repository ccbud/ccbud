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
  let fallbackCmd = ''; // first slash-command label, used only if no prose turn exists
  for (const m of messages) {
    if (!m || m.role !== 'user' || m._meta) continue;
    const raw = contentText(m.content).trim();
    if (!raw) continue;
    if (raw.startsWith('<')) { if (!fallbackCmd) fallbackCmd = commandLabel(raw); continue; }
    const t = raw.replace(/\s+/g, ' ');
    if (/^(\[Request interrupted|Caveat:)/.test(t)) continue;
    return t.slice(0, 90);
  }
  // No prose turn: prefer a parsed "/cmd" label over dumping raw XML; empty → renderer
  // substitutes a localized "(conversation)".
  return fallbackCmd.slice(0, 90);
}

// Optional per-conversation customization the app writes onto a session line as `__ccbud__`
// (custom title + user tags). It's an extra field on a meta line — invisible to Claude Code itself.
// See setCcbud for the writer. Returns { title|null, tags[] }.
function readCcbud(recs) {
  const r = recs.find((x) => x && typeof x === 'object' && x.__ccbud__);
  const c = r ? r.__ccbud__ : null;
  return {
    title: c && typeof c.title === 'string' && c.title.trim() ? c.title.trim() : null,
    tags: c && Array.isArray(c.tagList)
      ? c.tagList.filter((t) => typeof t === 'string' && t.trim()).map((t) => t.trim())
      : [],
  };
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

// Shape parsed records into the renderer's message model (+ rollup totals / model / span). Shared
// by getSession and the subagent reader so a subagent's timeline renders identically to the main one.
function shapeMessages(recs) {
  const messages = [];
  const totals = { in: 0, out: 0, cacheRead: 0, cacheCreation: 0, turns: 0 };
  let model = null, firstTs = null, lastTs = null;
  for (const r of recs) {
    const lm = lineToMessage(r);
    if (!lm || lm._meta) continue;
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
  return { messages, totals, model, firstTs, lastTs };
}

// Read a session's subagent dialogues — <sessionFile-dir>/<sessionId>/subagents/agent-<id>.{jsonl,meta.json}
// — keyed by the spawning Task/Agent tool_use id (agent-<id>.meta.json's toolUseId), so the "对话"
// view can nest each subagent's timeline under the call that spawned it. Mirrors the HTML export.
// Returns {} when the session has no subagents directory.
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
    let raw;
    try { raw = fs.readFileSync(path.join(dir, name), 'utf8'); } catch (_) { continue; }
    const shaped = shapeMessages(parseLines(raw));
    const key = meta.toolUseId || ('agent:' + agentId);
    byTool[key] = {
      agentId,
      file: path.join(dir, name), // absolute path to this subagent's .jsonl (for "copy path")
      type: meta.agentType || meta.subagent_type || 'agent',
      description: meta.description || '',
      count: shaped.messages.length,
      totals: shaped.totals,
      messages: shaped.messages,
    };
  }
  return byTool;
}

// Absolute path to a session's subagents dir (`<dir>/<stem>/subagents`), regardless of existence.
function subagentDir(file) {
  return path.join(path.dirname(file), path.basename(file, '.jsonl'), 'subagents');
}

// A session's raw subagent sidecar files (`agent-*.jsonl` + `agent-*.meta.json`) as
// [{ name, data:Buffer }], sorted by name. Empty when the session has no subagents. Shared by
// bundle export/import and the replay merge (mirrors src-tauri/src/history.rs read_subagent_files).
function readSubagentFiles(file) {
  const dir = subagentDir(file);
  let names;
  try { names = fs.readdirSync(dir); } catch (_) { return []; }
  const out = [];
  for (const name of names) {
    if (!/^agent-.*\.jsonl$/i.test(name) && !/^agent-.*\.meta\.json$/i.test(name)) continue;
    try {
      const p = path.join(dir, name);
      if (!fs.statSync(p).isFile()) continue;
      out.push({ name, data: fs.readFileSync(p) });
    } catch (_) {}
  }
  out.sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
  return out;
}

// Merge a session's transcript with its subagent transcripts into one .jsonl buffer (main lines
// first, then each subagent's — which already carry isSidechain/agentId, so a reader can tell them
// apart). null when the session has no subagents (caller uses the file as-is). Powers "Claude 分析"
// so the analysis covers subagent runs, not just the main thread.
function mergedTranscript(file) {
  const subs = readSubagentFiles(file).filter((s) => /\.jsonl$/i.test(s.name));
  if (!subs.length) return null;
  let main;
  try { main = fs.readFileSync(file); } catch (_) { return null; }
  const chunks = [main];
  for (const s of subs) {
    const prev = chunks[chunks.length - 1];
    if (prev.length && prev[prev.length - 1] !== 0x0a) chunks.push(Buffer.from('\n'));
    chunks.push(s.data);
  }
  return Buffer.concat(chunks);
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
      const cc = readCcbud(recs);
      const autoTitle = firstUserText(msgs);
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
          title: cc.title || autoTitle,
          autoTitle,
          tags: cc.tags,
          model,
          isSubagent: subagent,
          imported: !!(dm && dm.imported),
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
      return { id: dm.id, label: dm.label, projectsDir: dm.projectsDir, sessions: counts[dm.id] || 0, exists, imported: !!dm.imported };
    });
  }

  function getSession(file) {
    let raw;
    try { raw = fs.readFileSync(file, 'utf8'); } catch (_) { return null; }
    const recs = parseLines(raw);
    const metaRec = recs.find((r) => r.cwd) || recs.find((r) => r.sessionId) || {};
    const agentRec = recs.find((r) => r.agentId) || {};
    const summaryRec = recs.find((r) => r.type === 'summary' && r.summary);
    const cc = readCcbud(recs);

    const shaped = shapeMessages(recs);
    const messages = shaped.messages;
    const autoTitle = firstUserText(messages);

    const subagent = !!agentRec.agentId;
    // Imported transcripts carry a sidecar recording where they came from (see main.importOne).
    let imported = null;
    try { imported = JSON.parse(fs.readFileSync(file.replace(/\.jsonl$/, '.import.json'), 'utf8')); } catch (_) {}
    // Only a top-level session embeds its child subagent dialogues (a subagent file has no nested
    // subagents/ dir of its own), so the renderer can nest them under their spawning Task call.
    const subagents = subagent ? {} : readSubagents(file);
    const cwd = metaRec.cwd || null;
    const baseId = metaRec.sessionId || path.basename(file, '.jsonl');
    const sessId = subagent && agentRec.agentId ? `${baseId}-${agentRec.agentId}` : baseId;
    return {
      meta: {
        id: 'disk:' + path.basename(file, '.jsonl') + (subagent ? ':sub' : ''),
        file,
        source: 'disk',
        title: cc.title || autoTitle,
        autoTitle,
        tags: cc.tags,
        summary: summaryRec ? summaryRec.summary : null,
        sessionId: sessId,
        cwd,
        project: baseName(cwd),
        gitBranch: metaRec.gitBranch || null,
        version: metaRec.version || null,
        isSubagent: subagent,
        imported: !!imported,
        importedFrom: imported ? imported.originalPath : null,
        importedAt: imported ? imported.importedAt : null,
        model: shaped.model,
        totals: shaped.totals,
        messages: messages.length,
        subagentCount: Object.keys(subagents).length,
        firstTs: shaped.firstTs,
        lastTs: shaped.lastTs,
      },
      messages,
      subagents,
    };
  }

  // Write per-conversation customization (custom title + tags) onto the FIRST parseable line of a
  // session file as a `__ccbud__` field. patch: { title?, tags? } — empty title / empty tags removes
  // that key (empty __ccbud__ is dropped entirely). Atomic (tmp + rename, mirrors store.js). Guarded
  // to the configured projects dirs so a renderer can never drive an arbitrary-path write.
  function setCcbud(file, patch) {
    patch = patch || {};
    const within = dirs().some((dm) => dm && dm.projectsDir &&
      path.resolve(file).startsWith(path.resolve(dm.projectsDir) + path.sep));
    if (!within) return { ok: false, reason: 'out-of-scope' };
    let raw;
    try { raw = fs.readFileSync(file, 'utf8'); } catch (_) { return { ok: false, reason: 'read' }; }
    const lines = raw.split('\n');
    let idx = -1, obj = null;
    for (let i = 0; i < lines.length; i++) {
      const s = lines[i].trim(); if (!s) continue;
      try { obj = JSON.parse(s); idx = i; break; } catch (_) {}
    }
    if (idx < 0 || !obj || typeof obj !== 'object') return { ok: false, reason: 'empty' };
    const next = Object.assign({}, obj.__ccbud__ || {});
    if ('title' in patch) { const t = (patch.title || '').trim(); if (t) next.title = t; else delete next.title; }
    if ('tags' in patch) {
      const arr = [];
      for (const x of (patch.tags || [])) { const t = typeof x === 'string' ? x.trim() : ''; if (t && arr.indexOf(t) < 0) arr.push(t); }
      if (arr.length) next.tagList = arr; else delete next.tagList;
    }
    if (Object.keys(next).length) obj.__ccbud__ = next; else delete obj.__ccbud__;
    lines[idx] = JSON.stringify(obj);
    const out = lines.join('\n');
    const tmp = file + '.ccbud.tmp';
    try { fs.writeFileSync(tmp, out, 'utf8'); fs.renameSync(tmp, file); }
    catch (e) { try { fs.unlinkSync(tmp); } catch (_) {} return { ok: false, reason: 'write' }; }
    metaCache.delete(file);                                // next list re-reads the new title/tags
    try { offsets.set(file, Buffer.byteLength(out)); } catch (_) {}  // don't let the watcher replay the rewrite as new records
    return { ok: true };
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
    setCcbud,
  };
}

module.exports = { createHistoryWatcher, lineToMessage, firstUserText, decodeDirName, defaultDirs, subagentDir, readSubagentFiles, mergedTranscript };
