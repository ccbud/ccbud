'use strict';

const { contextBridge, ipcRenderer, webUtils } = require('electron');

contextBridge.exposeInMainWorld('ccbud', {
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
  desktopReplay: (file) => ipcRenderer.invoke('claudeDesktop:replay', file),

  serverStatus: () => ipcRenderer.invoke('server:status'),

  // usage panel
  usageGet: (range) => ipcRenderer.invoke('usage:get', range),

  // monitor inspector — full captured request/response for a forwarded request
  monitorGet: (id) => ipcRenderer.invoke('monitor:get', id),
  gatewaySetEnabled: (on) => ipcRenderer.invoke('gateway:setEnabled', on),
  monitorClear: () => ipcRenderer.invoke('monitor:clear'),

  // gateway log buffer — backfill the "网关日志" panel on open (events aren't replayed otherwise)
  logsGet: () => ipcRenderer.invoke('gateway:logs'),
  logsClear: () => ipcRenderer.invoke('gateway:logs:clear'),
  openMain: () => ipcRenderer.invoke('app:openMain'),
  quitApp: () => ipcRenderer.invoke('app:quit'),
  setSettingsMode: (on) => ipcRenderer.invoke('window:settingsMode', on),
  setViewMinWidth: (w) => ipcRenderer.invoke('window:viewMinWidth', w),

  // conversation history (reads the configured Claude config dirs' projects/*.jsonl)
  historyProjects: () => ipcRenderer.invoke('history:projects'),
  historyList: () => ipcRenderer.invoke('history:list'),
  historyGet: (file) => ipcRenderer.invoke('history:get', file),
  historyDirs: () => ipcRenderer.invoke('history:dirs'),
  historyPickDir: () => ipcRenderer.invoke('history:pickDir'),
  historySetActive: (id) => ipcRenderer.invoke('history:setActive', id),
  historyImport: () => ipcRenderer.invoke('history:import'),
  historyImportPaths: (paths) => ipcRenderer.invoke('history:importPaths', paths),
  // Resolve a dragged File to its absolute path (Electron 32+ removed File.path → use webUtils).
  pathForFile: (file) => { try { return webUtils.getPathForFile(file); } catch (_) { return (file && file.path) || ''; } },
  historyRemoveImport: (file) => ipcRenderer.invoke('history:removeImport', file),
  historySetMeta: (file, patch) => ipcRenderer.invoke('history:setMeta', file, patch),
  historyExportRaw: (file) => ipcRenderer.invoke('history:exportRaw', file),
  historyExportHtml: (payload) => ipcRenderer.invoke('history:exportHtml', payload),
  onHistoryChanged: (cb) => ipcRenderer.on('history:changed', (_e, p) => cb(p)),

  copy: (t) => ipcRenderer.invoke('util:copy', t),
  openExternal: (u) => ipcRenderer.invoke('util:openExternal', u),

  // in-app updates
  updateState: () => ipcRenderer.invoke('update:state'),
  updateCheck: () => ipcRenderer.invoke('update:check'),
  updateDownload: () => ipcRenderer.invoke('update:download'),
  updateApply: () => ipcRenderer.invoke('update:apply'),
  updateSetAuto: (patch) => ipcRenderer.invoke('update:setAuto', patch),
  onUpdateState: (cb) => ipcRenderer.on('update:state', (_e, s) => cb(s)),
  onUpdateStaged: (cb) => ipcRenderer.on('update:staged', (_e, s) => cb(s)),
  onUpdateOpenPane: (cb) => ipcRenderer.on('update:openPane', () => cb()),

  onLog: (cb) => ipcRenderer.on('gateway:log', (_e, l) => cb(l)),
  onRequest: (cb) => ipcRenderer.on('gateway:request', (_e, r) => cb(r)),
  onStatus: (cb) => ipcRenderer.on('gateway:status', (_e, s) => cb(s)),
  onPopoverShow: (cb) => ipcRenderer.on('popover:show', () => cb()),
});
