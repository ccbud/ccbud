'use strict';

const { app, BrowserWindow, ipcMain, shell, clipboard, Tray, Menu, nativeImage } = require('electron');
const fs = require('fs');
const path = require('path');
const { createStore } = require('./store');
const { createGateway } = require('./proxy');
const claude = require('./claude');
const { createUsageStore, formatTokens } = require('./usage');
const { createHistoryWatcher } = require('./history');
const { createMonitorStore } = require('./monitor');

let mainWindow = null;
let popover = null;
let tray = null;
let store = null;
let gateway = null;
let usage = null;
let history = null;
let monitor = null;
let lastStartError = null;
let isQuitting = false;
let lastPopoverHide = 0;
let titleTimer = 0;
let historyDirty = new Set();
let historyTimer = null;
let requestLogPath = null;

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
  try {
    fs.appendFileSync(requestLogPath, line);
    const buf = fs.readFileSync(requestLogPath, 'utf8');
    const lines = buf.split('\n');
    if (lines.length > 501) fs.writeFileSync(requestLogPath, lines.slice(-500).join('\n'));
  } catch (_) {}
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
  return c.requireToken && c.gatewayToken ? c.gatewayToken : 'clawdy-local';
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
  if (!cfg.providers.length) return { ok: false, message: '请先添加一个服务商再接入' };
  try {
    await gateway.start(cfg.port);
    lastStartError = null;
  } catch (e) {
    lastStartError = `端口 ${cfg.port} 无法启动：${e.message}`;
    broadcast('gateway:status', statusPayload());
    return { ok: false, message: lastStartError };
  }
  try {
    claude.connect(cfg.port, currentToken(), store);
  } catch (e) {
    return { ok: false, message: '写入 Claude Code 配置失败：' + e.message };
  }
  updateTray();
  broadcast('gateway:status', statusPayload());
  return { ok: true };
}

async function doDisconnect() {
  try {
    claude.disconnect(store);
  } catch (e) {
    return { ok: false, message: '恢复 Claude Code 配置失败：' + e.message };
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
    lastStartError = `failed to bind port ${cfg.port}: ${e.message}`;
    broadcast('gateway:log', { level: 'error', msg: lastStartError });
  }
  broadcast('gateway:status', statusPayload());
}

async function testProvider(provider) {
  const model = provider.defaultModel || (provider.models && provider.models[0] && provider.models[0].upstream) || '';
  if (!provider.baseUrl) return { ok: false, message: 'baseUrl is empty' };
  let url;
  try {
    const base = new URL(provider.baseUrl);
    url = base.protocol + '//' + base.host + base.pathname.replace(/\/+$/, '') + '/v1/messages';
  } catch (e) {
    return { ok: false, message: 'invalid baseUrl' };
  }
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), 30000);
  try {
    const r = await fetch(url, {
      method: 'POST',
      signal: controller.signal,
      headers: {
        'content-type': 'application/json',
        authorization: 'Bearer ' + (provider.authToken || ''),
        'x-api-key': provider.authToken || '',
        'anthropic-version': '2023-06-01',
      },
      body: JSON.stringify({ model: model || 'claude-3-5-haiku-20241022', max_tokens: 16, messages: [{ role: 'user', content: 'ping' }] }),
    });
    const text = await r.text();
    let json = null;
    try { json = JSON.parse(text); } catch (_) {}
    if (r.ok && json && json.type === 'message') return { ok: true, status: r.status, model: json.model, message: `连接正常（${json.model}）` };
    const msg = (json && json.error && json.error.message) || text.slice(0, 200) || `HTTP ${r.status}`;
    return { ok: false, status: r.status, message: msg };
  } catch (e) {
    return { ok: false, message: e.name === 'AbortError' ? '超时' : e.message };
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
        lastStartError = `failed to bind port ${nextPort}: ${e.message}`;
        try { await gateway.start(prevPort); } catch (_) {}
        broadcast('gateway:status', statusPayload());
        broadcast('gateway:log', { level: 'error', msg: lastStartError });
        throw new Error(lastStartError);
      }
    }
    const saved = store.save(next);
    applyOpenAtLogin(saved);
    // keep Claude Code settings in sync if currently connected (port / token changes)
    if (wasConnected) { try { claude.connect(saved.port, currentToken(), store); } catch (_) {} }
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

  ipcMain.handle('server:status', () => statusPayload());

  ipcMain.handle('usage:get', (_e, range) => usage.query(range || '7d'));

  // Monitor inspector: full captured exchange (headers + bodies) for one forwarded request.
  ipcMain.handle('monitor:get', (_e, id) => (monitor ? monitor.get(id) : null));
  ipcMain.handle('monitor:clear', () => { if (monitor) monitor.clear(); return true; });

  // On-disk conversation history (~/.claude/projects) — the "对话" view's data source.
  ipcMain.handle('history:projects', () => (history ? history.listProjects() : []));
  ipcMain.handle('history:list', () => (history ? history.listSessions() : []));
  ipcMain.handle('history:get', (_e, file) => (history ? history.getSession(file) : null));

  ipcMain.handle('app:openMain', () => { showWindow(); return true; });
  ipcMain.handle('app:quit', () => { app.quit(); return true; });

  ipcMain.handle('util:copy', (_e, text) => { clipboard.writeText(String(text || '')); return true; });
  ipcMain.handle('util:openExternal', (_e, url) => {
    try {
      const u = new URL(String(url || ''));
      if (u.protocol === 'https:' || u.protocol === 'http:') { shell.openExternal(u.href); return true; }
    } catch (_) {}
    return false;
  });
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
    { label: connected ? `● 已接入${ap ? '：' + ap.name : ''}` : '○ 未接入 Claude Code', enabled: false },
    { type: 'separator' },
    { label: '打开主界面', click: () => showWindow() },
    connected
      ? { label: '断开接入', click: () => doDisconnect() }
      : { label: '一键接入', click: () => doConnect() },
    { type: 'separator' },
    { label: '退出 Clawdy', click: () => app.quit() },
  ]);
}
function updateTrayTitle() {
  if (!tray || process.platform !== 'darwin') return;
  const tu = (store.get().trayUsage) || {};
  if (tu.enabled && usage) {
    tray.setTitle(' ' + formatTokens(usage.rangeTokens(tu.range || '7d')));
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
    width: 380,
    height: 300,
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

function showWindow() {
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
    width: 1120,
    height: 780,
    minWidth: 950,
    minHeight: 620,
    titleBarStyle: 'hidden',
    trafficLightPosition: { x: 20, y: 20 },
    vibrancy: 'under-window',
    visualEffectState: 'active',
    backgroundColor: '#00000000',
    title: 'Clawdy — Claude Code Gateway',
    webPreferences: { preload: path.join(__dirname, 'preload.js'), contextIsolation: true, nodeIntegration: false },
  });
  mainWindow.loadFile(path.join(__dirname, '..', 'renderer', 'index.html'));
  mainWindow.on('closed', () => { mainWindow = null; });
}

if (gotLock) {
  app.whenReady().then(async () => {
    const userData = app.getPath('userData');
    requestLogPath = path.join(userData, 'requests.log');
    store = createStore(userData);
    usage = createUsageStore(userData);
    monitor = createMonitorStore({ max: 30 });
    gateway = createGateway({ getConfig: () => store.get() });
    gateway.on('log', (l) => broadcast('gateway:log', l));
    gateway.on('request', (r) => {
      appendRequestLog(r);
      broadcast('gateway:request', r);
      usage.record(Object.assign({ ts: Date.now() }, r));
      updateTrayTitle();
    });
    // Full request/response capture (bounded, auth-redacted) for the monitor inspector.
    gateway.on('exchange', (ex) => monitor.record(ex));

    // Watch Claude Code's on-disk session history (~/.claude/projects); the "对话" view
    // reads it directly and live-follows active sessions via the 'changed' broadcast.
    history = createHistoryWatcher();
    history.on('changed', (p) => markHistoryDirty(p && p.files));
    try { history.start(); } catch (_) {}

    registerIpc();
    if (store.get().openAtLogin) applyOpenAtLogin(store.get());

    // If Claude Code is still pointed at us (from a previous session), bring the
    // gateway back up so it keeps working. Otherwise stay idle until the user connects.
    if (claude.isConnected(store.get().port)) {
      try { await gateway.start(store.get().port); lastStartError = null; }
      catch (e) { lastStartError = `failed to bind port ${store.get().port}: ${e.message}`; }
    }

    try {
      const img = nativeImage.createFromPath(path.join(__dirname, 'icon.png')).resize({ width: 18, height: 18 });
      tray = new Tray(img);
      tray.setToolTip('Clawdy — Claude Code Gateway');
      createPopover();
      updateTrayTitle();
      tray.on('click', () => togglePopover());
      tray.on('right-click', () => tray.popUpContextMenu(buildTrayMenu()));
    } catch (_) { /* tray optional */ }

    // refresh the menu-bar token count periodically (range may roll over by day)
    titleTimer = setInterval(() => updateTrayTitle(), 60000);
    if (titleTimer && titleTimer.unref) titleTimer.unref();

    createWindow();
    app.on('activate', () => { if (BrowserWindow.getAllWindows().length === 0) createWindow(); });
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
