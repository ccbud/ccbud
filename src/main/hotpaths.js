'use strict';

/*
 * Shared on-disk layout + state for the hot-update system. Used by BOTH the bootstrap
 * loader (src/main/bootstrap.js — runs before the real app) and the updater (updater.js).
 * Keeping the schema in one tiny, dependency-free module is what lets the staged bundle's
 * copy of these files agree byte-for-byte with the packaged shell's copy.
 *
 * Layout (under <userData>/hot):
 *   hot/state.json        — pointer + rollback bookkeeping (see schema below)
 *   hot/<version>/        — an extracted hot bundle; contains src/main/main.js, etc.
 *
 * state.json schema:
 *   {
 *     active:   { version, dir } | null,  // the live hot bundle (dir is relative to hot/)
 *     pending:  { version, dir } | null,  // staged, promoted to active on next launch
 *     previous: { version, dir } | null,  // last known-good active, for rollback
 *     trying:   string | null             // version we promoted but haven't confirmed booted ok
 *   }
 */

const fs = require('fs');
const path = require('path');

function hotRoot(userData) {
  return path.join(userData, 'hot');
}
function stateFile(userData) {
  return path.join(hotRoot(userData), 'state.json');
}
function bundleDir(userData, dirName) {
  return path.join(hotRoot(userData), dirName);
}
// The entry the bootstrap requires for a given bundle root (packaged root OR a hot bundle dir).
function mainEntry(root) {
  return path.join(root, 'src', 'main', 'main.js');
}

function readState(userData) {
  try {
    const raw = fs.readFileSync(stateFile(userData), 'utf8');
    const s = JSON.parse(raw);
    if (s && typeof s === 'object') {
      return {
        active: s.active || null,
        pending: s.pending || null,
        previous: s.previous || null,
        trying: typeof s.trying === 'string' ? s.trying : null,
      };
    }
  } catch (_) {}
  return { active: null, pending: null, previous: null, trying: null };
}

function writeState(userData, state) {
  const dir = hotRoot(userData);
  try { fs.mkdirSync(dir, { recursive: true, mode: 0o700 }); } catch (_) {}
  const file = stateFile(userData);
  const tmp = file + '.tmp';
  fs.writeFileSync(tmp, JSON.stringify(state, null, 2), { mode: 0o600 });
  fs.renameSync(tmp, file);
  return state;
}

module.exports = { hotRoot, stateFile, bundleDir, mainEntry, readState, writeState };
