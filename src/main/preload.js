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

  serverStatus: () => ipcRenderer.invoke('server:status'),

  // usage panel
  usageGet: (range) => ipcRenderer.invoke('usage:get', range),
  openMain: () => ipcRenderer.invoke('app:openMain'),
  quitApp: () => ipcRenderer.invoke('app:quit'),

  copy: (t) => ipcRenderer.invoke('util:copy', t),
  openExternal: (u) => ipcRenderer.invoke('util:openExternal', u),

  onLog: (cb) => ipcRenderer.on('gateway:log', (_e, l) => cb(l)),
  onRequest: (cb) => ipcRenderer.on('gateway:request', (_e, r) => cb(r)),
  onStatus: (cb) => ipcRenderer.on('gateway:status', (_e, s) => cb(s)),
  onPopoverShow: (cb) => ipcRenderer.on('popover:show', () => cb()),
});
