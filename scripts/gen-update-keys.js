#!/usr/bin/env node
'use strict';

/*
 * Generates an Ed25519 keypair for signing hot-update bundles.
 *   - PRIVATE key  → keep secret; store as the CCBUD_UPDATE_PRIVATE_KEY repo secret (used by
 *                    scripts/build-hotupdate.js in CI).
 *   - PUBLIC key   → paste into src/main/updater.js UPDATE_PUBLIC_KEY to REQUIRE signed bundles.
 *
 * Run: npm run gen:updatekeys
 */

const crypto = require('crypto');

const { publicKey, privateKey } = crypto.generateKeyPairSync('ed25519');
const pub = publicKey.export({ type: 'spki', format: 'pem' }).toString();
const priv = privateKey.export({ type: 'pkcs8', format: 'pem' }).toString();

console.log('=== PUBLIC KEY (paste into src/main/updater.js UPDATE_PUBLIC_KEY) ===\n');
console.log(pub);
console.log('=== PRIVATE KEY (store as CCBUD_UPDATE_PRIVATE_KEY secret — keep secret!) ===\n');
console.log(priv);
