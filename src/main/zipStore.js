'use strict';

/**
 * Minimal ZIP reader/writer for conversation bundles — no external deps.
 *
 * A conversation with subagents exports as a .zip whose FIRST level is the main session .jsonl
 * and whose `subagents/` directory holds the per-subagent files (`agent-<id>.jsonl` +
 * `agent-<id>.meta.json`). Re-importing that .zip restores the same on-disk relationship. This
 * module only implements the slice of the ZIP spec that round-trip needs:
 *   - write: STORE or raw-DEFLATE per entry (whichever is smaller), no zip64, no data descriptors.
 *   - read: parse via the central directory (so zips repacked by the OS — which use data
 *     descriptors — still read), handling STORE (0) and DEFLATE (8).
 * Kept in lockstep with the Rust port in src-tauri/src/ziputil.rs.
 */

const zlib = require('zlib');

const CRC_TABLE = (() => {
  const t = new Uint32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = (c & 1) ? (0xedb88320 ^ (c >>> 1)) : (c >>> 1);
    t[n] = c >>> 0;
  }
  return t;
})();

function crc32(buf) {
  let c = 0xffffffff;
  for (let i = 0; i < buf.length; i++) c = CRC_TABLE[(c ^ buf[i]) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

function toBuf(data) {
  return Buffer.isBuffer(data) ? data : Buffer.from(data == null ? '' : String(data), 'utf8');
}

// entries: [{ name, data }] where data is a Buffer or a string. Returns a Buffer (the .zip bytes).
function buildZip(entries) {
  const local = [];   // local file header + name + payload, per entry
  const central = [];  // central directory records
  let offset = 0;      // running offset of the next local header
  for (const e of entries) {
    const nameBuf = Buffer.from(e.name, 'utf8');
    const data = toBuf(e.data);
    const crc = crc32(data);
    const deflated = zlib.deflateRawSync(data);
    let method = 0, payload = data;
    if (deflated.length < data.length) { method = 8; payload = deflated; }

    const lh = Buffer.alloc(30);
    lh.writeUInt32LE(0x04034b50, 0);  // local file header signature
    lh.writeUInt16LE(20, 4);          // version needed
    lh.writeUInt16LE(0, 6);           // general purpose flags
    lh.writeUInt16LE(method, 8);      // compression method
    lh.writeUInt16LE(0, 10);          // mod time
    lh.writeUInt16LE(0x21, 12);       // mod date = 1980-01-01 (cosmetic)
    lh.writeUInt32LE(crc, 14);
    lh.writeUInt32LE(payload.length, 18);
    lh.writeUInt32LE(data.length, 22);
    lh.writeUInt16LE(nameBuf.length, 26);
    lh.writeUInt16LE(0, 28);          // extra length
    local.push(lh, nameBuf, payload);

    const ch = Buffer.alloc(46);
    ch.writeUInt32LE(0x02014b50, 0);  // central directory header signature
    ch.writeUInt16LE(20, 4);          // version made by
    ch.writeUInt16LE(20, 6);          // version needed
    ch.writeUInt16LE(0, 8);           // flags
    ch.writeUInt16LE(method, 10);
    ch.writeUInt16LE(0, 12);          // mod time
    ch.writeUInt16LE(0x21, 14);       // mod date
    ch.writeUInt32LE(crc, 16);
    ch.writeUInt32LE(payload.length, 20);
    ch.writeUInt32LE(data.length, 24);
    ch.writeUInt16LE(nameBuf.length, 28);
    ch.writeUInt16LE(0, 30);          // extra length
    ch.writeUInt16LE(0, 32);          // comment length
    ch.writeUInt16LE(0, 34);          // disk number start
    ch.writeUInt16LE(0, 36);          // internal attrs
    ch.writeUInt32LE(0, 38);          // external attrs
    ch.writeUInt32LE(offset, 42);     // relative offset of local header
    central.push(ch, nameBuf);

    offset += lh.length + nameBuf.length + payload.length;
  }
  const centralStart = offset;
  const centralBuf = Buffer.concat(central);
  const eocd = Buffer.alloc(22);
  eocd.writeUInt32LE(0x06054b50, 0);        // end of central directory signature
  eocd.writeUInt16LE(0, 4);                 // this disk
  eocd.writeUInt16LE(0, 6);                 // disk with central dir
  eocd.writeUInt16LE(entries.length, 8);    // entries on this disk
  eocd.writeUInt16LE(entries.length, 10);   // total entries
  eocd.writeUInt32LE(centralBuf.length, 12);
  eocd.writeUInt32LE(centralStart, 16);     // offset of start of central directory
  eocd.writeUInt16LE(0, 20);                // comment length
  return Buffer.concat([...local, centralBuf, eocd]);
}

// Parse a .zip Buffer → [{ name, data(Buffer) }]. Best-effort: unreadable/unsupported entries are
// skipped rather than throwing, so one bad member can't sink an otherwise-valid import.
function readZip(buf) {
  const out = [];
  if (!Buffer.isBuffer(buf) || buf.length < 22) return out;
  // Locate the End Of Central Directory record by scanning backwards for its signature.
  let eocd = -1;
  const minScan = Math.max(0, buf.length - 22 - 65535);
  for (let i = buf.length - 22; i >= minScan; i--) {
    if (buf.readUInt32LE(i) === 0x06054b50) { eocd = i; break; }
  }
  if (eocd < 0) return out;
  const count = buf.readUInt16LE(eocd + 10);
  let p = buf.readUInt32LE(eocd + 16);      // central directory offset
  for (let i = 0; i < count; i++) {
    if (p + 46 > buf.length || buf.readUInt32LE(p) !== 0x02014b50) break;
    const method = buf.readUInt16LE(p + 10);
    const compSize = buf.readUInt32LE(p + 20);
    const nameLen = buf.readUInt16LE(p + 28);
    const extraLen = buf.readUInt16LE(p + 30);
    const commentLen = buf.readUInt16LE(p + 32);
    const localOff = buf.readUInt32LE(p + 42);
    const name = buf.toString('utf8', p + 46, p + 46 + nameLen);
    // The local header repeats name/extra lengths; trust it for the data offset.
    if (localOff + 30 <= buf.length && buf.readUInt32LE(localOff) === 0x04034b50) {
      const lhName = buf.readUInt16LE(localOff + 26);
      const lhExtra = buf.readUInt16LE(localOff + 28);
      const dataStart = localOff + 30 + lhName + lhExtra;
      const dataEnd = dataStart + compSize;
      if (dataEnd <= buf.length) {
        const payload = buf.subarray(dataStart, dataEnd);
        let data = null;
        if (method === 0) data = Buffer.from(payload);
        else if (method === 8) { try { data = zlib.inflateRawSync(payload); } catch (_) { data = null; } }
        if (data) out.push({ name, data });
      }
    }
    p += 46 + nameLen + extraLen + commentLen;
  }
  return out;
}

const norm = (n) => String(n).replace(/\\/g, '/').replace(/^\.\//, '');
const inSubagents = (n) => norm(n).split('/').includes('subagents');
const depth = (n) => (norm(n).match(/\//g) || []).length;
const baseName = (n) => norm(n).split('/').filter(Boolean).pop() || '';

// Split a bundle's entries into { main, subagents } following the export layout: the main session
// is the shallowest top-level *.jsonl (never under a subagents/ segment); subagents are the
// agent-*.jsonl / agent-*.meta.json files under any subagents/ directory. Tolerant of an extra
// wrapping folder (e.g. a user who zipped the containing directory).
function splitBundle(entries) {
  let main = null;
  for (const e of entries) {
    if (!/\.jsonl$/i.test(e.name) || inSubagents(e.name)) continue;
    if (!main || depth(e.name) < depth(main.name)) main = { name: baseName(e.name), data: e.data };
  }
  const subagents = [];
  for (const e of entries) {
    if (!inSubagents(e.name)) continue;
    const base = baseName(e.name);
    if (/^agent-.*\.jsonl$/i.test(base) || /^agent-.*\.meta\.json$/i.test(base)) {
      subagents.push({ name: base, data: e.data });
    }
  }
  return { main, subagents };
}

module.exports = { buildZip, readZip, splitBundle, crc32 };
