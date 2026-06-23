'use strict';

/*
 * In-app updater. Two tiers:
 *   - HOT  : a JS-only bundle (gzipped JSON of the src/ tree) downloaded from the latest GitHub
 *            release, SHA-256 (+ optional Ed25519) verified, extracted under <userData>/hot/<ver>/
 *            and activated by bootstrap.js on the next launch. No reinstall / signing needed.
 *   - FULL : the new version needs a newer native shell (Electron bump, bundled binaries…), gated
 *            by the manifest's minShellVersion → we point the user at the installer / `brew upgrade`.
 *
 * The release publishes two extra assets (see scripts/build-hotupdate.js):
 *   hotupdate-manifest.json  { version, minShellVersion, sha256, signature?, bundle, notes }
 *   ccbud-hotupdate-<ver>.json.gz   the bundle the manifest points at
 */

const { app } = require('electron');
const fs = require('fs');
const path = require('path');
const https = require('https');
const zlib = require('zlib');
const crypto = require('crypto');
const hp = require('./hotpaths');

const REPO = 'ccbud/ccbud';
const MANIFEST_ASSET = 'hotupdate-manifest.json';
// Optional: paste an Ed25519 public key (SPKI PEM) here to REQUIRE signed bundles. Empty =
// SHA-256-only verification (the manifest is fetched over GitHub HTTPS). See scripts/gen-update-keys.js.
const UPDATE_PUBLIC_KEY = '';

let ctx = { userData: null, getConfig: () => ({}), broadcast: () => {}, log: () => {} };
let lastCheck = null; // cached result of the most recent checkForUpdates()
let checking = false;
let staging = false;

function init(opts) {
  ctx = Object.assign(ctx, opts || {});
}

/* ---------- semver-ish compare (x.y.z, prerelease ignored) ---------- */
function parseVer(v) {
  return String(v || '0')
    .replace(/^v/i, '')
    .split('-')[0]
    .split('.')
    .map((n) => parseInt(n, 10) || 0);
}
function cmpVer(a, b) {
  const pa = parseVer(a);
  const pb = parseVer(b);
  for (let i = 0; i < Math.max(pa.length, pb.length); i++) {
    const d = (pa[i] || 0) - (pb[i] || 0);
    if (d) return d < 0 ? -1 : 1;
  }
  return 0;
}

/* ---------- version helpers ---------- */
function shellVersion() {
  try { return app.getVersion(); } catch (_) { return '0.0.0'; }
}
function runningVersion() {
  // After a hot update the live JS may be newer than the installed shell.
  try {
    const st = hp.readState(ctx.userData);
    if (st.active && st.active.version) return st.active.version;
  } catch (_) {}
  return shellVersion();
}
function installMethod() {
  try { if (!app.isPackaged) return 'dev'; } catch (_) {}
  if (process.platform === 'linux' && process.env.APPIMAGE) return 'appimage';
  if (process.platform === 'darwin') return 'mac';
  if (process.platform === 'win32') return 'win';
  return 'linux';
}

/* ---------- network (follows redirects; strict TLS) ---------- */
function httpsGet(url, redirects) {
  redirects = redirects || 0;
  return new Promise((resolve, reject) => {
    if (redirects > 5) { reject(new Error('too many redirects')); return; }
    const req = https.get(url, { headers: { 'User-Agent': 'ccbud-updater', Accept: 'application/octet-stream, application/json;q=0.9, */*;q=0.5' } }, (res) => {
      const code = res.statusCode || 0;
      if (code >= 300 && code < 400 && res.headers.location) {
        res.resume();
        const next = new URL(res.headers.location, url).href;
        resolve(httpsGet(next, redirects + 1));
        return;
      }
      if (code < 200 || code >= 300) {
        res.resume();
        reject(new Error('HTTP ' + code + ' for ' + url));
        return;
      }
      const chunks = [];
      res.on('data', (c) => chunks.push(c));
      res.on('end', () => resolve(Buffer.concat(chunks)));
    });
    req.on('error', reject);
    req.setTimeout(30000, () => req.destroy(new Error('timeout')));
  });
}
async function getJson(url) {
  const buf = await httpsGet(url);
  return JSON.parse(buf.toString('utf8'));
}

function findAsset(release, name) {
  return (release.assets || []).find((a) => a.name === name) || null;
}
// Best-effort: the platform installer asset for a FULL update (so the UI can deep-link it).
function findInstaller(release, version) {
  const assets = release.assets || [];
  const arch = process.arch === 'arm64' ? 'arm64' : 'x64';
  const want = (ext) => assets.find((a) => a.name.includes('-' + arch + '.') && a.name.endsWith(ext))
    || assets.find((a) => a.name.endsWith(ext));
  let a = null;
  if (process.platform === 'darwin') a = want('.dmg');
  else if (process.platform === 'win32') a = want('.exe');
  else if (process.env.APPIMAGE) a = want('.AppImage');
  else a = want('.deb') || want('.AppImage');
  return a ? a.browser_download_url : null;
}

/* ---------- check ---------- */
async function checkForUpdates(opts) {
  opts = opts || {};
  if (checking) return lastCheck || { ok: false, error: 'busy' };
  checking = true;
  const sv = shellVersion();
  const rv = runningVersion();
  try {
    const release = await getJson('https://api.github.com/repos/' + REPO + '/releases/latest');
    const tag = release.tag_name || '';
    let manifest = null;
    const ma = findAsset(release, MANIFEST_ASSET);
    if (ma) {
      try { manifest = await getJson(ma.browser_download_url); } catch (_) { manifest = null; }
    }
    const latestVersion = (manifest && manifest.version) || tag.replace(/^v/i, '') || sv;
    const hasUpdate = cmpVer(latestVersion, rv) > 0;

    let bundleUrl = null;
    if (manifest && manifest.bundle) {
      const ba = findAsset(release, manifest.bundle);
      bundleUrl = ba ? ba.browser_download_url : null;
    }
    const minShell = (manifest && manifest.minShellVersion) || '0.0.0';
    const hotEligible = !!(hasUpdate && bundleUrl && manifest.sha256 && cmpVer(sv, minShell) >= 0);

    const mode = !hasUpdate ? 'none' : hotEligible ? 'hot' : 'full';
    lastCheck = {
      ok: true,
      checkedAt: Date.now(),
      shellVersion: sv,
      runningVersion: rv,
      latestVersion,
      mode,
      minShellVersion: minShell,
      notes: (manifest && manifest.notes) || release.body || '',
      releaseUrl: release.html_url || ('https://github.com/' + REPO + '/releases/latest'),
      installerUrl: hasUpdate ? findInstaller(release, latestVersion) : null,
      brewCommand: 'brew upgrade --cask ccbud',
      installMethod: installMethod(),
      // hot-only payload (kept server-private to the renderer; used by downloadAndStageHot)
      _bundleUrl: bundleUrl,
      _sha256: manifest && manifest.sha256,
      _signature: manifest && manifest.signature,
    };
  } catch (e) {
    lastCheck = { ok: false, error: (e && e.message) || String(e), checkedAt: Date.now(), shellVersion: sv, runningVersion: rv, installMethod: installMethod() };
  } finally {
    checking = false;
  }
  ctx.broadcast('update:state', publicState());
  return publicState();
}

/* ---------- download + stage a hot bundle ---------- */
function verifyBundle(buf) {
  if (!lastCheck || !lastCheck._sha256) throw new Error('no manifest checksum');
  const got = crypto.createHash('sha256').update(buf).digest('hex');
  if (got.toLowerCase() !== String(lastCheck._sha256).toLowerCase()) throw new Error('checksum mismatch');
  if (UPDATE_PUBLIC_KEY) {
    if (!lastCheck._signature) throw new Error('missing signature');
    const ok = crypto.verify(null, buf, UPDATE_PUBLIC_KEY, Buffer.from(lastCheck._signature, 'base64'));
    if (!ok) throw new Error('signature verification failed');
  }
}
function safeName(v) {
  return String(v || '0').replace(/[^A-Za-z0-9._-]/g, '_');
}

async function downloadAndStageHot() {
  if (staging) return { ok: false, error: 'busy' };
  if (!lastCheck || lastCheck.mode !== 'hot' || !lastCheck._bundleUrl) return { ok: false, error: 'no hot update' };
  staging = true;
  const version = lastCheck.latestVersion;
  try {
    ctx.log('downloading hot update ' + version);
    const gz = await httpsGet(lastCheck._bundleUrl);
    verifyBundle(gz);
    const json = JSON.parse(zlib.gunzipSync(gz).toString('utf8'));
    if (!json || !json.files || typeof json.files !== 'object') throw new Error('bad bundle');

    const root = hp.hotRoot(ctx.userData);
    const stageDir = path.join(root, '.staging-' + safeName(version));
    try { fs.rmSync(stageDir, { recursive: true, force: true }); } catch (_) {}
    fs.mkdirSync(stageDir, { recursive: true, mode: 0o700 });

    const stageResolved = path.resolve(stageDir);
    for (const rel of Object.keys(json.files)) {
      // Bundle paths are posix ('/'); split + rejoin so this is correct on Windows too, and
      // reject path traversal — only paths under the staging dir are allowed.
      const parts = String(rel).split(/[\\/]/).filter((p) => p && p !== '.');
      if (parts.includes('..')) throw new Error('unsafe path in bundle: ' + rel);
      const dest = path.join(stageDir, ...parts);
      if (path.resolve(dest) !== stageResolved && !path.resolve(dest).startsWith(stageResolved + path.sep)) {
        throw new Error('unsafe path in bundle: ' + rel);
      }
      fs.mkdirSync(path.dirname(dest), { recursive: true });
      fs.writeFileSync(dest, Buffer.from(json.files[rel], 'base64'));
    }
    if (!fs.existsSync(hp.mainEntry(stageDir))) throw new Error('bundle missing main entry');

    const finalDir = hp.bundleDir(ctx.userData, safeName(version));
    try { fs.rmSync(finalDir, { recursive: true, force: true }); } catch (_) {}
    fs.renameSync(stageDir, finalDir);

    const st = hp.readState(ctx.userData);
    st.pending = { version, dir: safeName(version) };
    hp.writeState(ctx.userData, st);
    ctx.log('hot update ' + version + ' staged — will apply on next launch');
    ctx.broadcast('update:state', publicState());
    return { ok: true, version };
  } catch (e) {
    ctx.log('hot update failed: ' + ((e && e.message) || e));
    return { ok: false, error: (e && e.message) || String(e) };
  } finally {
    staging = false;
  }
}

function relaunchToApply() {
  try { app.relaunch(); } catch (_) {}
  try { app.exit(0); } catch (_) {}
}

// Called by main.js once the app has booted cleanly, so a "trying" hot bundle is confirmed
// good and rollback won't fire on the next launch. Also prunes stale bundle dirs.
function confirmBootSuccess() {
  try {
    const st = hp.readState(ctx.userData);
    let changed = false;
    if (st.trying && st.active && st.trying === st.active.version) {
      st.trying = null;
      changed = true;
    }
    if (st.previous) {
      try { fs.rmSync(hp.bundleDir(ctx.userData, st.previous.dir), { recursive: true, force: true }); } catch (_) {}
      st.previous = null;
      changed = true;
    }
    if (changed) hp.writeState(ctx.userData, st);
    // Prune any leftover bundle dirs that aren't the active one.
    const keep = st.active && st.active.dir;
    const root = hp.hotRoot(ctx.userData);
    for (const name of fs.existsSync(root) ? fs.readdirSync(root) : []) {
      const full = path.join(root, name);
      if (name === 'state.json' || name === 'state.json.tmp') continue;
      try {
        if (!fs.statSync(full).isDirectory()) continue;
        if (name !== keep) fs.rmSync(full, { recursive: true, force: true });
      } catch (_) {}
    }
  } catch (_) {}
}

/* ---------- state for the renderer ---------- */
function pendingState() {
  try {
    const st = hp.readState(ctx.userData);
    return st.pending ? { staged: true, version: st.pending.version } : { staged: false };
  } catch (_) { return { staged: false }; }
}
// Strip the internal _-prefixed fields before handing the check result to the renderer.
function publicState() {
  const base = {
    shellVersion: shellVersion(),
    runningVersion: runningVersion(),
    installMethod: installMethod(),
    pending: pendingState(),
  };
  if (!lastCheck) return Object.assign(base, { mode: 'unknown' });
  const c = {};
  for (const k of Object.keys(lastCheck)) if (k[0] !== '_') c[k] = lastCheck[k];
  return Object.assign(base, c);
}

module.exports = {
  init,
  checkForUpdates,
  downloadAndStageHot,
  relaunchToApply,
  confirmBootSuccess,
  publicState,
  installMethod,
  cmpVer,
};
