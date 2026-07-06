'use strict';

const fs = require('fs');
const os = require('os');
const path = require('path');
const { createInsights } = require('../src/main/insights');

let pass = 0, fail = 0;
const check = (n, c, d) => { if (c) { pass++; console.log(`  \x1b[32mPASS\x1b[0m ${n}`); } else { fail++; console.log(`  \x1b[31mFAIL\x1b[0m ${n}${d ? ' — ' + d : ''}`); } };

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'ccbud-ins-'));
const today = new Date(); today.setHours(12, 0, 0, 0);
const iso = today.toISOString();

function asst(id, model, u) {
  return JSON.stringify({ type: 'assistant', timestamp: iso, message: { id, role: 'assistant', model, usage: u } }) + '\n';
}
function user(text) { return JSON.stringify({ type: 'user', timestamp: iso, message: { role: 'user', content: text } }) + '\n'; }

// dirA/projects/-proj-x/s1.jsonl
const projA = path.join(root, 'dirA', 'projects', '-proj-x');
fs.mkdirSync(projA, { recursive: true });
fs.writeFileSync(path.join(projA, 's1.jsonl'),
  user('hi') +
  asst('m1', 'glm-5.2', { input_tokens: 10, output_tokens: 5 }) +
  asst('m2', 'glm-5.2', { input_tokens: 10, output_tokens: 5 })
);

// dirB/projects/-proj-y/s2.jsonl — includes a DUPLICATE m1 (must be de-duped) + big cache read
const projB = path.join(root, 'dirB', 'projects', '-proj-y');
fs.mkdirSync(projB, { recursive: true });
fs.writeFileSync(path.join(projB, 's2.jsonl'),
  asst('m1', 'glm-5.2', { input_tokens: 10, output_tokens: 5 }) + // duplicate id → ignored
  asst('m3', 'claude-opus-4-8', { input_tokens: 100, output_tokens: 50, cache_read_input_tokens: 1000 })
);

const dirsA = path.join(root, 'dirA', 'projects');
const dirsB = path.join(root, 'dirB', 'projects');
let active = [dirsA, dirsB];
const ins = createInsights({ getDirs: () => active });

(async () => {
  try {
    const all = await ins.query('all');
    check('requests deduped across dirs (m1 once)', all.requests === 3, `req=${all.requests}`);
    check('total tokens incl cache', all.tokens === 30 + 1150, `tokens=${all.tokens}`); // (15+15) + (100+50+1000)
    check('input/output summed', all.input === 120 && all.output === 60, `in=${all.input} out=${all.output}`);
    check('cacheRead summed', all.cacheRead === 1000, `cr=${all.cacheRead}`);
    const byModel = Object.fromEntries(all.byModel.map((m) => [m.model, m.tokens]));
    check('byModel glm-5.2 = 30', byModel['glm-5.2'] === 30, JSON.stringify(byModel));
    check('byModel claude-opus-4-8 = 1150', byModel['claude-opus-4-8'] === 1150);
    check('favoriteModel = biggest', all.favoriteModel === 'claude-opus-4-8', all.favoriteModel);
    check('heatmap present + today lit', all.heatmap.length > 0 && all.heatmap.some((c) => c.tokens > 0));
    check('activeDays = 1', all.activeDays === 1, `days=${all.activeDays}`);
    check('rangeTokens(all) matches', (await ins.rangeTokens('all')) === 1180);

    // switch active to only dirA → recompute reflects the smaller set
    active = [dirsA];
    ins.invalidate();
    const aOnly = await ins.query('all');
    check('active filter: only dirA → 2 requests', aOnly.requests === 2, `req=${aOnly.requests}`);
    check('active filter: only dirA → 30 tokens', aOnly.tokens === 30, `tokens=${aOnly.tokens}`);

    // back to both — a real active-dir switch always invalidates (historySetActive does this);
    // without it the short-TTL memo intentionally returns the prior result.
    active = [dirsA, dirsB];
    ins.invalidate();
    const both = await ins.query('7d');
    check('switch back → 3 requests again', both.requests === 3, `req=${both.requests}`);
    check('memo serves repeat query within TTL', (await ins.query('7d')).requests === 3);

    // ---- undated record must bucket by file mtime, NOT "today" (the CRITICAL fix) ----
    const projC = path.join(root, 'dirC', 'projects', '-proj-z');
    fs.mkdirSync(projC, { recursive: true });
    const cFile = path.join(projC, 's3.jsonl');
    fs.writeFileSync(cFile, JSON.stringify({ type: 'assistant', message: { id: 'mz', role: 'assistant', model: 'glm', usage: { input_tokens: 7, output_tokens: 3 } } }) + '\n'); // NO timestamp
    const tenDaysAgo = (Date.now() - 10 * 86400000) / 1000;
    fs.utimesSync(cFile, tenDaysAgo, tenDaysAgo);
    const insC = createInsights({ getDirs: () => [path.join(root, 'dirC', 'projects')] });
    check('undated record still counted (all)', (await insC.query('all')).tokens === 10, `tokens=${(await insC.query('all')).tokens}`);
    insC.invalidate();
    check('undated record NOT misbucketed into today (1d excludes it)', (await insC.query('1d')).tokens === 0, `1d=${(await insC.query('1d')).tokens}`);

    // ---- per-session subagent transcripts + Codex rollouts (dirD) ----
    const rootD = path.join(root, 'dirD');
    const projD = path.join(rootD, 'projects', '-proj-w');
    fs.mkdirSync(projD, { recursive: true });
    fs.writeFileSync(path.join(projD, 's4.jsonl'), asst('m4', 'glm-5.2', { input_tokens: 20, output_tokens: 10 }));
    // subagent transcripts live one level deeper, per session: <proj>/<session>/subagents/
    const subD = path.join(projD, 's4', 'subagents');
    fs.mkdirSync(subD, { recursive: true });
    fs.writeFileSync(path.join(subD, 'agent-a.jsonl'), asst('m5', 'glm-5.2', { input_tokens: 30, output_tokens: 3 }));
    // codex rollout: model from turn_context; cached input split out; info-null line skipped
    const cxDay = path.join(rootD, 'sessions', '2026', '07', '01');
    fs.mkdirSync(cxDay, { recursive: true });
    const L = (type, payload) => JSON.stringify({ timestamp: iso, type, payload }) + '\n';
    fs.writeFileSync(path.join(cxDay, 'rollout-2026-07-01T12-00-00-x.jsonl'),
      L('session_meta', { id: 's' }) +
      L('turn_context', { cwd: '/tmp', model: 'gpt-5.5' }) +
      L('event_msg', { type: 'token_count', info: { last_token_usage: { input_tokens: 900, cached_input_tokens: 600, output_tokens: 80, total_tokens: 980 } } }) +
      L('event_msg', { type: 'token_count', info: null })
    );
    const insD = createInsights({
      getDirs: () => [path.join(rootD, 'projects')],
      getSessionDirs: () => [path.join(rootD, 'sessions')],
    });
    const d = await insD.query('all');
    check('session + subagent + codex all counted (3 requests)', d.requests === 3, `req=${d.requests}`);
    check('subagent at session depth counted', d.tokens === 30 + 33 + 980, `tokens=${d.tokens}`);
    check('codex cached input split out', d.cacheRead === 600 && d.input === 20 + 30 + 300, `cr=${d.cacheRead} in=${d.input}`);
    const byModelD = Object.fromEntries(d.byModel.map((m) => [m.model, m.tokens]));
    check('codex model from turn_context', byModelD['gpt-5.5'] === 980, JSON.stringify(byModelD));
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }

  console.log(`\n${pass} passed, ${fail} failed`);
  process.exit(fail ? 1 : 0);
})().catch((e) => {
  console.error(e);
  process.exit(1);
});
