'use strict';

/**
 * Config normalization. Locks issue #11 (custom provider icons were silently dropped on save
 * because normalize() rebuilt each provider without the `icon` field) plus the new gateway
 * defaults (429 retry, insecure TLS).
 */

const { normalize, defaultConfig } = require('../src/main/store');

let pass = 0, fail = 0;
const check = (n, c, d) => { if (c) { pass++; console.log(`  \x1b[32mPASS\x1b[0m ${n}`); } else { fail++; console.log(`  \x1b[31mFAIL\x1b[0m ${n}${d ? ' — ' + d : ''}`); } };

// --- Issue #11: provider icon survives normalize() ---
const emojiCfg = normalize({ providers: [{ id: 'p1', name: 'P1', baseUrl: 'http://x', icon: '🦊' }] });
check('emoji icon preserved', emojiCfg.providers[0].icon === '🦊', JSON.stringify(emojiCfg.providers[0]));

const dataUrl = 'data:image/png;base64,iVBORw0KGgoAAAANSUhEUg==';
const imgCfg = normalize({ providers: [{ id: 'p2', name: 'P2', baseUrl: 'http://x', icon: dataUrl }] });
check('uploaded image data-URL icon preserved', imgCfg.providers[0].icon === dataUrl);

const trimCfg = normalize({ providers: [{ id: 'p3', name: 'P3', baseUrl: 'http://x', icon: '  ⚡  ' }] });
check('icon is trimmed', trimCfg.providers[0].icon === '⚡');

const noIcon = normalize({ providers: [{ id: 'p4', name: 'P4', baseUrl: 'http://x' }] });
check('no icon → field absent (falls back to brand/default logo)', !('icon' in noIcon.providers[0]));

const blankIcon = normalize({ providers: [{ id: 'p5', name: 'P5', baseUrl: 'http://x', icon: '   ' }] });
check('blank icon dropped (treated as reset)', !('icon' in blankIcon.providers[0]));

const nonString = normalize({ providers: [{ id: 'p6', name: 'P6', baseUrl: 'http://x', icon: { nope: 1 } }] });
check('non-string icon ignored', !('icon' in nonString.providers[0]));

// Other provider fields still normalized as before alongside the icon.
check('icon does not disturb other fields', emojiCfg.providers[0].baseUrl === 'http://x' && emojiCfg.providers[0].mapDefaultModels === true);

// --- New gateway defaults ---
const d = defaultConfig();
check('default: 429 retry enabled', d.retry429 && d.retry429.enabled === true);
check('default: 429 retry max=3 baseMs=500', d.retry429.max === 3 && d.retry429.baseMs === 500);
check('default: insecureSkipVerify off', d.insecureSkipVerify === false);

// retry429 clamping
const clamped = normalize({ retry429: { enabled: true, max: 999, baseMs: -5 } });
check('retry max clamped to 10', clamped.retry429.max === 10, `max=${clamped.retry429.max}`);
check('negative baseMs floored to default', clamped.retry429.baseMs === 500, `baseMs=${clamped.retry429.baseMs}`);
check('insecureSkipVerify coerced to boolean', normalize({ insecureSkipVerify: 1 }).insecureSkipVerify === true);

console.log(`\n${pass} passed, ${fail} failed`);
process.exit(fail ? 1 : 0);
