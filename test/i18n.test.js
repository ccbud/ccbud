'use strict';

// Validates the i18n dictionary: every locale has exactly the same key set as `en`,
// LOCALE_TAG covers every language, and every {param} token resolves when params are given.

const { DICT, LANGS, LOCALE_TAG } = require('../src/shared/i18n-dict');

let pass = 0, fail = 0;
const check = (n, c, d) => { if (c) { pass++; console.log(`  \x1b[32mPASS\x1b[0m ${n}`); } else { fail++; console.log(`  \x1b[31mFAIL\x1b[0m ${n}${d ? ' — ' + d : ''}`); } };

check('LANGS = en/zh/zh-TW/ja/ko', JSON.stringify(LANGS) === JSON.stringify(['en', 'zh', 'zh-TW', 'ja', 'ko']));
check('every lang has a DICT', LANGS.every((l) => DICT[l] && typeof DICT[l] === 'object'));
check('every lang has a LOCALE_TAG', LANGS.every((l) => typeof LOCALE_TAG[l] === 'string' && LOCALE_TAG[l]));

const enKeys = Object.keys(DICT.en).sort();
check('en has keys', enKeys.length > 100, `n=${enKeys.length}`);

for (const l of LANGS) {
  const keys = Object.keys(DICT[l]).sort();
  const missing = enKeys.filter((k) => !(k in DICT[l]));
  const extra = keys.filter((k) => !(k in DICT.en));
  check(`${l}: key set matches en`, missing.length === 0 && extra.length === 0,
    `missing=[${missing.slice(0, 6).join(', ')}] extra=[${extra.slice(0, 6).join(', ')}]`);
  check(`${l}: no empty values`, Object.values(DICT[l]).every((v) => typeof v === 'string' && v.length > 0));
}

// Placeholder parity: a key with {tok} in en must keep the SAME token set in every locale.
const tokens = (s) => (s.match(/\{(\w+)\}/g) || []).sort().join(',');
let tokenMismatch = [];
for (const k of enKeys) {
  const want = tokens(DICT.en[k]);
  for (const l of LANGS) {
    if (tokens(DICT[l][k]) !== want) tokenMismatch.push(`${l}:${k}`);
  }
}
check('interpolation tokens consistent across locales', tokenMismatch.length === 0, tokenMismatch.slice(0, 8).join(' '));

// Simulate the runtime fill() — no {param} left unresolved when all params supplied.
const fill = (s, p) => s.replace(/\{(\w+)\}/g, (_, key) => (p[key] != null ? p[key] : '{' + key + '}'));
const sample = fill(DICT.ja['err.portFailed'], { port: 8788, msg: 'x' });
check('fill resolves all tokens', !/\{\w+\}/.test(sample), sample);

console.log(`\n${pass} passed, ${fail} failed`);
process.exit(fail ? 1 : 0);
