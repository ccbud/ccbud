'use strict';
// Generates build/icon.png (1024x1024): gradient rounded square with a white "C" ring.
const fs = require('fs');
const path = require('path');
const zlib = require('zlib');

const S = 1024;
const buf = Buffer.alloc(S * S * 4);

function lerp(a, b, t) { return Math.round(a + (b - a) * t); }
const c1 = [109, 139, 255]; // --accent
const c2 = [79, 110, 240];  // --accent-2

const R = 180;          // corner radius
const cx = S / 2, cy = S / 2;
const rOuter = 330, rInner = 200;
const gapHalf = 0.62;   // radians, opening of the C on the right side

function inRoundedRect(x, y) {
  const minx = R, maxx = S - R, miny = R, maxy = S - R;
  if (x >= minx && x <= maxx) return y >= 0 && y < S ? (y >= 0) : false;
  return true;
}

for (let y = 0; y < S; y++) {
  for (let x = 0; x < S; x++) {
    const i = (y * S + x) * 4;
    // rounded-rect mask
    let inside = true;
    const dxl = R - x, dxr = x - (S - 1 - R), dyt = R - y, dyb = y - (S - 1 - R);
    if (x < R && y < R) inside = (R - x) ** 2 + (R - y) ** 2 <= R * R;
    else if (x > S - 1 - R && y < R) inside = (x - (S - 1 - R)) ** 2 + (R - y) ** 2 <= R * R;
    else if (x < R && y > S - 1 - R) inside = (R - x) ** 2 + (y - (S - 1 - R)) ** 2 <= R * R;
    else if (x > S - 1 - R && y > S - 1 - R) inside = (x - (S - 1 - R)) ** 2 + (y - (S - 1 - R)) ** 2 <= R * R;

    if (!inside) { buf[i] = 0; buf[i + 1] = 0; buf[i + 2] = 0; buf[i + 3] = 0; continue; }

    const t = (x + y) / (2 * S);
    let r = lerp(c1[0], c2[0], t);
    let g = lerp(c1[1], c2[1], t);
    let b = lerp(c1[2], c2[2], t);

    // white "C" ring
    const dx = x - cx, dy = y - cy;
    const rad = Math.sqrt(dx * dx + dy * dy);
    const ang = Math.atan2(dy, dx); // -pi..pi, 0 points right
    const inRing = rad >= rInner && rad <= rOuter;
    const inGap = Math.abs(ang) < gapHalf;
    if (inRing && !inGap) { r = 255; g = 255; b = 255; }

    buf[i] = r; buf[i + 1] = g; buf[i + 2] = b; buf[i + 3] = 255;
  }
}

// Encode PNG (RGBA, filter 0 per scanline)
const raw = Buffer.alloc(S * (S * 4 + 1));
for (let y = 0; y < S; y++) {
  raw[y * (S * 4 + 1)] = 0;
  buf.copy(raw, y * (S * 4 + 1) + 1, y * S * 4, (y + 1) * S * 4);
}
const idat = zlib.deflateSync(raw, { level: 9 });

function chunk(type, data) {
  const len = Buffer.alloc(4); len.writeUInt32BE(data.length, 0);
  const t = Buffer.from(type, 'ascii');
  const body = Buffer.concat([t, data]);
  const crc = Buffer.alloc(4); crc.writeUInt32BE(crc32(body) >>> 0, 0);
  return Buffer.concat([len, body, crc]);
}
function crc32(b) {
  let c = ~0;
  for (let i = 0; i < b.length; i++) {
    c ^= b[i];
    for (let k = 0; k < 8; k++) c = (c >>> 1) ^ (0xedb88320 & -(c & 1));
  }
  return ~c;
}
const ihdr = Buffer.alloc(13);
ihdr.writeUInt32BE(S, 0); ihdr.writeUInt32BE(S, 4);
ihdr[8] = 8; ihdr[9] = 6; ihdr[10] = 0; ihdr[11] = 0; ihdr[12] = 0;

const png = Buffer.concat([
  Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
  chunk('IHDR', ihdr),
  chunk('IDAT', idat),
  chunk('IEND', Buffer.alloc(0)),
]);

fs.writeFileSync(path.join(__dirname, 'icon.png'), png);
console.log('wrote build/icon.png', png.length, 'bytes');
