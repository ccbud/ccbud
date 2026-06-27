'use strict';

// Per-conversation customization: custom title + user tags persisted as a `__ccbud__` field on the
// session file's first line (readCcbud on the read path, setCcbud on the write path).

const fs = require('fs');
const os = require('os');
const path = require('path');

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'ccbud-meta-'));
process.env.CCBUD_HISTORY_DIR = root;
const { createHistoryWatcher } = require('../src/main/history');

let pass = 0, fail = 0;
const check = (n, c, d) => { if (c) { pass++; console.log(`  \x1b[32mPASS\x1b[0m ${n}`); } else { fail++; console.log(`  \x1b[31mFAIL\x1b[0m ${n}${d ? ' — ' + d : ''}`); } };

const pdir = path.join(root, '-proj-x');
fs.mkdirSync(pdir, { recursive: true });
const file = path.join(pdir, 'ef0bc8c9-86f0-4ca6-b89d-0000000000aa.jsonl');
const L = (o) => JSON.stringify(o) + '\n';
// First line is a `mode` line carrying neither cwd nor sessionId-as-meta — exactly where setCcbud
// attaches __ccbud__ (mirrors the real Claude Code transcript shape).
fs.writeFileSync(file,
  L({ type: 'mode', mode: 'normal', sessionId: 's1' }) +
  L({ type: 'user', sessionId: 's1', cwd: '/proj/x', uuid: 'u1', timestamp: '2026-06-18T00:00:00Z', message: { role: 'user', content: 'hello world' } }) +
  L({ type: 'assistant', sessionId: 's1', cwd: '/proj/x', uuid: 'a1', timestamp: '2026-06-18T00:00:01Z', message: { id: 'msg_1', role: 'assistant', model: 'glm-5.2', content: [{ type: 'text', text: 'hi' }] } })
);

const firstLineObj = () => JSON.parse(fs.readFileSync(file, 'utf8').split('\n').find((s) => s.trim()));

try {
  const w = createHistoryWatcher();

  // ---- defaults before any customization ----
  let s = w.listSessions()[0];
  check('default title = auto (first user text)', s.title === 'hello world', s.title);
  check('default tags = []', Array.isArray(s.tags) && s.tags.length === 0, JSON.stringify(s.tags));
  check('autoTitle exposed', s.autoTitle === 'hello world', s.autoTitle);

  // ---- set custom title + tags (dup tag deduped) ----
  const r1 = w.setCcbud(file, { title: 'My Title', tags: ['alpha', 'beta', 'alpha'] });
  check('setCcbud ok', r1 && r1.ok === true, JSON.stringify(r1));
  const fl = firstLineObj();
  check('__ccbud__ written onto the FIRST line', !!fl.__ccbud__, JSON.stringify(fl).slice(0, 80));
  check('first line is still the mode line (merged, not replaced)', fl.type === 'mode' && fl.mode === 'normal');
  check('title persisted', fl.__ccbud__.title === 'My Title', JSON.stringify(fl.__ccbud__));
  check('tagList persisted + deduped', JSON.stringify(fl.__ccbud__.tagList) === JSON.stringify(['alpha', 'beta']), JSON.stringify(fl.__ccbud__.tagList));

  // ---- other transcript lines untouched ----
  const lines = fs.readFileSync(file, 'utf8').split('\n').filter((x) => x.trim());
  check('all 3 transcript lines intact', lines.length === 3, `n=${lines.length}`);
  check('user "hello world" line preserved', lines.some((x) => x.includes('"hello world"')));

  // ---- read path reflects the customization ----
  s = w.listSessions()[0];
  check('listSessions title overridden', s.title === 'My Title', s.title);
  check('listSessions tags reflected', JSON.stringify(s.tags) === JSON.stringify(['alpha', 'beta']), JSON.stringify(s.tags));
  check('listSessions autoTitle kept as original', s.autoTitle === 'hello world', s.autoTitle);

  const full = w.getSession(file);
  check('getSession title overridden', full.meta.title === 'My Title', full.meta.title);
  check('getSession tags reflected', JSON.stringify(full.meta.tags) === JSON.stringify(['alpha', 'beta']), JSON.stringify(full.meta.tags));
  check('getSession autoTitle kept', full.meta.autoTitle === 'hello world', full.meta.autoTitle);
  check('getSession transcript still parses (1 user + 1 assistant)', full.messages.length === 2, `n=${full.messages.length}`);

  // ---- clearing title reverts to auto, tags stay ----
  w.setCcbud(file, { title: '' });
  s = w.listSessions()[0];
  check('empty title clears override → auto title', s.title === 'hello world', s.title);
  check('tags untouched when only title patched', JSON.stringify(s.tags) === JSON.stringify(['alpha', 'beta']), JSON.stringify(s.tags));
  check('__ccbud__ retains tagList, drops title', !firstLineObj().__ccbud__.title && !!firstLineObj().__ccbud__.tagList);

  // ---- clearing tags removes __ccbud__ entirely ----
  w.setCcbud(file, { tags: [] });
  check('__ccbud__ removed when fully empty', !firstLineObj().__ccbud__, JSON.stringify(firstLineObj()).slice(0, 80));
  s = w.listSessions()[0];
  check('back to defaults', s.title === 'hello world' && s.tags.length === 0);

  // ---- write guard: refuse paths outside the configured projects dirs ----
  const out1 = w.setCcbud('/etc/hosts', { title: 'x' });
  check('guard blocks absolute out-of-scope path', out1 && out1.ok === false && out1.reason === 'out-of-scope', JSON.stringify(out1));
  const out2 = w.setCcbud(path.join(os.tmpdir(), 'nope-' + 'zzz', 'x.jsonl'), { title: 'x' });
  check('guard blocks sibling-temp path', out2 && out2.ok === false && out2.reason === 'out-of-scope', JSON.stringify(out2));
} catch (e) {
  fail++; console.log('  \x1b[31mFAIL\x1b[0m threw — ' + (e && e.stack || e));
}

console.log(`\n${pass} passed, ${fail} failed`);
process.exit(fail ? 1 : 0);
