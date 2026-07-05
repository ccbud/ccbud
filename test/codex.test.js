'use strict';

// Codex rollout support: normalization (rollout jsonl → renderer message model), the history
// pipeline wiring (list/get/setMeta via sidecar), and the HTML export routing.
// Fixture mirrors src-tauri/src/codex.rs tests so the two implementations stay in lockstep.

const fs = require('fs');
const os = require('os');
const path = require('path');

// A codex "work dir" is a plain config dir whose data lives in <dir>/sessions (like ~/.codex).
const codexBase = fs.mkdtempSync(path.join(os.tmpdir(), 'ccbud-codex-'));
const codexRoot = path.join(codexBase, 'sessions');
const ccbudHome = fs.mkdtempSync(path.join(os.tmpdir(), 'ccbud-home-'));
const claudeRoot = fs.mkdtempSync(path.join(os.tmpdir(), 'ccbud-hist-'));
process.env.CCBUD_CODEX_DIR = codexRoot;
process.env.CCBUD_HOME = ccbudHome;
process.env.CCBUD_HISTORY_DIR = claudeRoot;

const codex = require('../src/main/codex');
const { createHistoryWatcher, firstUserText } = require('../src/main/history');
const exportHtml = require('../src/main/exportHtml');

let pass = 0, fail = 0;
const check = (n, c, d) => { if (c) { pass++; console.log(`  \x1b[32mPASS\x1b[0m ${n}`); } else { fail++; console.log(`  \x1b[31mFAIL\x1b[0m ${n}${d ? ' — ' + d : ''}`); } };

const L = (ts, type, payload) => JSON.stringify({ timestamp: ts, type, payload }) + '\n';
const dayDir = path.join(codexRoot, '2026', '07', '04');
fs.mkdirSync(dayDir, { recursive: true });
const file = path.join(dayDir, 'rollout-2026-07-04T15-13-07-019f-abc.jsonl');
fs.writeFileSync(file,
  L('2026-07-04T07:13:08.965Z', 'session_meta', {
    session_id: '019f-abc', id: '019f-abc', timestamp: '2026-07-04T07:13:07.386Z',
    cwd: '/tmp/projx', originator: 'codex-tui', cli_version: '0.142.5', git: { branch: 'main' },
  }) +
  L('2026-07-04T07:13:08.967Z', 'turn_context', { cwd: '/tmp/projx', model: 'gpt-5.5' }) +
  L('2026-07-04T07:13:08.967Z', 'response_item', {
    type: 'message', role: 'user',
    content: [{ type: 'input_text', text: '<environment_context>\n<cwd>/tmp/projx</cwd>\n</environment_context>' }],
  }) +
  L('2026-07-04T07:13:08.969Z', 'response_item', {
    type: 'message', role: 'user', content: [{ type: 'input_text', text: 'fix the bug please' }],
  }) +
  L('2026-07-04T07:13:09.100Z', 'event_msg', { type: 'user_message', message: 'fix the bug please' }) +
  L('2026-07-04T07:13:10.000Z', 'response_item', {
    type: 'reasoning', summary: [{ type: 'summary_text', text: 'Looking at the repo' }], encrypted_content: 'xxx',
  }) +
  L('2026-07-04T07:13:11.000Z', 'response_item', {
    type: 'function_call', name: 'exec_command',
    arguments: '{"cmd": "ls -la", "yield_time_ms": 10000}', call_id: 'call_1',
  }) +
  L('2026-07-04T07:13:12.000Z', 'response_item', {
    type: 'function_call_output', call_id: 'call_1',
    output: 'Chunk ID: x\nWall time: 0.1 seconds\nProcess exited with code 0\nOutput:\n---\na.txt\nb.txt',
  }) +
  L('2026-07-04T07:13:13.000Z', 'response_item', {
    type: 'function_call', name: 'shell',
    arguments: '{"command": ["bash", "-lc", "cargo test"], "workdir": "/tmp/projx"}', call_id: 'call_2',
  }) +
  L('2026-07-04T07:13:14.000Z', 'response_item', {
    type: 'function_call_output', call_id: 'call_2',
    output: '{"output": "error: it broke", "metadata": {"exit_code": 101, "duration_seconds": 1.5}}',
  }) +
  L('2026-07-04T07:13:15.000Z', 'response_item', {
    type: 'function_call', name: 'update_plan',
    arguments: '{"plan": [{"step": "read code", "status": "completed"}, {"step": "fix bug", "status": "in_progress"}]}',
    call_id: 'call_3',
  }) +
  L('2026-07-04T07:13:16.000Z', 'response_item', {
    type: 'custom_tool_call', name: 'apply_patch', call_id: 'call_4',
    input: '*** Begin Patch\n*** Update File: src/a.rs\n@@\n-old\n+new\n*** End Patch',
  }) +
  L('2026-07-04T07:13:17.000Z', 'response_item', {
    type: 'message', role: 'assistant', content: [{ type: 'output_text', text: 'Done — fixed.' }], phase: 'final_answer',
  }) +
  L('2026-07-04T07:13:17.500Z', 'event_msg', {
    type: 'token_count',
    info: {
      total_token_usage: { input_tokens: 900, cached_input_tokens: 600, output_tokens: 80, total_tokens: 980 },
      last_token_usage: { input_tokens: 900, cached_input_tokens: 600, output_tokens: 80, total_tokens: 980 },
      model_context_window: 258400,
    },
  })
);

try {
  // ---- sniff ----
  const recs = fs.readFileSync(file, 'utf8').split('\n').filter(Boolean).map((l) => JSON.parse(l));
  check('looksCodex on rollout records', codex.looksCodex(recs));
  check('claude records do not sniff as codex', !codex.looksCodex([
    { type: 'user', message: { role: 'user', content: 'hi' }, cwd: '/x', sessionId: 's1' },
    { type: 'assistant', message: { role: 'assistant', content: [{ type: 'text', text: 'hello' }] } },
  ]));

  // ---- normalize ----
  const n = codex.normalize(recs);
  check('session ids from session_meta', n.sessionId === '019f-abc' && n.cwd === '/tmp/projx' && n.version === '0.142.5' && n.gitBranch === 'main');
  check('model from turn_context', n.model === 'gpt-5.5');
  const roles = n.messages.map((m) => m.role).join(',');
  check('env-context turn skipped; roles sequence', roles === 'user,assistant,assistant,user,assistant,user,assistant,assistant,assistant', roles);
  check('title from first prose turn', firstUserText(n.messages) === 'fix the bug please', firstUserText(n.messages));
  const tu1 = n.messages[2].content[0];
  check('exec_command → Bash', tu1.type === 'tool_use' && tu1.name === 'Bash' && tu1.input.command === 'ls -la');
  const tr1 = n.messages[3].content[0];
  check('exit 0 output not an error', tr1.tool_use_id === 'call_1' && !tr1.is_error);
  const tu2 = n.messages[4].content[0];
  check('shell argv unwraps bash -lc', tu2.input.command === 'cargo test', tu2.input.command);
  const tr2 = n.messages[5].content[0];
  check('exit 101 JSON output → error + unwrapped text', tr2.is_error === true && tr2.content === 'error: it broke');
  const tu3 = n.messages[6].content[0];
  check('update_plan → TodoWrite', tu3.name === 'TodoWrite' && tu3.input.todos[1].status === 'in_progress');
  const tu4 = n.messages[7].content[0];
  check('apply_patch → ApplyPatch', tu4.name === 'ApplyPatch' && tu4.input.patch.includes('*** Update File: src/a.rs'));
  check('reasoning → thinking block', n.messages[1].content[0].type === 'thinking');
  const last = n.messages[n.messages.length - 1];
  check('token_count rides last assistant turn', last.usage && last.usage.inputTokens === 300 && last.usage.cacheRead === 600);
  check('totals accumulate', n.totals.out === 80 && n.totals.turns === 1);
  check('message timestamps span', n.firstTs === '2026-07-04T07:13:08.969Z' && n.lastTs === '2026-07-04T07:13:17.000Z');

  // ---- history pipeline wiring: the codex dir is just another configured work dir, whose
  // sessions/ tree is walked next to the (absent) projects/ tree ----
  check('codexLabel is the plain config-dir name', codex.codexLabel() === codexBase, codex.codexLabel());
  const dm = { id: codexBase, label: codexBase, configDir: codexBase, projectsDir: path.join(codexBase, 'projects') };
  const w = createHistoryWatcher({ getDirs: () => [
    { id: 'default', label: '~/.claude', configDir: path.dirname(claudeRoot), projectsDir: claudeRoot },
    dm,
  ] });
  const sessions = w.listSessions();
  check('listSessions finds the codex session', sessions.length === 1, `n=${sessions.length}`);
  const s0 = sessions[0] || {};
  check('list row: source/dir fields', s0.source === 'codex' && s0.dirId === codexBase && s0.imported === false, JSON.stringify({ source: s0.source, dirId: s0.dirId }));
  const stats = w.dirStats();
  const codexStat = stats.find((d) => d.id === codexBase) || {};
  check('dirStats: sessions-only dir counts + exists', codexStat.sessions === 1 && codexStat.exists === true, JSON.stringify(codexStat));
  check('list row: title/project/model', s0.title === 'fix the bug please' && s0.project === 'projx' && s0.model === 'gpt-5.5');
  const projects = w.listProjects();
  check('listProjects groups codex session by cwd', projects.length === 1 && projects[0].name === 'projx');
  const det = w.getSession(file);
  check('getSession routes to codex shaper', det && det.meta.assistant === 'Codex' && det.meta.source === 'codex');
  check('getSession messages + no subagents', det.messages.length === 9 && Object.keys(det.subagents).length === 0);

  // ---- sidecar title/tags/delete (never rewrites the rollout) ----
  const before = fs.readFileSync(file, 'utf8');
  const r1 = w.setCcbud(file, { title: '我的 Codex 会话', tags: ['exp', 'exp', ' b '] });
  check('setCcbud on codex ok', r1 && r1.ok === true, JSON.stringify(r1));
  check('rollout file untouched by setCcbud', fs.readFileSync(file, 'utf8') === before);
  check('sidecar file written', fs.existsSync(path.join(ccbudHome, 'codex-meta.json')));
  const s1 = w.listSessions()[0];
  check('custom title + deduped tags surface', s1.title === '我的 Codex 会话' && s1.tags.join(',') === 'exp,b', JSON.stringify(s1.tags));
  const r2 = w.setCcbud(file, { delete: true });
  check('soft-delete flag persists to sidecar', r2.ok === true && w.listSessions()[0].deleted === true);
  w.setCcbud(file, { delete: false });
  check('restore clears the flag', w.listSessions()[0].deleted === false);
  w.setCcbud(file, { title: '' });
  check('empty title reverts to auto', w.listSessions()[0].title === 'fix the bug please');

  // ---- HTML export routing ----
  const data = exportHtml.buildData(file);
  check('export data routes codex', data.meta.assistant === 'Codex' && data.meta.model === 'gpt-5.5' && data.messages.length === 9);
  const html = exportHtml.htmlFromData(data);
  check('export html embeds conversation', html.includes('__CONV__') && html.includes('Codex') && html.includes('fix the bug please'));

  // ---- old envelope-less format ----
  const old = [
    { id: 'old-1', timestamp: '2025-05-01T00:00:00Z', instructions: 'x', cwd: '/tmp/old' },
    { type: 'message', role: 'user', content: [{ type: 'input_text', text: 'hello old codex' }] },
    { type: 'message', role: 'assistant', content: [{ type: 'output_text', text: 'hi' }] },
  ];
  check('old format sniffs as codex', codex.looksCodex(old));
  const no = codex.normalize(old);
  check('old format normalizes', no.sessionId === 'old-1' && no.messages.length === 2 && firstUserText(no.messages) === 'hello old codex');
} catch (e) {
  fail++;
  console.log('  \x1b[31mFAIL\x1b[0m exception: ' + (e && e.stack || e));
}

console.log(`\ncodex: ${pass} passed, ${fail} failed`);
process.exit(fail ? 1 : 0);
