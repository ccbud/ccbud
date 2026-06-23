#!/usr/bin/env node
'use strict';

/*
 * Builds the hot-update assets that ride along with a GitHub release:
 *   dist/ccbud-hotupdate-<version>.json.gz   gzip(JSON{ version, files:{ 'src/..': base64 } })
 *   dist/hotupdate-manifest.json             { version, minShellVersion, sha256, signature?, bundle, notes }
 *
 * The bundle is the entire src/ tree (the JS/renderer layer the app runs). bootstrap.js +
 * updater.js verify and apply it without a reinstall. Run AFTER `npm run build:css` so the
 * compiled styles.css is current.
 *
 * Signing (optional, recommended): set CCBUD_UPDATE_PRIVATE_KEY to an Ed25519 private key (PKCS#8
 * PEM) to attach a signature; paste the matching public key into updater.js UPDATE_PUBLIC_KEY to
 * REQUIRE it on the client. Generate a keypair with `npm run gen:updatekeys`.
 */

const fs = require('fs');
const path = require('path');
const zlib = require('zlib');
const crypto = require('crypto');

const ROOT = path.resolve(__dirname, '..');
const pkg = JSON.parse(fs.readFileSync(path.join(ROOT, 'package.json'), 'utf8'));
const version = pkg.version;
const minShellVersion = (pkg.hotUpdate && pkg.hotUpdate.minShellVersion) || version;

// Recursively collect every file under src/ as { 'src/rel/path': base64 } (posix keys).
function collect(dir, base, out) {
  for (const name of fs.readdirSync(dir)) {
    const full = path.join(dir, name);
    const st = fs.statSync(full);
    const rel = path.relative(base, full).split(path.sep).join('/');
    if (st.isDirectory()) collect(full, base, out);
    else if (st.isFile()) out[rel] = fs.readFileSync(full).toString('base64');
  }
}

const srcDir = path.join(ROOT, 'src');
const files = {};
collect(srcDir, ROOT, files);

if (!files['src/renderer/styles.css']) {
  console.warn('[build-hotupdate] WARNING: src/renderer/styles.css missing — run `npm run build:css` first.');
}

const bundleName = `ccbud-hotupdate-${version}.json.gz`;
const distDir = path.join(ROOT, 'dist');
fs.mkdirSync(distDir, { recursive: true });

const payload = JSON.stringify({ version, files });
const gz = zlib.gzipSync(Buffer.from(payload, 'utf8'), { level: 9 });
fs.writeFileSync(path.join(distDir, bundleName), gz);

const sha256 = crypto.createHash('sha256').update(gz).digest('hex');

let signature;
const privPem = process.env.CCBUD_UPDATE_PRIVATE_KEY;
if (privPem && privPem.trim()) {
  try {
    const key = crypto.createPrivateKey(privPem);
    signature = crypto.sign(null, gz, key).toString('base64');
    console.log('[build-hotupdate] signed bundle (Ed25519)');
  } catch (e) {
    console.error('[build-hotupdate] signing failed:', e.message);
    process.exit(1);
  }
}

const manifest = {
  version,
  minShellVersion,
  bundle: bundleName,
  sha256,
  notes: process.env.RELEASE_NOTES || `ccbud ${version}`,
};
if (signature) manifest.signature = signature;

fs.writeFileSync(path.join(distDir, 'hotupdate-manifest.json'), JSON.stringify(manifest, null, 2));

console.log('[build-hotupdate] version       :', version);
console.log('[build-hotupdate] minShellVersion:', minShellVersion);
console.log('[build-hotupdate] files         :', Object.keys(files).length);
console.log('[build-hotupdate] bundle        :', bundleName, '(' + (gz.length / 1024).toFixed(1) + ' KiB)');
console.log('[build-hotupdate] sha256        :', sha256);
console.log('[build-hotupdate] manifest      : dist/hotupdate-manifest.json');
