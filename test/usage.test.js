'use strict';

const fs = require('fs');
const os = require('os');
const path = require('path');
const { createUsageStore, formatTokens } = require('../src/main/usage');

let pass = 0, fail = 0;
const check = (n, c, d) => { if (c) { pass++; console.log(`  \x1b[32mPASS\x1b[0m ${n}`); } else { fail++; console.log(`  \x1b[31mFAIL\x1b[0m ${n}${d ? ' — ' + d : ''}`); } };

const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'clawdy-usage-'));
const DAY = 86400000;
const now = new Date(2026, 5, 18, 12, 0, 0).getTime(); // 2026-06-18 12:00 local
const at = (daysAgo, h = 12) => new Date(2026, 5, 18 - daysAgo, h, 0, 0).getTime();

try {
  const u = createUsageStore(dir);
  u.record({ ts: at(0), inputTokens: 100, outputTokens: 50, provider: 'GLM', requestedModel: 'A' });
  u.record({ ts: at(0), inputTokens: 10, outputTokens: 5, provider: 'GLM', requestedModel: 'B' });
  u.record({ ts: at(1), inputTokens: 200, outputTokens: 0, provider: 'GLM', requestedModel: 'A' });
  u.record({ ts: at(2), inputTokens: 100, outputTokens: 0, provider: 'DS', requestedModel: 'A' });
  u.record({ ts: at(8), inputTokens: 999, outputTokens: 0, provider: 'GLM', requestedModel: 'C' });

  const w = u.query('7d', now);
  check('7d total tokens = 465', w.tokens === 465, `got ${w.tokens}`);
  check('7d requests = 4', w.requests === 4, `got ${w.requests}`);
  check('7d active days = 3', w.activeDays === 3, `got ${w.activeDays}`);
  check('favorite model = A', w.favoriteModel === 'A', `got ${w.favoriteModel}`);
  check('peak hour = 12', w.peakHour === 12, `got ${w.peakHour}`);
  check('current streak = 3', w.currentStreak === 3, `got ${w.currentStreak}`);
  check('longest streak = 3', w.longestStreak === 3, `got ${w.longestStreak}`);
  check('byModel sorted, A first', w.byModel[0].model === 'A');

  const all = u.query('all', now);
  check('all total = 1464', all.tokens === 1464, `got ${all.tokens}`);
  check('rangeTokens 7d = 465', u.rangeTokens('7d', now) === 465);
  check('rangeTokens 1d = 165', u.rangeTokens('1d', now) === 165, `got ${u.rangeTokens('1d', now)}`);

  const todayCell = w.heatmap.find((c) => c.date === '2026-06-18');
  check('heatmap has today with tokens 165', !!todayCell && todayCell.tokens === 165, `got ${todayCell && todayCell.tokens}`);
  check('heatmap today level > 0', !!todayCell && todayCell.level > 0);

  check('formatTokens 950 = "950"', formatTokens(950) === '950');
  check('formatTokens 12345 = "12K"', formatTokens(12345) === '12K', formatTokens(12345));
  check('formatTokens 1.3e9 = "1.3B"', formatTokens(1.3e9) === '1.3B', formatTokens(1.3e9));
  check('formatTokens 65192 = "65K"', formatTokens(65192) === '65K', formatTokens(65192));
} finally {
  fs.rmSync(dir, { recursive: true, force: true });
}

console.log(`\n${pass} passed, ${fail} failed`);
process.exit(fail ? 1 : 0);
