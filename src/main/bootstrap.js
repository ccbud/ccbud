'use strict';

/*
 * Hot-update bootstrap — the app's real Electron entry point (package.json "main").
 *
 * Before the actual app code runs, this resolves WHICH copy of the app to load:
 *   - the version baked into the installed bundle ("shell"), or
 *   - a newer JS-only bundle the updater downloaded into <userData>/hot/<version>/.
 *
 * Because a hot bundle is just interpreted JS loaded by the already-installed native shell,
 * applying one needs no code-signing / reinstall — only the JS/renderer layer changes. Native
 * changes (Electron bump, bundled binaries) are gated by the manifest's minShellVersion and
 * fall back to a full installer instead (see updater.js).
 *
 * Safety: promotion of a staged bundle and rollback of a bundle that fails to boot both happen
 * here, and the require is wrapped so a broken bundle can never brick the app — it falls back to
 * the packaged shell. main.js confirms a successful boot (clears `trying`) via updater.js.
 */

const { app } = require('electron');
const fs = require('fs');
const path = require('path');
const hp = require('./hotpaths');

const PACKAGED_ROOT = app.getAppPath(); // dir (or app.asar) that contains src/main/main.js

function entryExists(root) {
  try { return !!root && fs.existsSync(hp.mainEntry(root)); } catch (_) { return false; }
}

// Resolve the bundle root to load, performing pending-promotion and crash-rollback on the way.
function resolveRoot(userData) {
  let state;
  try { state = hp.readState(userData); } catch (_) { return PACKAGED_ROOT; }
  let dirty = false;

  // 1) A staged bundle is waiting → promote it to active for this launch.
  if (state.pending && state.pending.dir) {
    const stagedRoot = hp.bundleDir(userData, state.pending.dir);
    if (entryExists(stagedRoot)) {
      state.previous = state.active || null;
      state.active = state.pending;
      state.trying = state.active.version || null; // unconfirmed until main.js says it booted
    }
    state.pending = null;
    dirty = true;
  } else if (state.trying && state.active && state.trying === state.active.version) {
    // 2) Last launch promoted this active bundle but never confirmed a clean boot
    //    (likely crashed during startup) → roll back to the previous known-good / packaged.
    const bad = state.active;
    state.active = state.previous || null;
    state.previous = null;
    state.trying = null;
    dirty = true;
    // best-effort: drop the bad bundle so it isn't retried
    try { if (bad && bad.dir) fs.rmSync(hp.bundleDir(userData, bad.dir), { recursive: true, force: true }); } catch (_) {}
  }

  if (dirty) { try { hp.writeState(userData, state); } catch (_) {} }

  if (state.active && state.active.dir) {
    const activeRoot = hp.bundleDir(userData, state.active.dir);
    if (entryExists(activeRoot)) return activeRoot;
    // active points at a missing/broken dir → clear it and use packaged
    try { state.active = null; state.trying = null; hp.writeState(userData, state); } catch (_) {}
  }
  return PACKAGED_ROOT;
}

function loadMain(root) {
  // Mark which root won so main.js / updater can report the running JS version accurately.
  try { process.env.CCBUD_APP_ROOT = root; } catch (_) {}
  require(hp.mainEntry(root));
}

(function bootstrap() {
  let userData;
  try { userData = app.getPath('userData'); } catch (_) { userData = null; }

  const root = userData ? resolveRoot(userData) : PACKAGED_ROOT;
  try {
    loadMain(root);
  } catch (e) {
    // A staged bundle threw on load — quarantine it and fall back to the packaged shell so the
    // app still starts. (If the packaged shell itself throws, there's nothing left to do.)
    if (root !== PACKAGED_ROOT && userData) {
      try {
        const state = hp.readState(userData);
        const bad = state.active;
        state.active = state.previous || null;
        state.previous = null;
        state.trying = null;
        hp.writeState(userData, state);
        if (bad && bad.dir) { try { fs.rmSync(hp.bundleDir(userData, bad.dir), { recursive: true, force: true }); } catch (_) {} }
      } catch (_) {}
      try { console.error('[ccbud] hot bundle failed to load, falling back to packaged:', e && e.message); } catch (_) {}
      loadMain(PACKAGED_ROOT);
    } else {
      throw e;
    }
  }
})();
