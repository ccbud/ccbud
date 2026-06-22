'use strict';
const fs = require('fs');
const path = require('path');
const zlib = require('zlib');

function dist(x1, y1, x2, y2) {
  const dx = x1 - x2, dy = y1 - y2;
  return Math.sqrt(dx * dx + dy * dy);
}

function distToLineSegment(x, y, x1, y1, x2, y2) {
  const dx = x2 - x1, dy = y2 - y1;
  const len = Math.sqrt(dx * dx + dy * dy);
  if (len < 0.001) return dist(x, y, x1, y1);
  const t = ((x - x1) * dx + (y - y1) * dy) / (len * len);
  const tc = Math.max(0, Math.min(1, t));
  return dist(x, y, x1 + tc * dx, y1 + tc * dy);
}

function insideShape(x, y, scale = 1) {
  const cx = 11 * scale;
  const lcx = 6.6 * scale;
  const rcx = 15.4 * scale;
  const cy = 13.5 * scale;
  const rOut = 3.3 * scale;
  const rIn = 2.1 * scale;

  // 1. Left 'c'
  const dl = dist(x, y, lcx, cy);
  const inLeftC = dl >= rIn && dl <= rOut && !(x > lcx && Math.abs(y - cy) < (x - lcx) * 0.7);

  // 2. Right 'c'
  const dr = dist(x, y, rcx, cy);
  const inRightC = dr >= rIn && dr <= rOut && !(x > rcx && Math.abs(y - cy) < (x - rcx) * 0.7);

  // 3. Sprout Stem (in between the 'c's)
  const stem = distToLineSegment(x, y, cx, 14 * scale, cx, 8.5 * scale) <= 0.6 * scale;

  // 4. Sprout Leaves
  const leftLeaf = distToLineSegment(x, y, cx, 8.5 * scale, 8.2 * scale, 5.0 * scale) <= 0.75 * scale;
  const rightLeaf = distToLineSegment(x, y, cx, 8.5 * scale, 13.8 * scale, 5.0 * scale) <= 0.75 * scale;

  return inLeftC || inRightC || stem || leftLeaf || rightLeaf;
}

function crc32(b) {
  let c = ~0;
  for (let i = 0; i < b.length; i++) {
    c ^= b[i];
    for (let k = 0; k < 8; k++) c = (c >>> 1) ^ (0xedb88320 & -(c & 1));
  }
  return ~c;
}

function chunk(type, data) {
  const len = Buffer.alloc(4); len.writeUInt32BE(data.length, 0);
  const t = Buffer.from(type, 'ascii');
  const body = Buffer.concat([t, data]);
  const crc = Buffer.alloc(4); crc.writeUInt32BE(crc32(body) >>> 0, 0);
  return Buffer.concat([len, body, crc]);
}

function generatePng(S, scale) {
  const buf = Buffer.alloc(S * S * 4);

  // Supersampling (8x8 subpixels per pixel)
  const SUB = 8;
  for (let y = 0; y < S; y++) {
    for (let x = 0; x < S; x++) {
      let insideCount = 0;
      for (let sy = 0; sy < SUB; sy++) {
        for (let sx = 0; sx < SUB; sx++) {
          const subX = x + (sx + 0.5) / SUB;
          const subY = y + (sy + 0.5) / SUB;
          if (insideShape(subX, subY, scale)) {
            insideCount++;
          }
        }
      }

      const alpha = Math.round((insideCount / (SUB * SUB)) * 255);
      const i = (y * S + x) * 4;
      // macOS template image: black color with alpha mask
      buf[i] = 0;
      buf[i + 1] = 0;
      buf[i + 2] = 0;
      buf[i + 3] = alpha;
    }
  }

  const raw = Buffer.alloc(S * (S * 4 + 1));
  for (let y = 0; y < S; y++) {
    raw[y * (S * 4 + 1)] = 0;
    buf.copy(raw, y * (S * 4 + 1) + 1, y * S * 4, (y + 1) * S * 4);
  }
  const idat = zlib.deflateSync(raw, { level: 9 });

  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(S, 0); ihdr.writeUInt32BE(S, 4);
  ihdr[8] = 8; ihdr[9] = 6; ihdr[10] = 0; ihdr[11] = 0; ihdr[12] = 0;

  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk('IHDR', ihdr),
    chunk('IDAT', idat),
    chunk('IEND', Buffer.alloc(0)),
  ]);
}

const out1x = path.join(__dirname, 'iconTemplate.png');
const out2x = path.join(__dirname, 'iconTemplate@2x.png');

fs.writeFileSync(out1x, generatePng(22, 1));
fs.writeFileSync(out2x, generatePng(44, 2));

// Copy to source folder
fs.copyFileSync(out1x, path.join(__dirname, '..', 'src', 'main', 'iconTemplate.png'));
fs.copyFileSync(out2x, path.join(__dirname, '..', 'src', 'main', 'iconTemplate@2x.png'));

console.log('Successfully generated minimalist template icons.');
