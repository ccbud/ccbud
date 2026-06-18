'use strict';
// Generates build/icon.png (1024×1024): Clawdy gateway hub mark on indigo gradient squircle.
const fs = require('fs');
const path = require('path');
const zlib = require('zlib');

const S = 1024;
const buf = Buffer.alloc(S * S * 4);

function lerp(a, b, t) { return a + (b - a) * t; }
function lerp3(c1, c2, t) {
  return [
    Math.round(lerp(c1[0], c2[0], t)),
    Math.round(lerp(c1[1], c2[1], t)),
    Math.round(lerp(c1[2], c2[2], t)),
  ];
}

const gradA = [88, 86, 214];   // #5856D6
const gradB = [75, 107, 255];  // #4B6BFF
const gradC = [0, 122, 255];   // #007AFF

const R = 225;
const cx = S / 2;
const cy = S / 2;

function inRoundedRect(x, y) {
  if (x < R && y < R) return (R - x) ** 2 + (R - y) ** 2 <= R * R;
  if (x > S - 1 - R && y < R) return (x - (S - 1 - R)) ** 2 + (R - y) ** 2 <= R * R;
  if (x < R && y > S - 1 - R) return (R - x) ** 2 + (y - (S - 1 - R)) ** 2 <= R * R;
  if (x > S - 1 - R && y > S - 1 - R) return (x - (S - 1 - R)) ** 2 + (y - (S - 1 - R)) ** 2 <= R * R;
  return x >= 0 && x < S && y >= 0 && y < S;
}

function dist(x1, y1, x2, y2) {
  const dx = x1 - x2, dy = y1 - y2;
  return Math.sqrt(dx * dx + dy * dy);
}

function drawLine(x, y, x1, y1, x2, y2, w) {
  const dx = x2 - x1, dy = y2 - y1;
  const len = Math.sqrt(dx * dx + dy * dy);
  if (len < 1) return false;
  const t = ((x - x1) * dx + (y - y1) * dy) / (len * len);
  const tc = Math.max(0, Math.min(1, t));
  const px = x1 + tc * dx, py = y1 + tc * dy;
  return dist(x, y, px, py) <= w;
}

const hubR = 52;
const spokeR = 34;
const orbitR = 248;
const spokes = [
  { ax: 0, ay: -1 },
  { ax: 0.866, ay: 0.5 },
  { ax: -0.866, ay: 0.5 },
];

for (let y = 0; y < S; y++) {
  for (let x = 0; x < S; x++) {
    const i = (y * S + x) * 4;
    if (!inRoundedRect(x, y)) {
      buf[i] = 0; buf[i + 1] = 0; buf[i + 2] = 0; buf[i + 3] = 0;
      continue;
    }

    const t = (x / S * 0.55 + y / S * 0.45);
    let [r, g, b] = t < 0.5
      ? lerp3(gradA, gradB, t * 2)
      : lerp3(gradB, gradC, (t - 0.5) * 2);

    // Subtle top-edge highlight (glass depth)
    const edge = Math.min(x, y, S - 1 - x, S - 1 - y);
    if (edge < 40) {
      const boost = (40 - edge) / 40 * 0.08;
      r = Math.min(255, r + 255 * boost);
      g = Math.min(255, g + 255 * boost);
      b = Math.min(255, b + 255 * boost);
    }

    // Gateway hub mark (white)
    let mark = 0;
    const dHub = dist(x, y, cx, cy);
    if (dHub <= hubR) mark = 1;

    for (const s of spokes) {
      const nx = cx + s.ax * orbitR;
      const ny = cy + s.ay * orbitR;
      if (dist(x, y, nx, ny) <= spokeR) mark = 1;
      if (drawLine(x, y, cx, cy, nx, ny, 22)) mark = Math.max(mark, 0.72);
    }

    // Cardinal micro-lines from hub
    const cardinals = [[0, -hubR - 18, 0, -hubR - 52], [0, hubR + 18, 0, hubR + 52],
      [-hubR - 18, 0, -hubR - 52, 0], [hubR + 18, 0, hubR + 52, 0]];
    for (const [ox, oy, ex, ey] of cardinals) {
      if (drawLine(x, y, cx + ox, cy + oy, cx + ex, cy + ey, 14)) mark = Math.max(mark, 0.5);
    }

    if (mark > 0) {
      const w = Math.round(255 * mark);
      r = w; g = w; b = w;
    }

    buf[i] = r; buf[i + 1] = g; buf[i + 2] = b; buf[i + 3] = 255;
  }
}

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

const out = path.join(__dirname, 'icon.png');
fs.writeFileSync(out, png);
fs.copyFileSync(out, path.join(__dirname, '..', 'src', 'main', 'icon.png'));
console.log('wrote', out, png.length, 'bytes');