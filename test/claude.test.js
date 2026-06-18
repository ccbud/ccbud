'use strict';

// Tests the one-click Claude settings integration against a TEMP file (never ~/.claude).
const fs = require('fs');
const os = require('os');
const path = require('path');

const tmp = path.join(os.tmpdir(), `clawdy-claude-test-${process.pid}.json`);
process.env.CLAWDY_CLAUDE_SETTINGS = tmp;
const claude = require('../src/main/claude');

let pass = 0, fail = 0;
const check = (name, cond, d) => {
  if (cond) { pass++; console.log(`  \x1b[32mPASS\x1b[0m ${name}`); }
  else { fail++; console.log(`  \x1b[31mFAIL\x1b[0m ${name}${d ? ' — ' + d : ''}`); }
};

// in-memory store
function makeStore(initial) {
  let cfg = initial || { providers: [], claudeBackup: null };
  return { get: () => cfg, save: (c) => (cfg = c) };
}
const read = () => { try { return JSON.parse(fs.readFileSync(tmp, 'utf8')); } catch { return null; } };

try {
  // Scenario 1: existing settings with model overrides + top-level model (mirrors the real user file)
  fs.writeFileSync(tmp, JSON.stringify({
    env: { ANTHROPIC_DEFAULT_SONNET_MODEL: 'glm-4.6', ANTHROPIC_DEFAULT_OPUS_MODEL: 'glm-4.6', OTHER: 'keep-me' },
    model: 'MiniMax-M2.5',
    enabledPlugins: { foo: true },
  }, null, 2));

  const store = makeStore();
  claude.connect(8788, 'clawdy-local', store);
  let s = read();
  check('connect sets ANTHROPIC_BASE_URL', s.env.ANTHROPIC_BASE_URL === 'http://127.0.0.1:8788');
  check('connect sets ANTHROPIC_AUTH_TOKEN', s.env.ANTHROPIC_AUTH_TOKEN === 'clawdy-local');
  check('connect clears model overrides', !('ANTHROPIC_DEFAULT_SONNET_MODEL' in s.env) && !('ANTHROPIC_DEFAULT_OPUS_MODEL' in s.env));
  check('connect clears top-level model', !('model' in s));
  check('connect preserves unrelated env', s.env.OTHER === 'keep-me');
  check('connect preserves unrelated keys', s.enabledPlugins && s.enabledPlugins.foo === true);
  check('isConnected true after connect', claude.isConnected(8788));
  check('backup saved', !!store.get().claudeBackup);

  // Disconnect restores exactly
  claude.disconnect(store);
  s = read();
  check('disconnect restores sonnet override', s.env.ANTHROPIC_DEFAULT_SONNET_MODEL === 'glm-4.6');
  check('disconnect restores top-level model', s.model === 'MiniMax-M2.5');
  check('disconnect removes our BASE_URL', !('ANTHROPIC_BASE_URL' in s.env));
  check('disconnect removes our AUTH_TOKEN', !('ANTHROPIC_AUTH_TOKEN' in s.env));
  check('disconnect preserves unrelated env', s.env.OTHER === 'keep-me');
  check('isConnected false after disconnect', !claude.isConnected(8788));
  check('backup cleared', !store.get().claudeBackup);

  // Scenario 2: no settings file at all
  fs.rmSync(tmp, { force: true });
  const store2 = makeStore();
  claude.connect(9000, 'tok2', store2);
  s = read();
  check('connect creates file when none', s && s.env.ANTHROPIC_BASE_URL === 'http://127.0.0.1:9000');
  claude.disconnect(store2);
  s = read();
  check('disconnect on fresh leaves no ANTHROPIC_BASE_URL', !(s.env && s.env.ANTHROPIC_BASE_URL));
} finally {
  fs.rmSync(tmp, { force: true });
}

console.log(`\n${pass} passed, ${fail} failed`);
process.exit(fail ? 1 : 0);
