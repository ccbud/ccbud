'use strict';

const fs = require('fs');
const path = require('path');

function defaultConfig() {
  return {
    port: 8788,
    activeProviderId: null,
    requireToken: false,
    gatewayToken: '',
    openAtLogin: false,
    claudeBackup: null, // snapshot of the user's Claude settings before we connected
    trayUsage: { enabled: false, range: '7d' }, // show token usage in the menu bar
    language: null, // ui language ('en'|'zh'|'zh-TW'|'ja'|'ko'); null = derive from system on first run
    historyDirs: ['~/.claude'], // Claude config dirs to read history/usage from (each has projects/)
    historyActive: 'all',       // which configured dir the conversation/usage views show ('all' or a path)
    // Auto-retry upstream 429s before surfacing them to the client (gives low-concurrency
    // providers a moment to recover instead of failing the request outright).
    retry429: { enabled: true, max: 3, baseMs: 500 },
    // Skip TLS certificate verification on upstream HTTPS requests. Off by default; turn on
    // only when a corporate proxy / self-signed chain breaks otherwise-valid connections.
    insecureSkipVerify: false,
    providers: [],
  };
}

function normalize(cfg) {
  const c = Object.assign(defaultConfig(), cfg || {});
  c.providers = Array.isArray(c.providers) ? c.providers : [];
  c.providers = c.providers.map((p) => {
    const np = {
      id: p.id,
      name: p.name || 'Unnamed',
      baseUrl: p.baseUrl || '',
      authToken: p.authToken || '',
      defaultModel: p.defaultModel || '',
      smallFastModel: p.smallFastModel || '',
      mapDefaultModels: p.mapDefaultModels !== false,
      models: Array.isArray(p.models)
        ? p.models
            .filter((m) => m && (m.alias || m.upstream))
            .map((m) => ({ alias: m.alias || '', upstream: m.upstream || '' }))
        : [],
    };
    // Optional per-provider custom icon — an emoji or a data:/http(s)/assets URL.
    // Preserve it: rebuilding the object without this field is exactly why custom
    // icons silently reverted to the brand/default logo on save.
    if (typeof p.icon === 'string' && p.icon.trim()) np.icon = p.icon.trim();
    return np;
  });
  if (!c.providers.find((p) => p.id === c.activeProviderId)) {
    c.activeProviderId = c.providers.length ? c.providers[0].id : null;
  }
  c.port = Number(c.port) || 8788;
  c.requireToken = !!c.requireToken;
  c.gatewayToken = c.gatewayToken || '';
  c.openAtLogin = !!c.openAtLogin;
  c.claudeBackup = c.claudeBackup || null;
  const tu = c.trayUsage || {};
  c.trayUsage = { enabled: !!tu.enabled, range: ['1d', '7d', '30d', 'all'].includes(tu.range) ? tu.range : '7d' };
  // 429 auto-retry: clamp to sane bounds so a bad config can't wedge the gateway.
  const rr = c.retry429 || {};
  c.retry429 = {
    enabled: rr.enabled !== false,
    max: Number.isFinite(rr.max) && rr.max >= 0 ? Math.min(Math.floor(rr.max), 10) : 3,
    baseMs: Number.isFinite(rr.baseMs) && rr.baseMs >= 0 ? Math.min(Math.floor(rr.baseMs), 10000) : 500,
  };
  c.insecureSkipVerify = !!c.insecureSkipVerify;
  // language: keep null (= "not yet chosen", main.js fills it from the system locale on first run)
  c.language = ['en', 'zh', 'zh-TW', 'ja', 'ko'].includes(c.language) ? c.language : null;
  // History/usage directories: trimmed, trailing-slash-normalized (so '~/.claude' and
  // '~/.claude/' don't both survive as phantom duplicates), unique, non-empty default.
  let dirs = Array.isArray(c.historyDirs)
    ? c.historyDirs.map((d) => String(d || '').trim().replace(/(.)[/\\]+$/, '$1')).filter(Boolean)
    : [];
  dirs = [...new Set(dirs)];
  if (!dirs.includes('~/.claude')) {
    dirs.unshift('~/.claude');
  }
  c.historyDirs = dirs;
  c.historyActive = c.historyActive === 'all' || dirs.includes(c.historyActive) ? c.historyActive : 'all';
  return c;
}

function createStore(dir) {
  const file = path.join(dir, 'config.json');
  let cfg = defaultConfig();

  function load() {
    try {
      cfg = normalize(JSON.parse(fs.readFileSync(file, 'utf8')));
    } catch (_) {
      cfg = defaultConfig();
    }
    return cfg;
  }

  function save(next) {
    const normalized = normalize(next);
    try {
      fs.mkdirSync(dir, { recursive: true, mode: 0o700 });
    } catch (_) {}
    // Atomic: write to a temp file then rename, so a crash mid-write never
    // produces a torn config.json. Only commit to in-memory cfg after success.
    const tmp = file + '.tmp';
    fs.writeFileSync(tmp, JSON.stringify(normalized, null, 2), { mode: 0o600 });
    fs.renameSync(tmp, file);
    // writeFileSync's mode is ignored for an already-existing file; chmod covers that.
    try {
      fs.chmodSync(file, 0o600);
    } catch (_) {}
    cfg = normalized;
    return cfg;
  }

  function get() {
    return cfg;
  }

  load();
  return { get, load, save, file };
}

module.exports = { createStore, defaultConfig, normalize };
