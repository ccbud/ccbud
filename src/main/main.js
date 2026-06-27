'use strict';

const { app, BrowserWindow, ipcMain, shell, clipboard, Tray, Menu, nativeImage, dialog } = require('electron');
const fs = require('fs');
const path = require('path');
const { createStore } = require('./store');
const { createGateway } = require('./proxy');
const claude = require('./claude');
const claudeDesktop = require('./claudeDesktop');
const updater = require('./updater');
const os = require('os');
const { formatTokens } = require('./usage');
const { createHistoryWatcher } = require('./history');
const { createInsights } = require('./insights');
const { createMonitorStore } = require('./monitor');
const { DICT } = require('../shared/i18n-dict');

// Main-process translator: reads the chosen language from config, falls back en → key.
function mt(key, params) {
  const lang = (store && store.get().language) || 'en';
  const d = DICT[lang] || DICT.en;
  let s = d[key] != null ? d[key] : (DICT.en[key] != null ? DICT.en[key] : key);
  if (params) s = s.replace(/\{(\w+)\}/g, (_, k) => (params[k] != null ? params[k] : '{' + k + '}'));
  return s;
}
// Map a system locale (app.getLocale()) to the nearest supported UI language.
function mapLocale(loc) {
  loc = String(loc || '').toLowerCase();
  if (loc.startsWith('zh')) return (/-(tw|hk|mo)\b/.test(loc) || loc.includes('hant')) ? 'zh-TW' : 'zh';
  if (loc.startsWith('ja')) return 'ja';
  if (loc.startsWith('ko')) return 'ko';
  return 'en';
}

let mainWindow = null;
let popover = null;
let tray = null;
let store = null;
let gateway = null;
let insights = null;
let history = null;
let monitor = null;
let lastStartError = null;
// Bounded ring buffer of gateway lifecycle/error events, so the monitor's "网关日志" panel can
// backfill on open (the events fire once — e.g. "listening on …" at boot — and aren't replayed
// otherwise). Each entry is stamped with a monotonic seq so the renderer dedupes replay vs live.
const gatewayLogs = [];
let gatewayLogSeq = 0;
const MAX_GATEWAY_LOGS = 80;
let isQuitting = false;
let lastPopoverHide = 0;
let titleTimer = 0;
let historyDirty = new Set();
let historyTimer = null;
let requestLogPath = null;

let requestCountSinceTruncate = 0;
function appendRequestLog(r) {
  if (!requestLogPath) return;
  const agent = r.agentId ? 'sub' : 'main';
  const line = [
    new Date().toISOString(),
    agent,
    r.requestedModel || '-',
    '→',
    r.outgoingModel || '-',
    r.status,
    (r.sessionId || '').slice(0, 8),
  ].join(' ') + '\n';
  fs.appendFile(requestLogPath, line, (err) => {
    if (err) return;
    requestCountSinceTruncate++;
    if (requestCountSinceTruncate >= 50) {
      requestCountSinceTruncate = 0;
      fs.readFile(requestLogPath, 'utf8', (err, data) => {
        if (err) return;
        const lines = data.split('\n');
        if (lines.length > 600) {
          fs.writeFile(requestLogPath, lines.slice(-500).join('\n'), 'utf8', () => {});
        }
      });
    }
  });
}

// Single-instance lock: a second launch must NOT try to bind the same port.
const gotLock = app.requestSingleInstanceLock();
if (!gotLock) {
  app.quit();
} else {
  app.on('second-instance', () => showWindow());
}

function broadcast(channel, payload) {
  if (mainWindow && !mainWindow.isDestroyed()) mainWindow.webContents.send(channel, payload);
}

// Record a gateway log event into the ring buffer (stamped with seq + ts) and broadcast it live.
function pushGatewayLog(l) {
  const entry = Object.assign({ seq: ++gatewayLogSeq, ts: Date.now() }, l);
  gatewayLogs.push(entry);
  while (gatewayLogs.length > MAX_GATEWAY_LOGS) gatewayLogs.shift();
  broadcast('gateway:log', entry);
  return entry;
}

// Coalesce on-disk history change notifications into ~200ms batches before hitting IPC,
// so a burst of file-watch events becomes one renderer refresh.
function markHistoryDirty(files) {
  (files || []).forEach((f) => historyDirty.add(f));
  if (historyTimer) return;
  historyTimer = setTimeout(() => {
    const changed = [...historyDirty];
    historyDirty.clear();
    historyTimer = null;
    broadcast('history:changed', { files: changed });
  }, 200);
}

function currentToken() {
  const c = store.get();
  return c.requireToken && c.gatewayToken ? c.gatewayToken : 'ccbud-local';
}

/* ---------- history / usage directories ---------- */
function expandPath(p) {
  p = String(p || '').trim();
  if (p === '~') return os.homedir();
  if (p.startsWith('~/') || p.startsWith('~\\')) return path.join(os.homedir(), p.slice(2));
  return p;
}
function dirLabel(raw) {
  const home = os.homedir();
  const exp = expandPath(raw);
  if (exp === path.join(home, '.claude')) return '~/.claude';
  if (exp === home + path.sep || exp.startsWith(home + path.sep)) return '~/' + path.relative(home, exp);
  return String(raw);
}
// All configured config dirs → { id(raw path), label, configDir, projectsDir }.
function configDirs() {
  const list = (store && store.get().historyDirs) || ['~/.claude'];
  return list.map((raw) => {
    const exp = expandPath(raw);
    return { id: raw, label: dirLabel(raw), configDir: exp, projectsDir: path.join(exp, 'projects') };
  });
}
// All user settings + data live under ~/.ccbud so they survive an app uninstall/reinstall and are
// trivial to back up or reuse. (The hot-update bundle stays under userData — it's tied to the
// installed shell version, not portable.) Override with CCBUD_HOME for tests / custom locations.
const CCBUD_HOME = process.env.CCBUD_HOME || path.join(os.homedir(), '.ccbud');

// The app-managed import store, laid out exactly like a native config dir's projects/ tree so the
// whole history pipeline (list/group/subagents/getSession/watch/count) handles imports unchanged.
const IMPORTED_ID = '__imported__';
function importsRoot() { return path.join(CCBUD_HOME, 'imports'); }
function importedDir() {
  return { id: IMPORTED_ID, label: mt('conv.imported'), configDir: importsRoot(), projectsDir: path.join(importsRoot(), 'projects'), imported: true };
}
// History reader sees user dirs + the import store; usage (activeProjectsDirs) deliberately does NOT,
// so other people's tokens never pollute your usage stats.
function historyDirsList() { return configDirs().concat([importedDir()]); }
// Active selection ('all' or one dir id) → list of projects dirs for the usage engine.
function activeProjectsDirs() {
  const active = (store && store.get().historyActive) || 'all';
  const all = configDirs();
  const sel = active === 'all' ? all : all.filter((d) => d.id === active);
  return (sel.length ? sel : all).map((d) => d.projectsDir);
}

function statusPayload() {
  const port = store ? store.get().port : null;
  return Object.assign(
    {},
    gateway ? gateway.status() : { running: false, port: null },
    { lastStartError, connected: store ? claude.isConnected(port) : false, claudePath: claude.settingsPath() }
  );
}

function genId() {
  return 'p_' + Date.now().toString(36) + '_' + Math.random().toString(36).slice(2, 8);
}

/* ---------- one-click connect / disconnect ---------- */
async function doConnect() {
  const cfg = store.get();
  if (!cfg.providers.length) return { ok: false, message: mt('err.noProvider') };
  try {
    await gateway.start(cfg.port);
    lastStartError = null;
  } catch (e) {
    lastStartError = mt('err.portFailed', { port: cfg.port, msg: e.message });
    broadcast('gateway:status', statusPayload());
    return { ok: false, message: lastStartError };
  }
  try {
    claude.connect(cfg.port, currentToken(), store);
  } catch (e) {
    return { ok: false, message: mt('err.writeConfig', { msg: e.message }) };
  }
  updateTray();
  broadcast('gateway:status', statusPayload());
  return { ok: true };
}

async function doDisconnect() {
  try {
    claude.disconnect(store);
  } catch (e) {
    return { ok: false, message: mt('err.restoreConfig', { msg: e.message }) };
  }
  await gateway.stop();
  updateTray();
  broadcast('gateway:status', statusPayload());
  return { ok: true };
}

async function restartServer() {
  const cfg = store.get();
  await gateway.stop();
  try {
    await gateway.start(cfg.port);
    lastStartError = null;
  } catch (e) {
    lastStartError = mt('err.portFailed', { port: cfg.port, msg: e.message });
    pushGatewayLog({ level: 'error', msg: lastStartError });
  }
  broadcast('gateway:status', statusPayload());
}

// Raw HTTPS POST that can skip TLS verification (Node fetch can't, and undici isn't a dep).
// Only used for the provider Test button when insecure TLS is enabled; mirrors the proxy's
// `rejectUnauthorized:false` so a self-signed/MITM chain doesn't make Test falsely fail.
function rawPostJson(urlStr, { headers, body, timeoutMs, insecure }) {
  return new Promise((resolve, reject) => {
    let u;
    try { u = new URL(urlStr); } catch (e) { reject(e); return; }
    const lib = u.protocol === 'http:' ? require('http') : require('https');
    const opts = {
      protocol: u.protocol, hostname: u.hostname,
      port: u.port || (u.protocol === 'https:' ? 443 : 80),
      path: u.pathname + u.search, method: 'POST',
      headers: Object.assign({ 'accept-encoding': 'identity' }, headers), // avoid gzip we'd have to decode
    };
    if (insecure && u.protocol === 'https:') opts.rejectUnauthorized = false;
    const r = lib.request(opts, (resp) => {
      const chunks = [];
      resp.on('data', (c) => chunks.push(c));
      resp.on('end', () => resolve({ status: resp.statusCode, text: Buffer.concat(chunks).toString('utf8') }));
    });
    const timer = setTimeout(() => { const e = new Error('timeout'); e.timeout = true; r.destroy(e); }, timeoutMs || 30000);
    if (timer.unref) timer.unref();
    r.on('close', () => clearTimeout(timer));
    r.on('error', reject);
    if (body) r.write(body);
    r.end();
  });
}

async function testProvider(provider) {
  const model = provider.defaultModel || (provider.models && provider.models[0] && provider.models[0].upstream) || '';
  if (!provider.baseUrl) return { ok: false, message: mt('err.baseUrlEmpty') };
  let url;
  try {
    const base = new URL(provider.baseUrl);
    url = base.protocol + '//' + base.host + base.pathname.replace(/\/+$/, '') + '/v1/messages';
  } catch (e) {
    return { ok: false, message: mt('err.baseUrlInvalid') };
  }
  const insecure = !!(store && store.get().insecureSkipVerify);
  const headers = {
    'content-type': 'application/json',
    authorization: 'Bearer ' + (provider.authToken || ''),
    'x-api-key': provider.authToken || '',
    'anthropic-version': '2023-06-01',
  };
  const body = JSON.stringify({ model: model || 'claude-3-5-haiku-20241022', max_tokens: 16, messages: [{ role: 'user', content: 'ping' }] });
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), 30000);
  try {
    let status, text;
    if (insecure) {
      const resp = await rawPostJson(url, { headers, body, timeoutMs: 30000, insecure: true });
      status = resp.status; text = resp.text;
    } else {
      const r = await fetch(url, { method: 'POST', signal: controller.signal, headers, body });
      status = r.status; text = await r.text();
    }
    let json = null;
    try { json = JSON.parse(text); } catch (_) {}
    const httpOk = status >= 200 && status < 300;
    if (httpOk && json && json.type === 'message') return { ok: true, status, model: json.model, message: mt('err.testOk', { model: json.model }) };
    const msg = (json && json.error && json.error.message) || text.slice(0, 200) || `HTTP ${status}`;
    return { ok: false, status, message: msg };
  } catch (e) {
    return { ok: false, message: e.name === 'AbortError' || e.timeout ? mt('err.timeout') : e.message };
  } finally {
    clearTimeout(timer);
  }
}

function registerIpc() {
  ipcMain.handle('config:get', () => store.get());

  ipcMain.handle('config:save', async (_e, next) => {
    const prevPort = store.get().port;
    const nextPort = Number(next && next.port) || prevPort;
    const wasConnected = claude.isConnected(prevPort);

    if (nextPort !== prevPort) {
      // bind the NEW port before committing it, so a bad port never locks the user out
      await gateway.stop();
      try {
        await gateway.start(nextPort);
        lastStartError = null;
      } catch (e) {
        lastStartError = mt('err.portFailed', { port: nextPort, msg: e.message });
        try { await gateway.start(prevPort); } catch (_) {}
        broadcast('gateway:status', statusPayload());
        pushGatewayLog({ level: 'error', msg: lastStartError });
        throw new Error(lastStartError);
      }
    }
    const prevDirs = JSON.stringify(store.get().historyDirs);
    const saved = store.save(next);
    applyOpenAtLogin(saved);
    // keep Claude Code settings in sync if currently connected (port / token changes)
    if (wasConnected) { try { claude.connect(saved.port, currentToken(), store); } catch (_) {} }
    // history dirs changed → re-watch + recompute usage from the new set
    if (JSON.stringify(saved.historyDirs) !== prevDirs) {
      if (history) try { history.refresh(); } catch (_) {}
      if (insights) insights.invalidate();
      broadcast('history:changed', { files: [] });
    }
    updateTray();
    broadcast('gateway:status', statusPayload());
    return saved;
  });

  ipcMain.handle('provider:upsert', async (_e, provider) => {
    const cfg = JSON.parse(JSON.stringify(store.get()));
    if (provider.id) {
      const i = cfg.providers.findIndex((p) => p.id === provider.id);
      if (i >= 0) cfg.providers[i] = provider; else cfg.providers.push(provider);
    } else {
      provider.id = genId();
      cfg.providers.push(provider);
      if (!cfg.activeProviderId) cfg.activeProviderId = provider.id;
    }
    const saved = store.save(cfg);
    updateTray();
    return saved;
  });

  ipcMain.handle('provider:delete', async (_e, id) => {
    const cfg = JSON.parse(JSON.stringify(store.get()));
    cfg.providers = cfg.providers.filter((p) => p.id !== id);
    if (cfg.activeProviderId === id) cfg.activeProviderId = cfg.providers[0] ? cfg.providers[0].id : null;
    const saved = store.save(cfg);
    updateTray();
    return saved;
  });

  ipcMain.handle('provider:setActive', async (_e, id) => {
    const cfg = JSON.parse(JSON.stringify(store.get()));
    cfg.activeProviderId = id;
    const saved = store.save(cfg);
    updateTray();
    broadcast('gateway:status', statusPayload());
    return saved;
  });

  ipcMain.handle('provider:test', async (_e, provider) => testProvider(provider));

  ipcMain.handle('claude:connect', async () => doConnect());
  ipcMain.handle('claude:disconnect', async () => doDisconnect());

  // One-click Claude Desktop ("Third-Party Inference") integration — delivered as a macOS
  // Configuration Profile the user approves once (install) / removes via admin prompt (restore).
  ipcMain.handle('claudeDesktop:status', () => claudeDesktop.status(store.get().port));
  ipcMain.handle('claudeDesktop:connect', async () => {
    const cfg = store.get();
    if (!claudeDesktop.appInstalled()) return { ok: false, reason: 'notInstalled' };
    if (!cfg.providers.length) return { ok: false, reason: 'noProvider' };
    // Claude Desktop must be able to reach the gateway → ensure it's listening first.
    if (!(gateway && gateway.status() && gateway.status().running)) {
      try { await gateway.start(cfg.port); lastStartError = null; }
      catch (e) {
        lastStartError = mt('err.portFailed', { port: cfg.port, msg: e.message });
        broadcast('gateway:status', statusPayload());
        return { ok: false, reason: 'gateway', message: lastStartError };
      }
      updateTray();
      broadcast('gateway:status', statusPayload());
    }
    return claudeDesktop.connect(cfg.port, currentToken());
  });
  ipcMain.handle('claudeDesktop:disconnect', async () => {
    const res = await claudeDesktop.disconnect();
    broadcast('gateway:status', statusPayload());
    return res;
  });

  // Open a past conversation's .jsonl in Claude Desktop for replay/analysis via the official
  // `claude://` deep link: a new Cowork chat with the file attached and a prompt prefilled. The user
  // reviews and presses send. Cowork's `file=` param supports attaching a local absolute path (Claude
  // prompts for permission on first use). Cross-platform (macOS/Windows) — no UI automation needed.
  ipcMain.handle('claudeDesktop:replay', async (_e, file) => {
    if (!file) return { ok: false, reason: 'noFile' };
    if (process.platform === 'darwin' && !claudeDesktop.appInstalled()) return { ok: false, reason: 'notInstalled' };
    const prompt = mt('desktop.replayPrompt').slice(0, 13000); // q is truncated ~14k by Claude
    const url = `claude://cowork/new?q=${encodeURIComponent(prompt)}&file=${encodeURIComponent(file)}`;
    try { await shell.openExternal(url); return { ok: true }; }
    catch (e) { return { ok: false, reason: 'failed', message: e && e.message }; }
  });

  ipcMain.handle('server:status', () => statusPayload());

  ipcMain.handle('usage:get', (_e, range) => (insights ? insights.query(range || '7d') : { range, heatmap: [], byModel: [], byProvider: [] }));

  // Monitor inspector: full captured exchange (headers + bodies) for one forwarded request.
  ipcMain.handle('monitor:get', (_e, id) => (monitor ? monitor.get(id) : null));
  ipcMain.handle('monitor:clear', () => { if (monitor) monitor.clear(); return true; });
  ipcMain.handle('gateway:logs', () => gatewayLogs.slice());
  ipcMain.handle('gateway:logs:clear', () => { gatewayLogs.length = 0; return true; });

  // On-disk conversation history across the configured Claude config dirs.
  ipcMain.handle('history:projects', () => (history ? history.listProjects(store.get().historyActive) : []));
  ipcMain.handle('history:list', () => (history ? history.listSessions(store.get().historyActive) : []));
  ipcMain.handle('history:get', (_e, file) => (history ? history.getSession(file) : null));
  ipcMain.handle('history:dirs', () => ({ dirs: history ? history.dirStats() : [], active: store.get().historyActive }));
  ipcMain.handle('history:pickDir', async () => {
    const win = mainWindow && !mainWindow.isDestroyed() ? mainWindow : null;
    let res;
    try {
      // showHiddenFiles → dot-directories like ~/.claude are visible by default.
      res = await dialog.showOpenDialog(win, {
        title: mt('dialog.pickTitle'),
        message: mt('dialog.pickMessage'),
        defaultPath: os.homedir(),
        buttonLabel: mt('dialog.pickButton'),
        properties: ['openDirectory', 'showHiddenFiles', 'createDirectory'],
      });
    } catch (e) {
      return { canceled: true, error: e && e.message };
    }
    if (res.canceled || !res.filePaths || !res.filePaths.length) return { canceled: true };
    let picked = res.filePaths[0];
    // If the user drilled into the projects/ dir itself, store its parent (the config dir).
    try {
      if (path.basename(picked) === 'projects' && !fs.existsSync(path.join(picked, 'projects'))) {
        picked = path.dirname(picked);
      }
    } catch (_) {}
    return { canceled: false, path: picked };
  });
  ipcMain.handle('history:setActive', (_e, id) => {
    const cfg = JSON.parse(JSON.stringify(store.get()));
    cfg.historyActive = id || 'all';
    const saved = store.save(cfg);
    if (insights) insights.invalidate();
    updateTrayTitle();
    broadcast('history:changed', { files: [], active: saved.historyActive });
    return { active: saved.historyActive };
  });

  // ---- import: copy someone else's .jsonl into the app-managed store (snapshot; the original may
  //      be deleted/moved without consequence). Laid out like a native projects/ tree so the rest of
  //      the pipeline renders it identically; a sidecar .import.json records provenance. ----
  function encodeCwd(cwd) {
    // Mirror Claude Code's lossy dir encoding (decodeDirName is the inverse): '/foo/bar' → '-foo-bar'.
    return cwd ? String(cwd).replace(/[/\\]/g, '-') : '-imported';
  }
  function copyDirSync(src, dst) {
    fs.mkdirSync(dst, { recursive: true });
    for (const e of fs.readdirSync(src, { withFileTypes: true })) {
      const s = path.join(src, e.name), d = path.join(dst, e.name);
      if (e.isDirectory()) copyDirSync(s, d);
      else if (e.isFile()) fs.copyFileSync(s, d);
    }
  }
  function importOne(src, out) {
    let raw;
    try { raw = fs.readFileSync(src, 'utf8'); } catch (e) { out.failed++; return; }
    const recs = [];
    for (const line of raw.split('\n')) { const s = line.trim(); if (!s) continue; try { recs.push(JSON.parse(s)); } catch (_) {} }
    const hasMsg = recs.some((r) => r && (r.type === 'user' || r.type === 'assistant') && r.message);
    if (!hasMsg) { out.failed++; return; } // not a Claude Code transcript
    const metaRec = recs.find((r) => r && r.cwd) || recs.find((r) => r && r.sessionId) || {};
    const baseId = metaRec.sessionId || path.basename(src, '.jsonl');
    const destDir = path.join(importsRoot(), 'projects', encodeCwd(metaRec.cwd));
    const destFile = path.join(destDir, baseId + '.jsonl');
    if (fs.existsSync(destFile)) { out.skipped++; return; } // same session already imported
    try {
      fs.mkdirSync(destDir, { recursive: true });
      fs.copyFileSync(src, destFile);
      // Bring along the session's subagent dialogues if they sit next to the source file.
      const srcSub = path.join(path.dirname(src), path.basename(src, '.jsonl'), 'subagents');
      try { if (fs.statSync(srcSub).isDirectory()) copyDirSync(srcSub, path.join(destDir, baseId, 'subagents')); } catch (_) {}
      fs.writeFileSync(destFile.replace(/\.jsonl$/, '.import.json'),
        JSON.stringify({ originalPath: src, originalName: path.basename(src), sessionId: baseId, importedAt: Date.now() }, null, 2), 'utf8');
      out.imported++;
    } catch (e) { out.failed++; }
  }
  ipcMain.handle('history:import', async () => {
    const win = mainWindow && !mainWindow.isDestroyed() ? mainWindow : null;
    let res;
    try {
      res = await dialog.showOpenDialog(win, {
        title: mt('conv.importTitle'),
        defaultPath: app.getPath('downloads'),
        buttonLabel: mt('conv.importButton'),
        properties: ['openFile', 'multiSelections', 'showHiddenFiles'],
        filters: [{ name: 'JSONL', extensions: ['jsonl'] }],
      });
    } catch (e) { return { canceled: true, error: e && e.message }; }
    if (res.canceled || !res.filePaths || !res.filePaths.length) return { canceled: true };
    const out = { imported: 0, skipped: 0, failed: 0 };
    for (const src of res.filePaths) importOne(src, out);
    broadcast('history:changed', { files: [], active: store.get().historyActive });
    return out;
  });
  // Import by absolute path(s) — drives the drag-and-drop entry point. importOne validates each file
  // is a real Claude Code transcript (has user/assistant message records) before copying it in, so a
  // non-transcript .jsonl just lands in `failed`. Mirrors history:import (which uses a file picker).
  ipcMain.handle('history:importPaths', (_e, paths) => {
    const out = { imported: 0, skipped: 0, failed: 0 };
    for (const src of (Array.isArray(paths) ? paths : [])) {
      if (typeof src === 'string' && /\.jsonl$/i.test(src)) importOne(src, out);
      else out.failed++;
    }
    if (out.imported || out.skipped || out.failed) broadcast('history:changed', { files: [], active: store.get().historyActive });
    return out;
  });
  ipcMain.handle('history:removeImport', async (_e, file) => {
    if (!file) return { ok: false };
    const root = path.resolve(importsRoot());
    const f = path.resolve(file);
    // Hard safety: only ever delete inside our own import store.
    if (f !== root && !f.startsWith(root + path.sep)) return { ok: false, error: 'outside import store' };
    // Confirm via a native message box (localized buttons — window.confirm can't set them to Chinese).
    const win = mainWindow && !mainWindow.isDestroyed() ? mainWindow : null;
    try {
      const r = await dialog.showMessageBox(win, {
        type: 'warning',
        buttons: [mt('modal.cancel'), mt('conv.removeImport')],
        defaultId: 1, cancelId: 0,
        message: mt('conv.removeImportConfirm'),
      });
      if (r.response !== 1) return { ok: false, canceled: true };
    } catch (e) { return { ok: false, error: e && e.message }; }
    try {
      const dir = path.dirname(f), base = path.basename(f, '.jsonl');
      fs.rmSync(f, { force: true });
      fs.rmSync(path.join(dir, base + '.import.json'), { force: true });
      fs.rmSync(path.join(dir, base), { recursive: true, force: true }); // subagents/
    } catch (e) { return { ok: false, error: e && e.message }; }
    broadcast('history:changed', { files: [], active: store.get().historyActive });
    return { ok: true };
  });

  // Set per-conversation customization (custom title + user tags) — persisted as a `__ccbud__`
  // field on the session file's first line. Broadcast so the open list refreshes immediately.
  ipcMain.handle('history:setMeta', (_e, file, patch) => {
    if (!history || !file) return { ok: false };
    const r = history.setCcbud(file, patch || {});
    if (r && r.ok) broadcast('history:changed', { files: [file] });
    return r;
  });

  // Export a conversation: raw .jsonl (verbatim source) or a self-contained .html the
  // renderer assembled (inlined styles + rendered timeline). Both go through a save dialog.
  function saveDialogPath(defName, ext, extLabel) {
    const win = mainWindow && !mainWindow.isDestroyed() ? mainWindow : null;
    return dialog.showSaveDialog(win, {
      title: mt('dialog.exportTitle'),
      defaultPath: path.join(app.getPath('downloads'), defName),
      filters: [{ name: extLabel, extensions: [ext] }],
    });
  }
  // Default export name: <project>-<convStart>-<exportedAt>.<ext>, both timestamps as YYMMDDHHmm
  // (local time). Earlier the JSONL kept the on-disk basename and the HTML used the first user
  // message — both were collision-prone when exporting many conversations from the same project.
  function exportBaseName(file) {
    const fmt = (d) => {
      const p = (n) => String(n).padStart(2, '0');
      return String(d.getFullYear()).slice(-2) + p(d.getMonth() + 1) + p(d.getDate()) + p(d.getHours()) + p(d.getMinutes());
    };
    const sanitize = (s) => String(s || '').replace(/[\/\\:*?"<>|\n\r]+/g, '_').replace(/\s+/g, '_').replace(/^[_.\-]+|[_.\-]+$/g, '').slice(0, 60);
    try {
      const exportHtml = require('./exportHtml');
      const data = exportHtml.buildData(file);
      const project = sanitize(data.meta.project) || 'conversation';
      const convTs = data.meta.firstTs ? new Date(data.meta.firstTs) : null;
      const convPart = convTs && !isNaN(convTs.getTime()) ? fmt(convTs) : 'unknown';
      return project + '-' + convPart + '-' + fmt(new Date());
    } catch (_) {
      return path.basename(file, '.jsonl');
    }
  }
  ipcMain.handle('history:exportRaw', async (_e, file) => {
    if (!file) return { canceled: true, error: 'no file' };
    let data;
    try { data = fs.readFileSync(file, 'utf8'); } catch (e) { return { canceled: true, error: e && e.message }; }
    let res;
    try { res = await saveDialogPath(exportBaseName(file) + '.jsonl', 'jsonl', 'JSONL'); } catch (e) { return { canceled: true, error: e && e.message }; }
    if (res.canceled || !res.filePath) return { canceled: true };
    try { fs.writeFileSync(res.filePath, data, 'utf8'); } catch (e) { return { canceled: true, error: e && e.message }; }
    return { canceled: false, path: res.filePath };
  });
  // Build the standalone Claude-styled viewer (+ embedded subagent dialogues, read from
  // disk here since the renderer never loads them) and save it.
  ipcMain.handle('history:exportHtml', async (_e, file) => {
    if (!file) return { canceled: true, error: 'no file' };
    let html;
    try {
      const exportHtml = require('./exportHtml');
      html = exportHtml.htmlFromData(exportHtml.buildData(file));
    } catch (e) { return { canceled: true, error: e && e.message }; }
    let res;
    try { res = await saveDialogPath(exportBaseName(file) + '.html', 'html', 'HTML'); } catch (e) { return { canceled: true, error: e && e.message }; }
    if (res.canceled || !res.filePath) return { canceled: true };
    try { fs.writeFileSync(res.filePath, html, 'utf8'); } catch (e) { return { canceled: true, error: e && e.message }; }
    // Open the freshly-exported viewer in the user's default browser so they don't have to go
    // hunting for it in the file system (issue #7).
    try { shell.openPath(res.filePath); } catch (_) {}
    return { canceled: false, path: res.filePath };
  });

  ipcMain.handle('app:openMain', () => { showWindow(); return true; });
  ipcMain.handle('app:quit', () => { app.quit(); return true; });
  ipcMain.handle('window:settingsMode', (_e, on) => { setSettingsWindowMode(!!on); return true; });
  // Per-view window minimum width (renderer drives it on view switch): 对话 needs 1300 for its
  // 3-column layout; other views can go down to ~900 so a wide window doesn't leave side gaps.
  ipcMain.handle('window:viewMinWidth', (_e, w) => {
    const win = mainWindow;
    if (!win || win.isDestroyed()) return false;
    try { win.setMinimumSize(Math.max(600, (w | 0) || 900), 600); } catch (_) {}
    return true;
  });

  ipcMain.handle('util:copy', (_e, text) => { clipboard.writeText(String(text || '')); return true; });
  ipcMain.handle('util:openExternal', (_e, url) => {
    try {
      const u = new URL(String(url || ''));
      if (u.protocol === 'https:' || u.protocol === 'http:') { shell.openExternal(u.href); return true; }
    } catch (_) {}
    return false;
  });

  // In-app updates. `update:state` is the cached snapshot (versions + last check); `update:check`
  // hits GitHub; `update:download` stages a hot bundle; `update:apply` relaunches into it.
  ipcMain.handle('update:state', () => updater.publicState());
  ipcMain.handle('update:check', async () => updater.checkForUpdates({ manual: true }));
  ipcMain.handle('update:download', async () => updater.downloadAndStageHot());
  ipcMain.handle('update:apply', () => { updater.relaunchToApply(); return true; });
  ipcMain.handle('update:setAuto', (_e, patch) => {
    const cfg = JSON.parse(JSON.stringify(store.get()));
    cfg.autoUpdate = Object.assign({}, cfg.autoUpdate, patch || {});
    return store.save(cfg).autoUpdate;
  });
}

// Auto-update: on launch (after a short delay) and once a day, check GitHub. When a release
// qualifies for a hot (JS-only) update and autoDownload is on, stage it silently — it applies on
// the next launch (we never force a relaunch). Full (native) updates only surface in the UI.
let updateTimer = 0;
async function runAutoUpdate() {
  try {
    if (!(store.get().autoUpdate || {}).check) return;
    const st = await updater.checkForUpdates({ auto: true });
    if (st && st.ok && st.mode === 'hot' && (store.get().autoUpdate || {}).autoDownload && !(st.pending && st.pending.staged)) {
      const r = await updater.downloadAndStageHot();
      if (r && r.ok) broadcast('update:staged', { version: r.version });
    }
  } catch (_) {}
}
function scheduleAutoUpdate() {
  const kick = () => { runAutoUpdate(); };
  setTimeout(kick, 8000); // give the gateway/tray time to settle first
  updateTimer = setInterval(kick, 24 * 60 * 60 * 1000);
  if (updateTimer && updateTimer.unref) updateTimer.unref();
}

function applyOpenAtLogin(cfg) {
  try { app.setLoginItemSettings({ openAtLogin: !!(cfg && cfg.openAtLogin) }); } catch (_) {}
}

/* ---------- tray ---------- */
function buildTrayMenu() {
  const cfg = store.get();
  const connected = claude.isConnected(cfg.port);
  const ap = cfg.providers.find((p) => p.id === cfg.activeProviderId);
  return Menu.buildFromTemplate([
    { label: connected ? (ap ? mt('tray.connectedWith', { name: ap.name }) : mt('status.connected')) : mt('tray.disconnected'), enabled: false },
    { type: 'separator' },
    { label: mt('tray.openMain'), click: () => showWindow() },
    connected
      ? { label: mt('tray.disconnect'), click: () => doDisconnect() }
      : { label: mt('tray.connect'), click: () => doConnect() },
    { type: 'separator' },
    { label: mt('tray.checkUpdates'), click: () => { showWindow(); setTimeout(() => broadcast('update:openPane'), 250); } },
    { label: mt('tray.quit'), click: () => app.quit() },
  ]);
}
async function updateTrayTitle() {
  if (!tray || process.platform !== 'darwin') return;
  const tu = (store.get().trayUsage) || {};
  if (tu.enabled && insights) {
    try {
      const tokens = await insights.rangeTokens(tu.range || '7d');
      if (tray) {
        tray.setTitle(' ' + formatTokens(tokens));
      }
    } catch (_) {
      if (tray) {
        tray.setTitle('');
      }
    }
  } else {
    tray.setTitle('');
  }
}
function updateTray() {
  updateTrayTitle();
}

/* ---------- tray popover (rich usage panel) ---------- */
function createPopover() {
  popover = new BrowserWindow({
    width: 424,
    height: 344,
    show: false,
    frame: false,
    resizable: false,
    movable: false,
    transparent: true,
    skipTaskbar: true,
    alwaysOnTop: true,
    fullscreenable: false,
    backgroundColor: '#00000000',
    webPreferences: { preload: path.join(__dirname, 'preload.js'), contextIsolation: true, nodeIntegration: false },
  });
  // Show on whatever Space/desktop the user is currently on (and over fullscreen apps), instead of
  // yanking them to the Space where the main window lives when the menu-bar icon is clicked.
  try { popover.setVisibleOnAllWorkspaces(true, { visibleOnFullScreen: true }); } catch (_) {}
  popover.loadFile(path.join(__dirname, '..', 'renderer', 'popover.html'));
  popover.on('blur', () => hidePopover());
}
function positionPopover() {
  const { screen } = require('electron');
  const b = tray.getBounds();
  const wb = popover.getBounds();
  const area = screen.getDisplayMatching(b).workArea;
  let x = Math.round(b.x + b.width / 2 - wb.width / 2);
  x = Math.max(area.x + 4, Math.min(x, area.x + area.width - wb.width - 4));
  const y = process.platform === 'darwin' ? Math.round(b.y + b.height + 2) : Math.round(area.y + 4);
  popover.setPosition(x, y, false);
}
function hidePopover() {
  if (popover && popover.isVisible()) { popover.hide(); lastPopoverHide = Date.now(); }
}
function togglePopover() {
  if (!popover || popover.isDestroyed()) createPopover();
  if (popover.isVisible()) { hidePopover(); return; }
  if (Date.now() - lastPopoverHide < 250) return; // debounce click-after-blur
  positionPopover();
  popover.show();
  popover.webContents.send('popover:show');
}

// macOS Dock visibility follows the main window: shown while a window is open, hidden when it's
// closed so ccbud drops to a menu-bar-only background app (the tray icon stays). Guarded/no-op off
// macOS. The Dock icon *image* is set once at startup; these just toggle its presence.
function showDock() { if (process.platform === 'darwin' && app.dock) { try { app.dock.show(); } catch (_) {} } }
function hideDock() { if (process.platform === 'darwin' && app.dock) { try { app.dock.hide(); } catch (_) {} } }

function showWindow() {
  showDock();
  if (mainWindow && !mainWindow.isDestroyed()) {
    if (mainWindow.isMinimized()) mainWindow.restore();
    mainWindow.show();
    mainWindow.focus();
  } else {
    createWindow();
  }
}

function createWindow() {
  mainWindow = new BrowserWindow({
    width: 1400,
    height: 920,
    // Per-view minimum width: the renderer raises this to 1300 for the 3-column 对话 view and
    // drops it to 900 for the others (window:viewMinWidth). Startup view is 服务 (non-对话) → 900.
    minWidth: 900,
    minHeight: 600,
    titleBarStyle: 'hidden',
    trafficLightPosition: { x: 20, y: 20 },
    vibrancy: 'under-window',
    visualEffectState: 'active',
    backgroundColor: '#00000000',
    title: 'ccbud — Claude Code Gateway',
    webPreferences: { preload: path.join(__dirname, 'preload.js'), contextIsolation: true, nodeIntegration: false },
  });
  mainWindow.loadFile(path.join(__dirname, '..', 'renderer', 'index.html'));
  showDock(); // a visible main window means ccbud should appear in the Dock
  mainWindow.on('closed', () => {
    mainWindow = null;
    // Closing the window (red ×) returns ccbud to the menu bar only — hide the Dock icon. It comes
    // back when the window is reopened (tray → Open Main, or Dock/activate). Skip during quit.
    if (!isQuitting) hideDock();
  });
}

// All views — including Settings — now share ONE freely-resizable window with a unified minimum
// size (set in createWindow). Settings is no longer a fixed-size special case; this stays as a
// no-op so the renderer's settings-mode IPC call remains valid without changing behavior.
function setSettingsWindowMode(_on) { /* no-op: settings no longer locks window size */ }

if (gotLock) {
  app.whenReady().then(async () => {
    const userData = app.getPath('userData');
    // Settings + user data now live under ~/.ccbud (portable across uninstall/reinstall). On first run
    // there, migrate from the legacy locations: the app's userData, and the older "clawdy"-named one —
    // so existing providers/settings/imports survive the move.
    try {
      fs.mkdirSync(CCBUD_HOME, { recursive: true, mode: 0o700 });
      if (!fs.existsSync(path.join(CCBUD_HOME, 'config.json'))) {
        for (const old of [userData, path.join(path.dirname(userData), 'clawdy')]) {
          if (old === CCBUD_HOME || !fs.existsSync(path.join(old, 'config.json'))) continue;
          for (const name of ['config.json', 'requests.log']) {
            const from = path.join(old, name);
            if (fs.existsSync(from)) { try { fs.copyFileSync(from, path.join(CCBUD_HOME, name)); } catch (_) {} }
          }
          const oldImports = path.join(old, 'imports');
          if (fs.existsSync(oldImports)) { try { fs.cpSync(oldImports, path.join(CCBUD_HOME, 'imports'), { recursive: true }); } catch (_) {} }
          break;
        }
      }
    } catch (_) {}
    requestLogPath = path.join(CCBUD_HOME, 'requests.log');
    store = createStore(CCBUD_HOME);
    // First run: pick the UI language from the system locale (then it's user-controlled).
    if (!store.get().language) {
      try { store.save(Object.assign({}, store.get(), { language: mapLocale(app.getLocale()) })); } catch (_) {}
    }
    monitor = createMonitorStore({ max: 30 });
    gateway = createGateway({ getConfig: () => store.get() });
    gateway.on('log', (l) => pushGatewayLog(l));
    gateway.on('request', (r) => {
      appendRequestLog(r);
      broadcast('gateway:request', r);
    });
    // Full request/response capture (bounded, auth-redacted) for the monitor inspector.
    gateway.on('exchange', (ex) => monitor.record(ex));

    // Usage analytics computed from on-disk history (.jsonl) across the active config dirs.
    insights = createInsights({ getDirs: () => activeProjectsDirs() });

    // Watch Claude Code's on-disk session history across ALL configured dirs; the "对话"
    // view reads it directly and live-follows active sessions via the 'changed' broadcast.
    history = createHistoryWatcher({ getDirs: () => historyDirsList() });
    history.on('changed', (p) => {
      const files = (p && p.files) || [];
      files.forEach((f) => insights && insights.invalidate(f));
      markHistoryDirty(files);
      updateTrayTitle();
    });
    try { history.start(); } catch (_) {}

    registerIpc();

    // Hot-update plumbing: confirm this boot succeeded (so a freshly-applied bundle isn't rolled
    // back) and kick off background update checks.
    updater.init({
      userData,
      getConfig: () => store.get(),
      broadcast,
      log: (msg) => pushGatewayLog({ level: 'info', msg: '[update] ' + msg }),
    });
    setTimeout(() => updater.confirmBootSuccess(), 4000);
    scheduleAutoUpdate();

    if (store.get().openAtLogin) applyOpenAtLogin(store.get());

    // If Claude Code is still pointed at us (from a previous session), bring the
    // gateway back up so it keeps working. Otherwise stay idle until the user connects.
    if (claude.isConnected(store.get().port)) {
      try { await gateway.start(store.get().port); lastStartError = null; }
      catch (e) { lastStartError = mt('err.portFailed', { port: store.get().port, msg: e.message }); }
    }

    try {
      let img;
      if (process.platform === 'darwin') {
        img = nativeImage.createFromPath(path.join(__dirname, 'iconTemplate.png')).resize({ width: 18, height: 18 });
        img.setTemplateImage(true);
      } else {
        img = nativeImage.createFromPath(path.join(__dirname, 'icon.png')).resize({ width: 18, height: 18 });
      }
      tray = new Tray(img);
      tray.setToolTip('ccbud — Claude Code Gateway');
      // Wire the handlers right after creating the Tray so a later failure (e.g. popover) can't
      // leave the menu-bar icon inert. The popover is best-effort.
      tray.on('click', () => togglePopover());
      tray.on('right-click', () => tray.popUpContextMenu(buildTrayMenu()));
      try { createPopover(); } catch (_) {}
      updateTrayTitle();
    } catch (_) { /* tray optional */ }

    // refresh the menu-bar token count periodically (range may roll over by day)
    titleTimer = setInterval(() => updateTrayTitle(), 60000);
    if (titleTimer && titleTimer.unref) titleTimer.unref();

    createWindow();
    // Dock-icon click. Use showWindow() (not a getAllWindows().length check): the tray popover is also
    // a BrowserWindow, so after the main window is closed the count is still ≥1 and the old check did
    // nothing — leaving the Dock icon dead until you used the tray menu. showWindow() re-creates/shows it.
    app.on('activate', () => showWindow());

    // macOS: set the Dock icon image once. Dock *visibility* follows the window (showDock/hideDock);
    // done after the tray + window are up so it never interferes with their setup.
    if (process.platform === 'darwin' && app.dock) {
      try {
        const dockImg = nativeImage.createFromPath(path.join(__dirname, 'icon.png'));
        if (dockImg && !dockImg.isEmpty()) app.dock.setIcon(dockImg);
      } catch (_) {}
    }
  });
}

// Keep running in the tray after the window is closed (the gateway must stay up while
// Claude Code is connected). Quit explicitly via the tray menu or Cmd+Q.
app.on('window-all-closed', () => {});

app.on('before-quit', (e) => {
  if (isQuitting || !gateway) return;
  isQuitting = true;
  e.preventDefault();
  if (historyTimer) { clearTimeout(historyTimer); historyTimer = null; }
  try { if (history) history.stop(); } catch (_) {}
  Promise.resolve(gateway.stop()).finally(() => app.exit(0));
});
