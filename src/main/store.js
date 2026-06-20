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
    providers: [],
  };
}

function normalize(cfg) {
  const c = Object.assign(defaultConfig(), cfg || {});
  c.providers = Array.isArray(c.providers) ? c.providers : [];
  c.providers = c.providers.map((p) => ({
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
  }));
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
