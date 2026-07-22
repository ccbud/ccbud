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

  // ---- subagents embedded into getSession (nested under the spawning Task tool_use id) ----
  const subDir = path.join(pdir, path.basename(file, '.jsonl'), 'subagents');
  fs.mkdirSync(subDir, { recursive: true });
  fs.writeFileSync(path.join(subDir, 'agent-aaa.jsonl'),
    L({ type: 'user', isSidechain: true, sessionId: 's1', agentId: 'aaa', uuid: 'su1', timestamp: '2026-06-18T00:00:02Z', message: { role: 'user', content: 'do a thing' } }) +
    L({ type: 'assistant', isSidechain: true, sessionId: 's1', agentId: 'aaa', uuid: 'sa1', timestamp: '2026-06-18T00:00:03Z', message: { id: 'msg_s1', role: 'assistant', model: 'glm-5.2', content: [{ type: 'text', text: 'done' }], usage: { input_tokens: 1, output_tokens: 4 } } })
  );
  fs.writeFileSync(path.join(subDir, 'agent-aaa.meta.json'), JSON.stringify({ agentType: 'general-purpose', description: 'thing doer', toolUseId: 'tu1' }));

  const s2 = w.getSession(file);
  check('getSession returns a subagents map', s2.subagents && typeof s2.subagents === 'object');
  check('subagent keyed by spawning toolUseId (tu1)', !!(s2.subagents && s2.subagents.tu1), Object.keys(s2.subagents || {}).join(','));
  const sa = s2.subagents && s2.subagents.tu1;
  check('subagent carries type + description', !!sa && sa.type === 'general-purpose' && sa.description === 'thing doer', JSON.stringify(sa && { t: sa.type, d: sa.description }));
  check('subagent messages shaped (2, last = "done")', !!sa && sa.messages.length === 2 && sa.messages[1].content[0].text === 'done', JSON.stringify(sa && sa.messages.length));
  check('subagent totals rolled up (in=1 out=4)', !!sa && sa.totals.out === 4 && sa.totals.in === 1, JSON.stringify(sa && sa.totals));
  check('meta.subagentCount = 1', s2.meta.subagentCount === 1, String(s2.meta.subagentCount));

  // ---- readSubagentFiles (bundle export) + subagentTranscriptPaths ("Claude 分析" multi-attach) ----
  const { readSubagentFiles, subagentTranscriptPaths } = require('../src/main/history');
  const subFiles = readSubagentFiles(file);
  check('readSubagentFiles returns jsonl + meta (2)', subFiles.length === 2, `n=${subFiles.length}`);
  check('readSubagentFiles names are agent-aaa.*', subFiles.every((f) => /^agent-aaa\.(jsonl|meta\.json)$/.test(f.name)));
  check('readSubagentFiles data is a Buffer', subFiles.every((f) => Buffer.isBuffer(f.data)));
  const subPaths = subagentTranscriptPaths(file);
  check('subagentTranscriptPaths returns only the agent-*.jsonl (1)', subPaths.length === 1, `n=${subPaths.length}`);
  check('subagentTranscriptPaths excludes the .meta.json sidecar', subPaths.every((p) => /agent-aaa\.jsonl$/.test(p)));
  check('subagentTranscriptPaths are absolute paths', subPaths.every((p) => p.startsWith(subDir)));

  // ---- skill attribution (spawning Skill tool_use, with the transcript sentinel as fallback) ----
  const file2 = path.join(pdir, 'ef0bc8c9-86f0-4ca6-b89d-000000000002.jsonl');
  fs.writeFileSync(file2,
    L({ type: 'user', sessionId: 'sk1', cwd: '/proj/x', uuid: 'v1', timestamp: '2026-06-19T00:00:00Z', message: { role: 'user', content: 'run the research skill' } }) +
    L({ type: 'assistant', sessionId: 'sk1', cwd: '/proj/x', uuid: 'v2', timestamp: '2026-06-19T00:00:01Z', message: { id: 'msg_k1', role: 'assistant', model: 'glm-5.2', content: [{ type: 'tool_use', id: 'tu9', name: 'Skill', input: { skill: 'deep-research', args: 'topic' } }], usage: { input_tokens: 2, output_tokens: 1 } } }) +
    L({ type: 'user', sessionId: 'sk1', cwd: '/proj/x', uuid: 'v3', message: { role: 'user', content: [{ type: 'tool_result', tool_use_id: 'tu9', content: 'done' }] }, toolUseResult: { agentId: 'bbb' } })
  );
  const subDir2 = path.join(pdir, path.basename(file2, '.jsonl'), 'subagents');
  fs.mkdirSync(subDir2, { recursive: true });
  // Sentinel says "dr-dir" but the spawning tool_use says "deep-research" — the call site wins.
  fs.writeFileSync(path.join(subDir2, 'agent-bbb.jsonl'),
    L({ type: 'user', isSidechain: true, sessionId: 'sk1', agentId: 'bbb', uuid: 'sb1', timestamp: '2026-06-19T00:00:02Z', message: { role: 'user', content: 'Base directory for this skill: /home/u/.claude/skills/dr-dir\n\ndo the research' } }) +
    L({ type: 'assistant', isSidechain: true, sessionId: 'sk1', agentId: 'bbb', uuid: 'sb2', timestamp: '2026-06-19T00:00:03Z', message: { id: 'msg_k2', role: 'assistant', model: 'glm-5.2', content: [{ type: 'text', text: 'ok' }], usage: { input_tokens: 1, output_tokens: 1 } } })
  );
  fs.writeFileSync(path.join(subDir2, 'agent-bbb.meta.json'), JSON.stringify({ agentType: 'general-purpose', description: 'skill runner', toolUseId: 'tu9' }));
  // No meta.json and no matching tool_use — only the sentinel (block content + Windows path) names it.
  fs.writeFileSync(path.join(subDir2, 'agent-ccc.jsonl'),
    L({ type: 'user', isSidechain: true, sessionId: 'sk1', agentId: 'ccc', uuid: 'sc1', timestamp: '2026-06-19T00:00:04Z', message: { role: 'user', content: [{ type: 'text', text: 'Base directory for this skill: C:\\Users\\u\\.claude\\skills\\pdf' }] } })
  );

  const s3 = w.getSession(file2);
  check('skill named by the spawning Skill tool_use (overrides sentinel)', !!s3.subagents.tu9 && s3.subagents.tu9.skill === 'deep-research', JSON.stringify(s3.subagents.tu9 && s3.subagents.tu9.skill));
  check('skill sentinel fallback when no Skill call resolves', !!s3.subagents['agent:ccc'] && s3.subagents['agent:ccc'].skill === 'pdf', JSON.stringify(s3.subagents['agent:ccc'] && s3.subagents['agent:ccc'].skill));
  check('plain Task subagent carries no skill', s2.subagents.tu1.skill == null, JSON.stringify(s2.subagents.tu1.skill));
  check('main session meta carries no skill', s3.meta.skill == null, JSON.stringify(s3.meta.skill));
  const s4 = w.getSession(path.join(subDir2, 'agent-bbb.jsonl'));
  check('standalone subagent transcript self-reports its skill via sentinel', s4.meta.isSubagent === true && s4.meta.skill === 'dr-dir', JSON.stringify(s4.meta.skill));

  // HTML export embeds the same attribution (exportHtml.js mirrors history.js).
  const exp = require('../src/main/exportHtml').buildData(file2);
  check('export data carries subagent skill', !!exp.subagents.tu9 && exp.subagents.tu9.skill === 'deep-research' && exp.subagents['agent:ccc'].skill === 'pdf', JSON.stringify(exp.subagents.tu9 && exp.subagents.tu9.skill));
} finally {
  fs.rmSync(root, { recursive: true, force: true });
}

console.log(`\n${pass} passed, ${fail} failed`);
process.exit(fail ? 1 : 0);
