'use strict';

const { contextBridge, ipcRenderer } = require('electron');

contextBridge.exposeInMainWorld('clawdy', {
  getConfig: () => ipcRenderer.invoke('config:get'),
  saveConfig: (cfg) => ipcRenderer.invoke('config:save', cfg),

  upsertProvider: (p) => ipcRenderer.invoke('provider:upsert', p),
  deleteProvider: (id) => ipcRenderer.invoke('provider:delete', id),
  setActive: (id) => ipcRenderer.invoke('provider:setActive', id),
  testProvider: (p) => ipcRenderer.invoke('provider:test', p),

  // one-click Claude Code integration
  connect: () => ipcRenderer.invoke('claude:connect'),
  disconnect: () => ipcRenderer.invoke('claude:disconnect'),

  // one-click Claude Desktop ("Third-Party Inference") integration
  desktopStatus: () => ipcRenderer.invoke('claudeDesktop:status'),
  desktopConnect: () => ipcRenderer.invoke('claudeDesktop:connect'),
  desktopDisconnect: () => ipcRenderer.invoke('claudeDesktop:disconnect'),

  // Presidio local PII content filter
  presidioStatus: () => ipcRenderer.invoke('presidio:status'),
  presidioSetup: () => ipcRenderer.invoke('presidio:setup'),
  presidioEnable: (on) => ipcRenderer.invoke('presidio:enable', on),
  presidioLogs: () => ipcRenderer.invoke('presidio:logs'),
  presidioLogsClear: () => ipcRenderer.invoke('presidio:logs:clear'),
  onPresidioLog: (cb) => ipcRenderer.on('presidio:log', (_e, l) => cb(l)),
  presidioFindings: () => ipcRenderer.invoke('presidio:findings'),
  presidioFindingsClear: () => ipcRenderer.invoke('presidio:findings:clear'),
  onPresidioFinding: (cb) => ipcRenderer.on('presidio:finding', (_e, f) => cb(f)),

  serverStatus: () => ipcRenderer.invoke('server:status'),

  // usage panel
  usageGet: (range) => ipcRenderer.invoke('usage:get', range),

  // monitor inspector — full captured request/response for a forwarded request
  monitorGet: (id) => ipcRenderer.invoke('monitor:get', id),
  monitorClear: () => ipcRenderer.invoke('monitor:clear'),

  // gateway log buffer — backfill the "网关日志" panel on open (events aren't replayed otherwise)
  logsGet: () => ipcRenderer.invoke('gateway:logs'),
  logsClear: () => ipcRenderer.invoke('gateway:logs:clear'),
  openMain: () => ipcRenderer.invoke('app:openMain'),
  quitApp: () => ipcRenderer.invoke('app:quit'),
  setSettingsMode: (on) => ipcRenderer.invoke('window:settingsMode', on),

  // conversation history (reads the configured Claude config dirs' projects/*.jsonl)
  historyProjects: () => ipcRenderer.invoke('history:projects'),
  historyList: () => ipcRenderer.invoke('history:list'),
  historyGet: (file) => ipcRenderer.invoke('history:get', file),
  historyDirs: () => ipcRenderer.invoke('history:dirs'),
  historyPickDir: () => ipcRenderer.invoke('history:pickDir'),
  historySetActive: (id) => ipcRenderer.invoke('history:setActive', id),
  onHistoryChanged: (cb) => ipcRenderer.on('history:changed', (_e, p) => cb(p)),

  copy: (t) => ipcRenderer.invoke('util:copy', t),
  openExternal: (u) => ipcRenderer.invoke('util:openExternal', u),

  onLog: (cb) => ipcRenderer.on('gateway:log', (_e, l) => cb(l)),
  onRequest: (cb) => ipcRenderer.on('gateway:request', (_e, r) => cb(r)),
  onStatus: (cb) => ipcRenderer.on('gateway:status', (_e, s) => cb(s)),
  onPopoverShow: (cb) => ipcRenderer.on('popover:show', () => cb()),
});
