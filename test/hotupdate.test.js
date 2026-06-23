'use strict';

/**
 * Hot-update plumbing: hotpaths state roundtrip + promotion/rollback state machine,
 * version comparison, and the build→extract contract (a bundle from scripts/build-hotupdate.js
 * extracts to a tree whose main entry exists). Runs without Electron.
 */

const fs = require('fs');
const os = require('os');
const path = require('path');
const zlib = require('zlib');
const crypto = require('crypto');
const cp = require('child_process');
const hp = require('../src/main/hotpaths');
const { cmpVer } = require('../src/main/updater');

let pass = 0, fail = 0;
const check = (n, c, d) => { if (c) { pass++; console.log(`  \x1b[32mPASS\x1b[0m ${n}`); } else { fail++; console.log(`  \x1b[31mFAIL\x1b[0m ${n}${d ? ' — ' + d : ''}`); } };

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'ccbud-hot-'));

/* ---- hotpaths state ---- */
const empty = hp.readState(tmp);
check('readState defaults', empty.active === null && empty.pending === null && empty.previous === null && empty.trying === null);

hp.writeState(tmp, { active: { version: '1.0.7', dir: '1.0.7' }, pending: null, previous: null, trying: null });
const rt = hp.readState(tmp);
check('writeState roundtrip', rt.active && rt.active.version === '1.0.7');
check('mainEntry layout', hp.mainEntry('/x').endsWith(path.join('src', 'main', 'main.js')));

/* ---- promotion + rollback state machine (mirror of bootstrap.js) ---- */
function resolveStep(userData) {
  const st = hp.readState(userData);
  if (st.pending && st.pending.dir) {
    st.previous = st.active || null;
    st.active = st.pending;
    st.trying = st.active.version;
    st.pending = null;
  } else if (st.trying && st.active && st.trying === st.active.version) {
    st.active = st.previous || null;
    st.previous = null;
    st.trying = null;
  }
  hp.writeState(userData, st);
  return st;
}

// stage a pending bundle → promote
hp.writeState(tmp, { active: null, pending: { version: '1.1.0', dir: '1.1.0' }, previous: null, trying: null });
let s = resolveStep(tmp);
check('pending promoted to active', s.active && s.active.version === '1.1.0' && s.pending === null && s.trying === '1.1.0');

// confirmed boot clears trying
s.trying = null; hp.writeState(tmp, s);
s = resolveStep(tmp);
check('confirmed boot stays active', s.active && s.active.version === '1.1.0' && s.trying === null);

// crash before confirm (trying still set) → rollback to previous
hp.writeState(tmp, { active: { version: '1.2.0', dir: '1.2.0' }, pending: null, previous: { version: '1.1.0', dir: '1.1.0' }, trying: '1.2.0' });
s = resolveStep(tmp);
check('unconfirmed boot rolls back', s.active && s.active.version === '1.1.0' && s.trying === null);

/* ---- version compare ---- */
check('cmpVer newer', cmpVer('1.0.7', '1.0.6') > 0);
check('cmpVer older', cmpVer('1.0.6', '1.0.7') < 0);
check('cmpVer equal', cmpVer('1.2.3', '1.2.3') === 0);
check('cmpVer v-prefix + prerelease', cmpVer('v1.2.0-beta', '1.1.9') > 0);
check('cmpVer minor gate', cmpVer('1.0.6', '1.0.0') >= 0);

/* ---- build → extract contract ---- */
const ROOT = path.resolve(__dirname, '..');
try {
  cp.execSync('node scripts/build-hotupdate.js', { cwd: ROOT, stdio: 'ignore' });
  const manifest = JSON.parse(fs.readFileSync(path.join(ROOT, 'dist', 'hotupdate-manifest.json'), 'utf8'));
  const gz = fs.readFileSync(path.join(ROOT, 'dist', manifest.bundle));
  check('manifest sha256 matches bundle', crypto.createHash('sha256').update(gz).digest('hex') === manifest.sha256);
  const bundle = JSON.parse(zlib.gunzipSync(gz).toString('utf8'));

  // extract exactly like updater.downloadAndStageHot()
  const dest = path.join(tmp, 'extract');
  for (const rel of Object.keys(bundle.files)) {
    const parts = rel.split(/[\\/]/).filter((p) => p && p !== '.');
    const f = path.join(dest, ...parts);
    fs.mkdirSync(path.dirname(f), { recursive: true });
    fs.writeFileSync(f, Buffer.from(bundle.files[rel], 'base64'));
  }
  check('extracted bundle has main entry', fs.existsSync(hp.mainEntry(dest)));
  check('extracted bundle has bootstrap', fs.existsSync(path.join(dest, 'src', 'main', 'bootstrap.js')));
  check('extracted bundle has compiled styles', fs.existsSync(path.join(dest, 'src', 'renderer', 'styles.css')));
} catch (e) {
  check('build→extract contract', false, e.message);
}

fs.rmSync(tmp, { recursive: true, force: true });

console.log(`\n${pass} passed, ${fail} failed`);
process.exit(fail ? 1 : 0);
