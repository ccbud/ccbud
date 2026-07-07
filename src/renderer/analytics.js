'use strict';
/*
 * Usage analytics — Microsoft Clarity (https://clarity.microsoft.com).
 *
 * Loaded FIRST in both windows (index.html + popover.html) so the `clarity` command
 * queue exists before any UI code runs; the real tag is then loaded async through the
 * vendored @microsoft/clarity npm package (vendor/clarity, synced from node_modules
 * by `npm run sync:clarity`).
 *
 * Coverage: window opens, virtual page views (sidebar views, settings panes, popover
 * tabs), every click / right-click / double-click / drag, control changes, first
 * keystroke per field, JS errors + unhandled rejections, and a semantic funnel layer
 * (connect, provider CRUD, exports, updates). Clarity itself records scrolls, mouse
 * traces and dwell time for replay/heatmaps.
 *
 * Privacy: only element identifiers, i18n keys and enum-ish dataset values are ever
 * used as event names — free text, input values and content-bearing dataset payloads
 * (paths, session ids, urls) never leave the app; Clarity's default masking covers
 * replay content, and password fields are never captured. Secrets and user content
 * rendered as page text (export snippet, conversations view, request inspector, …)
 * additionally carry data-clarity-mask="true" in the markup so they stay masked in
 * every Clarity masking mode.
 */
(function () {
  var PROJECT_ID = 'xij8wflxsj';
  var SURFACE = /popover\.html$/.test(location.pathname) ? 'popover' : 'main';
  var MAX = 250;

  // Command-queue stub (same shape the official tag installs) so calls made before
  // the async script lands are replayed once it arrives — and become no-ops that
  // simply keep queueing if the machine is offline.
  var w = window;
  w.clarity = w.clarity || function () { (w.clarity.q = w.clarity.q || []).push(arguments); };

  function track(name) { try { w.clarity('event', String(name).slice(0, MAX)); } catch (_) {} }
  function tag(key, value) {
    try { if (value != null && value !== '') w.clarity('set', key, String(value).slice(0, MAX)); } catch (_) {}
  }
  // Manual hooks for future call sites.
  w.ccTrack = track;
  w.ccTag = tag;

  /* ---------- identity & baseline attributes ---------- */
  var deviceId = '';
  try {
    deviceId = localStorage.getItem('ccbud-device-id') || '';
    if (!deviceId) {
      var buf = new Uint8Array(12);
      crypto.getRandomValues(buf);
      deviceId = 'ccbud-' + Array.prototype.map.call(buf, function (b) { return b.toString(16).padStart(2, '0'); }).join('');
      localStorage.setItem('ccbud-device-id', deviceId);
    }
  } catch (_) {}
  if (deviceId) { try { w.clarity('identify', deviceId); } catch (_) {} }

  tag('surface', SURFACE);
  tag('platform', navigator.platform);
  tag('locale', navigator.language);
  try { tag('theme', localStorage.getItem('ccbud-theme') || 'light'); } catch (_) {}
  try { tag('lang', localStorage.getItem('ccbud-lang') || ''); } catch (_) {}
  try {
    var T = w.__TAURI__;
    if (T && T.app && T.app.getVersion) T.app.getVersion().then(function (v) { tag('appVersion', v); }, function () {});
  } catch (_) {}
  track('open:' + SURFACE);
  track('view:' + (SURFACE === 'popover' ? 'popover/overview' : 'providers'));

  /* ---------- interaction descriptors ---------- */
  // Enum-ish dataset keys whose VALUES are safe to report (fixed UI vocabulary).
  var ENUM_KEYS = ['view', 'settings', 'tab', 'range', 'hrange', 'preset', 'copy', 'export', 'icon'];
  // Content-bearing dataset keys: report the bare key name, never the value.
  var NAME_KEYS = ['proj', 'file', 'id', 'target', 'act', 'tip'];

  function classToken(n) {
    return typeof n.className === 'string' ? n.className.trim().split(/\s+/)[0] : '';
  }
  function descriptor(start) {
    for (var n = start, depth = 0; n && n.nodeType === 1 && depth < 15; n = n.parentElement, depth++) {
      if (n.id) return '#' + n.id;
      var d = n.dataset;
      if (d) {
        for (var i = 0; i < ENUM_KEYS.length; i++) if (d[ENUM_KEYS[i]]) return ENUM_KEYS[i] + '=' + d[ENUM_KEYS[i]];
        if (d.i18n) return 'i18n:' + d.i18n;
        if (d.i18nTitle) return 'i18n:' + d.i18nTitle;
        // Dynamic list items (provider cards, sessions, stream rows): label them by
        // their semantic class token, never by the id/path payload they carry.
        for (var j = 0; j < NAME_KEYS.length; j++) if (d[NAME_KEYS[j]] != null) return classToken(n) || 'data-' + NAME_KEYS[j];
      }
    }
    var el = start && start.closest ? start.closest('button, a, summary, label, [role="button"], input, select') : null;
    var probe = el || start;
    if (probe && probe.nodeType === 1) {
      var cls = classToken(probe);
      return probe.tagName.toLowerCase() + (cls ? '.' + cls : '');
    }
    return 'unknown';
  }

  // Semantic funnel layer: element id → business event (fires alongside the raw click).
  var FUNNEL = {
    btnConnect: 'connect-toggle',
    popConnect: 'connect-toggle',
    btnDesktopConnect: 'desktop-connect',
    btnAdd: 'provider-add',
    btnAddEmpty: 'provider-add',
    btnSave: 'provider-save',
    btnTest: 'provider-test',
    btnUpdateCheck: 'update-check',
    btnUpdateDownload: 'update-download',
    btnUpdateApply: 'update-apply',
    btnUpdateOpen: 'update-open',
    btnUpdateBrew: 'update-brew',
    convImportBtn: 'conv-import',
    convExportBtn: 'conv-export',
    convReplayBtn: 'conv-replay',
    convCopyPathBtn: 'conv-copy-path',
    btnCopyExport: 'copy-export',
    btnGenToken: 'token-generate',
    btnPickHistDir: 'histdir-pick',
    popOpen: 'popover-open-main',
    popQuit: 'app-quit'
  };

  /* ---------- listeners (capture phase, so no UI code can swallow them) ---------- */
  document.addEventListener('click', function (e) {
    var t = e.target && e.target.nodeType === 1 ? e.target : null;
    if (!t) return;
    track('click:' + descriptor(t));

    var host = t.closest ? t.closest('[id]') : null;
    if (host && FUNNEL[host.id]) track('goal:' + FUNNEL[host.id]);
    // Theme flips after the app handler runs — re-read it on the next tick.
    if (host && host.id === 'btnTheme') {
      setTimeout(function () { try { tag('theme', localStorage.getItem('ccbud-theme') || ''); } catch (_) {} }, 0);
    }

    // Virtual page views: sidebar views, settings panes, popover tabs.
    var nav = t.closest ? t.closest('[data-view],[data-settings],[data-tab]') : null;
    if (nav) {
      var d = nav.dataset;
      var view = d.view || (d.settings ? 'settings/' + d.settings : 'popover/' + d.tab);
      track('view:' + view);
      tag('view', view);
    }
    var fmt = t.closest ? t.closest('[data-export]') : null;
    if (fmt) track('goal:conv-export:' + fmt.dataset.export);
  }, true);

  document.addEventListener('contextmenu', function (e) {
    if (e.target && e.target.nodeType === 1) track('rclick:' + descriptor(e.target));
  }, true);
  document.addEventListener('dblclick', function (e) {
    if (e.target && e.target.nodeType === 1) track('dblclick:' + descriptor(e.target));
  }, true);
  document.addEventListener('dragend', function (e) {
    if (e.target && e.target.nodeType === 1) track('drag:' + descriptor(e.target));
  }, true);

  // Committed control changes: checkbox/radio state and enum select values are safe;
  // for anything free-form only the field identity is reported.
  document.addEventListener('change', function (e) {
    var t = e.target;
    if (!t || t.nodeType !== 1) return;
    var name = t.id || t.name || descriptor(t);
    var suffix = '';
    if (t.type === 'checkbox' || t.type === 'radio') suffix = t.checked ? ':on' : ':off';
    else if (t.tagName === 'SELECT') suffix = ':' + String(t.value).slice(0, 32);
    track('change:' + name + suffix);
    if (t.id === 'fLang') tag('lang', t.value);
  }, true);

  // First keystroke per field per window life — signals "user typed here", no content.
  var typed = {};
  document.addEventListener('input', function (e) {
    var t = e.target;
    if (!t || t.nodeType !== 1) return;
    var k = t.id || t.name || t.tagName;
    if (typed[k]) return;
    typed[k] = 1;
    track('input:' + k);
  }, true);

  /* ---------- errors ---------- */
  // Error messages can embed user paths or URLs — redact those before tagging.
  function scrubError(s) {
    return String(s == null ? 'unknown' : s)
      .replace(/(?:file|https?):\/\/[^\s'")]+/gi, '<url>')
      .replace(/(^|[\s'"(=:,])(?:~\/|\/)[^\s'")]+/g, '$1<path>')
      .replace(/[A-Za-z]:\\[^\s'")]+/g, '<path>')
      .slice(0, 120);
  }
  window.addEventListener('error', function (e) {
    if (e && e.target && e.target !== w && e.target.nodeType === 1) {
      track('error:resource:' + (e.target.tagName || '').toLowerCase());
      return;
    }
    track('error:js');
    tag('lastError', scrubError(e && e.message));
    try { w.clarity('upgrade', 'js-error'); } catch (_) {}
  }, true);
  window.addEventListener('unhandledrejection', function (e) {
    var r = e && e.reason;
    track('error:unhandled-rejection');
    tag('lastError', scrubError(r && r.message ? r.message : r));
    try { w.clarity('upgrade', 'js-error'); } catch (_) {}
  });

  /* ---------- window foreground/background ---------- */
  document.addEventListener('visibilitychange', function () {
    track(document.hidden ? 'app:hidden' : 'app:visible');
  });

  /* ---------- boot the tag ---------- */
  function fallbackInject() {
    try {
      if (document.getElementById('clarity-script')) return;
      var s = document.createElement('script');
      s.async = true;
      s.id = 'clarity-script';
      s.src = 'https://www.clarity.ms/tag/' + PROJECT_ID;
      (document.head || document.documentElement).appendChild(s);
    } catch (_) {}
  }
  try {
    import('./vendor/clarity/index.js')
      .then(function (m) { (m.default || m).init(PROJECT_ID); })
      .catch(fallbackInject);
  } catch (_) { fallbackInject(); }
})();
