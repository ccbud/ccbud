'use strict';

/**
 * One-click integration with Claude Code's user settings (~/.claude/settings.json).
 *
 * Connect:   point Claude Code at the local gateway by writing env.ANTHROPIC_BASE_URL /
 *            env.ANTHROPIC_AUTH_TOKEN, and CLEAR any model-name overrides so Claude Code
 *            sends its native claude-* names — the gateway then auto-maps them to whichever
 *            provider is active. The user's original values are backed up first.
 * Disconnect: restore the exact prior state from the backup.
 *
 * The settings path is overridable via CLAWDY_CLAUDE_SETTINGS (used by tests so the real
 * user config is never touched).
 */

const fs = require('fs');
const path = require('path');
const os = require('os');

// Model-selection keys we clear while connected (so the gateway controls routing).
const MODEL_ENV_KEYS = [
  'ANTHROPIC_MODEL',
  'ANTHROPIC_SMALL_FAST_MODEL',
  'ANTHROPIC_DEFAULT_HAIKU_MODEL',
  'ANTHROPIC_DEFAULT_SONNET_MODEL',
  'ANTHROPIC_DEFAULT_OPUS_MODEL',
];
const ALL_BACKUP_KEYS = ['ANTHROPIC_BASE_URL', 'ANTHROPIC_AUTH_TOKEN', ...MODEL_ENV_KEYS];

function settingsPath() {
  return process.env.CLAWDY_CLAUDE_SETTINGS || path.join(os.homedir(), '.claude', 'settings.json');
}

function readSettings() {
  try {
    const raw = fs.readFileSync(settingsPath(), 'utf8');
    const obj = JSON.parse(raw);
    return obj && typeof obj === 'object' ? obj : {};
  } catch (_) {
    return {};
  }
}

function writeSettings(obj) {
  const p = settingsPath();
  fs.mkdirSync(path.dirname(p), { recursive: true });
  const tmp = p + '.clawdy.tmp';
  fs.writeFileSync(tmp, JSON.stringify(obj, null, 2));
  fs.renameSync(tmp, p);
}

function endpoint(port) {
  return `http://localhost:${port}`;
}

function isGatewayUrl(url, port) {
  if (!url) return false;
  try {
    const u = new URL(url);
    const p = u.port || (u.protocol === 'https:' ? '443' : '80');
    return (u.hostname === 'localhost' || u.hostname === '127.0.0.1') && String(p) === String(port);
  } catch (_) {
    return false;
  }
}

function isConnected(port) {
  const s = readSettings();
  return isGatewayUrl(s.env && s.env.ANTHROPIC_BASE_URL, port);
}

/** Connect Claude Code to the gateway. `store` is the app config store (get/save). */
function connect(port, token, store) {
  const s = readSettings();
  s.env = s.env || {};

  // Back up the original values exactly once (preserve across reconnects).
  const cfg = store.get();
  if (!cfg.claudeBackup) {
    const backup = { model: 'model' in s ? s.model : undefined, env: {} };
    for (const k of ALL_BACKUP_KEYS) backup.env[k] = k in s.env ? s.env[k] : undefined;
    store.save(Object.assign({}, cfg, { claudeBackup: backup }));
  }

  s.env.ANTHROPIC_BASE_URL = endpoint(port);
  s.env.ANTHROPIC_AUTH_TOKEN = token;
  for (const k of MODEL_ENV_KEYS) delete s.env[k];
  delete s.model; // let Claude Code send native claude-* names; the gateway maps them
  writeSettings(s);
}

/** Disconnect Claude Code: restore the backed-up state (or just remove our keys). */
function disconnect(store) {
  const s = readSettings();
  s.env = s.env || {};
  const cfg = store.get();
  const b = cfg.claudeBackup;

  if (b) {
    const restore = (k) => {
      if (b.env[k] === undefined) delete s.env[k];
      else s.env[k] = b.env[k];
    };
    for (const k of ALL_BACKUP_KEYS) restore(k);
    if (b.model === undefined) delete s.model;
    else s.model = b.model;
    store.save(Object.assign({}, cfg, { claudeBackup: null }));
  } else {
    delete s.env.ANTHROPIC_BASE_URL;
    delete s.env.ANTHROPIC_AUTH_TOKEN;
  }

  if (s.env && Object.keys(s.env).length === 0) delete s.env;
  writeSettings(s);
}

module.exports = { settingsPath, readSettings, isConnected, connect, disconnect, endpoint, MODEL_ENV_KEYS };
