'use strict';
/*
 * Tauri IPC bridge — exposes the `window.ccbud` API consumed by the renderer on top of
 * Tauri's invoke()/listen(). Loaded before renderer.js (which does `const api = window.ccbud`
 * at line 1).
 *
 * Backend commands are snake_case Tauri commands (see src-tauri/src/lib.rs); event names
 * keep their original "namespace:event" form so the renderer's onX handlers are untouched.
 */
(function () {
  const T = window.__TAURI__;
  if (!T) { console.error('[ccbud] Tauri API not found — window.__TAURI__ missing'); return; }
  const invoke = T.core.invoke;
  const listen = T.event.listen;
  const inv = (cmd, args) => invoke(cmd, args || {});
  const on = (event, cb) => { listen(event, (e) => cb(e.payload)); };
  let droppedPaths = [];

  function fileName(path) {
    return String(path || '').split(/[\\/]/).filter(Boolean).pop() || '';
  }
  function rememberDrop(payload) {
    const paths = Array.isArray(payload && payload.paths) ? payload.paths
      : Array.isArray(payload) ? payload
        : [];
    if (paths.length) droppedPaths = paths.map(String);
  }
  try {
    listen('tauri://drag-drop', (e) => rememberDrop(e.payload));
    listen('tauri://file-drop', (e) => rememberDrop(e.payload));
  } catch (_) {}

  window.ccbud = {
    getConfig: () => inv('config_get'),
    saveConfig: (cfg) => inv('config_save', { cfg }),

    upsertProvider: (p) => inv('provider_upsert', { p }),
    deleteProvider: (id) => inv('provider_delete', { id }),
    setActive: (id) => inv('provider_set_active', { id }),
    testProvider: (p) => inv('provider_test', { p }),

    connect: () => inv('claude_connect'),
    disconnect: () => inv('claude_disconnect'),
    setConnectTarget: (target, on) => inv('set_connect_target', { target, on }),

    desktopStatus: () => inv('desktop_status'),
    desktopConnect: () => inv('desktop_connect'),
    desktopDisconnect: () => inv('desktop_disconnect'),
    desktopReplay: (file, prompt) => inv('desktop_replay', { file, prompt }),

    serverStatus: () => inv('server_status'),
    usageGet: (range) => inv('usage_get', { range }),

    monitorGet: (id) => inv('monitor_get', { id }),
    gatewaySetEnabled: (on) => inv('gateway_set_enabled', { on }),
    monitorClear: () => inv('monitor_clear'),
    logsGet: () => inv('logs_get'),
    logsClear: () => inv('logs_clear'),

    openMain: () => inv('app_open_main'),
    quitApp: () => inv('app_quit'),
    setSettingsMode: (on) => inv('window_settings_mode', { on }),
    setViewMinWidth: (w) => inv('window_view_min_width', { w }),

    historyProjects: () => inv('history_projects'),
    historyList: () => inv('history_list'),
    historyGet: (file) => inv('history_get', { file }),
    historySearch: (query) => inv('history_search', { query }),
    historyDirs: () => inv('history_dirs'),
    historyPickDir: () => inv('history_pick_dir'),
    historySetActive: (id) => inv('history_set_active', { id }),
    historyImport: () => inv('history_import'),
    historyImportPaths: (paths) => inv('history_import_paths', { paths }),
    historyRemoveImport: (file) => inv('history_remove_import', { file }),
    historySetMeta: (file, patch) => inv('history_set_meta', { file, patch }),
    historyDeleteForever: (file) => inv('history_delete_forever', { file }),
    historyExportRaw: (file) => inv('history_export_raw', { file }),
    historyExportHtml: (payload) => inv('history_export_html', { payload }),
    pathForFile: (file) => {
      const name = file && file.name;
      if (!name) return '';
      return droppedPaths.find((p) => fileName(p) === name) || '';
    },
    onHistoryChanged: (cb) => on('history:changed', cb),

    copy: (t) => inv('util_copy', { text: t }),
    openExternal: (u) => inv('util_open_external', { url: u }),

    updateState: () => inv('update_state'),
    updateCheck: () => inv('update_check'),
    updateDownload: () => inv('update_download'),
    updateApply: () => inv('update_apply'),
    updateSetAuto: (patch) => inv('update_set_auto', { patch }),
    onUpdateState: (cb) => on('update:state', cb),
    onUpdateStaged: (cb) => on('update:staged', cb),
    onUpdateOpenPane: (cb) => on('update:openPane', cb),

    onLog: (cb) => on('gateway:log', cb),
    onRequest: (cb) => on('gateway:request', cb),
    onStatus: (cb) => on('gateway:status', cb),
    onPopoverShow: (cb) => on('popover:show', cb),
  };

  // Window dragging: map the existing `.drag-region` elements onto Tauri's `data-tauri-drag-region`
  // (its bundled handler starts a window drag on mousedown over an element carrying that attr;
  // `.no-drag` children like buttons/inputs are untouched since the event target is the child).
  function wireDrag(root) {
    (root || document).querySelectorAll('.drag-region').forEach((el) => {
      el.setAttribute('data-tauri-drag-region', '');
    });
  }
  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', () => wireDrag());
  else wireDrag();
  // Re-apply for any drag bars the renderer injects after first paint (view switches, etc.).
  try {
    const mo = new MutationObserver(() => wireDrag());
    mo.observe(document.documentElement, { childList: true, subtree: true });
  } catch (_) {}
})();
