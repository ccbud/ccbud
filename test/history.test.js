'use strict';

const fs = require('fs');
const os = require('os');
const path = require('path');

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'ccbud-hist-'));
process.env.CCBUD_HISTORY_DIR = root;
const { createHistoryWatcher } = require('../src/main/history');

let pass = 0, fail = 0;
const check = (n, c, d) => { if (c) { pass++; console.log(`  \x1b[32mPASS\x1b[0m ${n}`); } else { fail++; console.log(`  \x1b[31mFAIL\x1b[0m ${n}${d ? ' — ' + d : ''}`); } };

const pdir = path.join(root, '-proj-x');
fs.mkdirSync(pdir, { recursive: true });
const file = path.join(pdir, 'ef0bc8c9-86f0-4ca6-b89d-000000000001.jsonl');
const L = (o) => JSON.stringify(o) + '\n';
fs.writeFileSync(file,
  // CLI plumbing — must be filtered out of the timeline & title
  L({ type: 'user', isMeta: true, sessionId: 's1', cwd: '/proj/x', uuid: 'u0', message: { role: 'user', content: '<command-name>/clear</command-name>' } }) +
  L({ type: 'user', sessionId: 's1', cwd: '/proj/x', gitBranch: 'main', uuid: 'u1', timestamp: '2026-06-18T00:00:00Z', message: { role: 'user', content: 'hello world' } }) +
  L({ type: 'assistant', sessionId: 's1', cwd: '/proj/x', gitBranch: 'main', uuid: 'a1', timestamp: '2026-06-18T00:00:01Z', message: { id: 'msg_1', role: 'assistant', model: 'glm-5.2', content: [{ type: 'thinking', thinking: 'hmm' }, { type: 'text', text: 'hi' }, { type: 'tool_use', id: 'tu1', name: 'Bash', input: { command: 'ls' } }], stop_reason: 'tool_use', usage: { input_tokens: 5, output_tokens: 2, cache_read_input_tokens: 3 } } }) +
  L({ type: 'user', sessionId: 's1', cwd: '/proj/x', uuid: 'u2', message: { role: 'user', content: [{ type: 'tool_result', tool_use_id: 'tu1', content: 'a.txt' }] } }) +
  L({ type: 'summary', summary: 'greeting session' }) +
  L({ type: 'file-history-snapshot', snapshot: {} }) +
  L({ type: 'progress', data: { type: 'bash_progress' } })
);

try {
  const w = createHistoryWatcher();

  // ---- listSessions ----
  const sessions = w.listSessions();
  check('listSessions finds 1 session', sessions.length === 1, `n=${sessions.length}`);
  check('session title from first NON-meta user', sessions[0].title === 'hello world', sessions[0].title);
  check('session cwd parsed', sessions[0].cwd === '/proj/x', sessions[0].cwd);
  check('session project = basename(cwd)', sessions[0].project === 'x', sessions[0].project);
  check('session id from sessionId', sessions[0].sessionId === 's1');
  check('session model from assistant', sessions[0].model === 'glm-5.2', sessions[0].model);

  // ---- listProjects (grouping) ----
  const projects = w.listProjects();
  check('listProjects groups into 1 project', projects.length === 1, `n=${projects.length}`);
  check('project name', projects[0].name === 'x', projects[0].name);
  check('project carries its sessions', projects[0].sessions.length === 1);

  // ---- getSession (rich parse) ----
  const s = w.getSession(file);
  check('getSession skips meta/summary/progress/snapshot', s.messages.length === 3, `n=${s.messages.length}`); // user + assistant + tool_result user
  const asst = s.messages[1];
  check('assistant has thinking+text+tool_use blocks', Array.isArray(asst.content) && asst.content.length === 3);
  check('assistant per-turn model attached', asst.modelActual === 'glm-5.2', asst.modelActual);
  check('assistant per-turn usage attached', asst.usage && asst.usage.inputTokens === 5 && asst.usage.outputTokens === 2, JSON.stringify(asst.usage));
  check('assistant stopReason attached', asst.stopReason === 'tool_use');
  check('tool_result kept in following user turn', JSON.stringify(s.messages[2].content).includes('tool_result'));
  check('totals: out=2 in=5 cacheRead=3 turns=1', s.meta.totals.out === 2 && s.meta.totals.in === 5 && s.meta.totals.cacheRead === 3 && s.meta.totals.turns === 1, JSON.stringify(s.meta.totals));
  check('meta model', s.meta.model === 'glm-5.2');
  check('meta summary captured', s.meta.summary === 'greeting session', s.meta.summary);
  check('meta project', s.meta.project === 'x');

  // ---- incremental tail → correlate + changed ----
  let corr = null, changed = null;
  w.on('correlate', (r) => { corr = r; });
  w.on('changed', (p) => { changed = p; });
  w.start(); // primes offsets to current size (skip backlog)
  fs.appendFileSync(file, L({ type: 'assistant', sessionId: 's1', cwd: '/proj/x', gitBranch: 'dev', uuid: 'a2', message: { id: 'msg_2', role: 'assistant', model: 'glm-5.2', content: [{ type: 'text', text: 'again' }] } }));
  w.tailNew(); // detect new bytes
  check('tail emits correlate for new assistant line', !!corr && corr.messageId === 'msg_2', JSON.stringify(corr));
  check('correlate carries cwd/branch/sessionId', corr && corr.cwd === '/proj/x' && corr.gitBranch === 'dev' && corr.sessionId === 's1');
  check('tail emits changed for the touched file', !!changed && changed.files && changed.files.indexOf(file) !== -1, JSON.stringify(changed));
  w.stop();
} finally {
  fs.rmSync(root, { recursive: true, force: true });
}

console.log(`\n${pass} passed, ${fail} failed`);
process.exit(fail ? 1 : 0);
