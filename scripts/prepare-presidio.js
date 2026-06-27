#!/usr/bin/env node
'use strict';

/*
 * Cross-platform build of the BUNDLED Presidio runtime (replaces the old bash prepare-presidio.sh,
 * which only ran on macOS and needed an external presidio source checkout).
 *
 * Produces, for the CURRENT platform:
 *   vendor/presidio-env/python   a self-contained, relocatable standalone Python (python-build-
 *                                standalone, via uv) with presidio-analyzer/anonymizer[server] +
 *                                the spaCy small model installed into ITS OWN site-packages.
 *   vendor/presidio-src/...      the official Flask entry points (app.py + logging.ini), bundled IN
 *                                THIS REPO at assets/presidio-src — no external checkout required.
 *
 * electron-builder ships both as extraResources (-> resourcesPath/presidio-env + /presidio), which
 * src/main/presidio.js auto-detects and runs locally. Python is NATIVE + per-arch, so this must run
 * on each target OS's build (macOS / Windows / Linux). Presidio itself comes from PyPI, so the build
 * is fully reproducible on a clean machine.
 *
 * Requires `uv` on PATH (CI installs it via astral-sh/setup-uv; locally: https://astral.sh/uv).
 */

const fs = require('fs');
const path = require('path');
const os = require('os');
const { execFileSync } = require('child_process');

const ROOT = path.resolve(__dirname, '..');
const OUT = path.join(ROOT, 'vendor', 'presidio-env');
const SRCOUT = path.join(ROOT, 'vendor', 'presidio-src');
const SRCIN = path.join(ROOT, 'assets', 'presidio-src');
const PYVER = process.env.PRESIDIO_PYTHON || '3.12';
// spaCy small models as direct wheels (uv venvs have no pip, so `spacy download` won't work).
// English + Chinese, so the NER tier can recognize Chinese names / places / orgs too.
const MODELS = (process.env.PRESIDIO_MODELS || [
  'https://github.com/explosion/spacy-models/releases/download/en_core_web_sm-3.8.0/en_core_web_sm-3.8.0-py3-none-any.whl',
  'https://github.com/explosion/spacy-models/releases/download/zh_core_web_sm-3.8.0/zh_core_web_sm-3.8.0-py3-none-any.whl',
].join(' ')).split(/\s+/).filter(Boolean);
const IS_WIN = process.platform === 'win32';

// Locate uv: PATH first (CI), then the common per-user / Homebrew install locations.
function resolveUv() {
  const cands = [
    process.env.CCBUD_UV,
    path.join(os.homedir(), IS_WIN ? '.local\\bin\\uv.exe' : '.local/bin/uv'),
    '/opt/homebrew/bin/uv',
    '/usr/local/bin/uv',
  ].filter(Boolean);
  for (const p of cands) { try { if (fs.existsSync(p)) return p; } catch (_) {} }
  return IS_WIN ? 'uv.exe' : 'uv'; // rely on PATH
}
const UV = resolveUv();
const uv = (args) => execFileSync(UV, args, { stdio: 'inherit' });
const uvOut = (args) => execFileSync(UV, args, { encoding: 'utf8' }).trim();

console.log('[prepare-presidio] platform:', process.platform, process.arch, '| uv:', UV);

// 1) Ensure a standalone Python exists, then locate its install root.
uv(['python', 'install', PYVER]); // idempotent; downloads python-build-standalone if missing
const foundExe = uvOut(['python', 'find', PYVER]);
// Windows layout: <root>\python.exe ; POSIX layout: <root>/bin/python3.12
const pyRoot = IS_WIN ? path.dirname(foundExe) : path.resolve(path.dirname(foundExe), '..');
if (!/cpython|uv[\\/]+python/i.test(pyRoot)) {
  throw new Error('refusing: not a relocatable uv standalone python -> ' + pyRoot);
}
console.log('[prepare-presidio] standalone python root:', pyRoot);

// 2) Install Presidio (server extras) + models INTO the uv-managed standalone Python FIRST, then copy
//    the whole tree dereferencing symlinks. Two cross-platform reasons:
//    · installing into a freshly-COPIED tree makes uv resolve a bogus relocated path → fails on Linux;
//    · macOS `codesign --deep --strict` rejects bundle-internal symlinks ("invalid destination for
//      symbolic link"), so the shipped tree must contain REAL files only (dereference: true).
const srcPy = IS_WIN ? path.join(pyRoot, 'python.exe') : path.join(pyRoot, 'bin', 'python3.12');
console.log('[prepare-presidio] installing presidio[server] + spaCy models (en, zh) from PyPI…');
uv(['pip', 'install', '--python', srcPy, '--break-system-packages',
  'presidio-analyzer[server]', 'presidio-anonymizer[server]', ...MODELS]);

fs.rmSync(OUT, { recursive: true, force: true });
fs.rmSync(SRCOUT, { recursive: true, force: true });
fs.mkdirSync(OUT, { recursive: true });
// Flatten ALL symlinks into real files: fs.cpSync({dereference}) leaves inner symlinks behind, and
// macOS codesign rejects bundle-internal symlinks. `cp -RL` follows every link; Windows has none.
if (IS_WIN) {
  fs.cpSync(pyRoot, path.join(OUT, 'python'), { recursive: true });
} else {
  execFileSync('cp', ['-RL', pyRoot, path.join(OUT, 'python')], { stdio: 'inherit' });
}
const outPy = IS_WIN
  ? path.join(OUT, 'python', 'python.exe')
  : path.join(OUT, 'python', 'bin', 'python3.12');
if (!fs.existsSync(outPy)) throw new Error('copied python interpreter not found at ' + outPy);

// 3) Bundle the official Flask entry points from the repo (no external checkout).
for (const svc of ['presidio-analyzer', 'presidio-anonymizer']) {
  const d = path.join(SRCOUT, svc);
  fs.mkdirSync(d, { recursive: true });
  for (const f of ['app.py', 'logging.ini']) {
    fs.copyFileSync(path.join(SRCIN, svc, f), path.join(d, f));
  }
}

// 4) Smoke test: the shipped interpreter can import Presidio.
execFileSync(outPy, ['-c', 'import presidio_analyzer, presidio_anonymizer; print("presidio import OK")'], { stdio: 'inherit' });
console.log('[prepare-presidio] done ->', path.relative(ROOT, OUT), '+', path.relative(ROOT, SRCOUT));
