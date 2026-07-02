'use strict';

// Validates the pure ZIP container (src/main/zipStore.js): build → read round-trips names + bytes
// (through both the STORE and DEFLATE paths), the archive is readable by Node's own zlib-agnostic
// central-directory layout, and splitBundle() recovers the main session + subagent files from a
// conversation bundle. This is the local proof of the byte format that the Rust port mirrors.

const { execFileSync } = require('child_process');
const { buildZip, readZip, splitBundle, crc32 } = require('../src/main/zipStore');

let pass = 0, fail = 0;
const check = (n, c, d) => { if (c) { pass++; console.log(`  \x1b[32mPASS\x1b[0m ${n}`); } else { fail++; console.log(`  \x1b[31mFAIL\x1b[0m ${n}${d ? ' — ' + d : ''}`); } };

// ---- crc32 against a known vector ("123456789" → 0xCBF43926, the standard CRC-32 check value) ----
check('crc32 check value', crc32(Buffer.from('123456789')) === 0xcbf43926, crc32(Buffer.from('123456789')).toString(16));

// ---- round-trip: a small (STORE) entry, a compressible (DEFLATE) entry, and binary bytes ----
const big = Buffer.from('{"type":"assistant"}\n'.repeat(4000), 'utf8'); // very compressible → DEFLATE
const bin = Buffer.from([0, 1, 2, 3, 255, 254, 10, 13, 0, 42]);
const entries = [
  { name: 'main.jsonl', data: 'hi\n' },
  { name: 'subagents/agent-aaa.jsonl', data: big },
  { name: 'subagents/agent-aaa.meta.json', data: '{"toolUseId":"tu1"}' },
  { name: 'blob.bin', data: bin },
];
const zip = buildZip(entries);
check('buildZip returns a Buffer', Buffer.isBuffer(zip) && zip.length > 0);
check('zip starts with PK local header', zip.readUInt32LE(0) === 0x04034b50);
check('deflate actually shrank the big entry', zip.length < big.length, `zip=${zip.length} raw=${big.length}`);

const read = readZip(zip);
check('readZip recovers all entries', read.length === entries.length, `n=${read.length}`);
for (const src of entries) {
  const got = read.find((r) => r.name === src.name);
  const want = Buffer.isBuffer(src.data) ? src.data : Buffer.from(src.data, 'utf8');
  check(`round-trips ${src.name}`, !!got && Buffer.compare(got.data, want) === 0);
}

// ---- readable by the system `unzip` (proves it's a real ZIP, not just self-consistent) ----
try {
  const fs = require('fs');
  const os = require('os');
  const path = require('path');
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'ccbud-zip-'));
  const zp = path.join(tmp, 'b.zip');
  fs.writeFileSync(zp, zip);
  const listing = execFileSync('unzip', ['-l', zp], { encoding: 'utf8' });
  check('system unzip lists main.jsonl', listing.includes('main.jsonl'));
  check('system unzip lists nested subagent', listing.includes('subagents/agent-aaa.jsonl'));
  const extracted = execFileSync('unzip', ['-p', zp, 'subagents/agent-aaa.meta.json'], { encoding: 'utf8' });
  check('system unzip extracts meta content', extracted.trim() === '{"toolUseId":"tu1"}', extracted.trim());
  fs.rmSync(tmp, { recursive: true, force: true });
} catch (e) {
  console.log(`  \x1b[33mSKIP\x1b[0m system unzip check (${(e && e.message) || e})`);
}

// ---- splitBundle: recover main + subagents from a bundle's entries ----
const sb = splitBundle(read);
check('splitBundle finds the main session', !!sb.main && sb.main.name === 'main.jsonl');
check('splitBundle collects 2 subagent files', sb.subagents.length === 2, `n=${sb.subagents.length}`);
check('splitBundle strips subagents/ prefix from names', sb.subagents.every((s) => !s.name.includes('/')));
check('splitBundle keeps the .meta.json sidecar',
  sb.subagents.some((s) => s.name === 'agent-aaa.meta.json'));
check('splitBundle ignores non-bundle blobs (blob.bin)',
  !sb.subagents.some((s) => s.name === 'blob.bin') && sb.main.name !== 'blob.bin');

// ---- tolerant of a wrapping folder (user zipped the directory, not its contents) ----
const wrapped = splitBundle([
  { name: 'bundle/sess.jsonl', data: Buffer.from('x') },
  { name: 'bundle/subagents/agent-x.jsonl', data: Buffer.from('y') },
]);
check('splitBundle handles a wrapping folder', !!wrapped.main && wrapped.main.name === 'sess.jsonl' && wrapped.subagents.length === 1);

// ---- empty / garbage input never throws ----
check('readZip tolerates garbage', readZip(Buffer.from('not a zip at all')).length === 0);
check('readZip tolerates empty', readZip(Buffer.alloc(0)).length === 0);

console.log(`\n${pass} passed, ${fail} failed`);
process.exit(fail ? 1 : 0);
