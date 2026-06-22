'use strict';

/**
 * count_tokens fallback.
 *
 * Claude Code calls POST /v1/messages/count_tokens before sending. When the provider
 * implements it, the gateway forwards the real number; when it doesn't (404 / non-JSON /
 * unreachable), the gateway estimates locally (o200k + structural overhead, rounded up)
 * so context sizing keeps working — flagged via x-ccbud-tokens, never under-counted.
 */

const http = require('http');
const { createGateway } = require('../src/main/proxy');
const { estimateInputTokens, tokenizerReady } = require('../src/main/countTokens');

let pass = 0, fail = 0;
const check = (n, c, d) => { if (c) { pass++; console.log(`  \x1b[32mPASS\x1b[0m ${n}`); } else { fail++; console.log(`  \x1b[31mFAIL\x1b[0m ${n}${d ? ' — ' + d : ''}`); } };

// ---- unit: estimateInputTokens ---------------------------------------------
check('o200k tokenizer loaded (not the char fallback)', tokenizerReady());

const one = estimateInputTokens({ messages: [{ role: 'user', content: 'hello there, count my tokens' }] });
check('single message → positive estimate', one > 10, `got ${one}`);

const empty = estimateInputTokens({});
check('empty body → small positive (base overhead)', empty >= 1 && empty < 20, `got ${empty}`);

const noTools = estimateInputTokens({ messages: [{ role: 'user', content: 'hi' }] });
const withTools = estimateInputTokens({ messages: [{ role: 'user', content: 'hi' }], tools: [{ name: 'get_weather', description: 'Get weather', input_schema: { type: 'object', properties: { city: { type: 'string' } } } }] });
check('tools add overhead', withTools > noTools, `${withTools} vs ${noTools}`);

const withSys = estimateInputTokens({ system: 'You are a helpful assistant.', messages: [{ role: 'user', content: 'hi' }] });
check('system adds overhead', withSys > noTools, `${withSys} vs ${noTools}`);

const longer = estimateInputTokens({ messages: [{ role: 'user', content: 'word '.repeat(300) }] });
check('longer text → more tokens', longer > one * 5, `got ${longer}`);

const toolResult = estimateInputTokens({ messages: [{ role: 'user', content: [{ type: 'tool_result', content: 'the result text here' }] }] });
check('tool_result text is counted', toolResult > empty, `got ${toolResult}`);

// ---- integration: mock upstream --------------------------------------------
function startUpstream() {
  return new Promise((resolve) => {
    const srv = http.createServer((req, res) => {
      let b = ''; req.on('data', (c) => (b += c));
      req.on('end', () => {
        const path = req.url.replace(/\?.*$/, '');
        if (path.endsWith('/v1/messages/count_tokens')) {
          let body = {}; try { body = JSON.parse(b); } catch (_) {}
          const first = body.messages && body.messages[0] && body.messages[0].content;
          const txt = typeof first === 'string' ? first : '';
          if (txt.includes('REAL')) { res.writeHead(200, { 'content-type': 'application/json' }); res.end(JSON.stringify({ input_tokens: 42 })); return; }
          if (txt.includes('BADJSON')) { res.writeHead(200, { 'content-type': 'text/html' }); res.end('<html>not json</html>'); return; }
          res.writeHead(404, { 'content-type': 'application/json' }); res.end(JSON.stringify({ type: 'error', error: { message: 'not found' } })); return;
        }
        res.writeHead(200, { 'content-type': 'application/json' }); res.end('{}');
      });
    });
    srv.listen(0, '127.0.0.1', () => resolve(srv));
  });
}

function ct(base, content) {
  return fetch(base + '/v1/messages/count_tokens', {
    method: 'POST', headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ model: 'claude-opus-4-8', messages: [{ role: 'user', content }] }),
  }).then((r) => r.text().then((t) => ({
    status: r.status, tok: r.headers.get('x-ccbud-tokens'), up: r.headers.get('x-ccbud-upstream-status'),
    body: (() => { try { return JSON.parse(t); } catch (_) { return t; } })(),
  })));
}

(async () => {
  const up = await startUpstream();
  const upPort = up.address().port;
  const config = { activeProviderId: 'fake', providers: [{ id: 'fake', name: 'Fake', baseUrl: `http://127.0.0.1:${upPort}`, authToken: 'sk-x', defaultModel: 'up-model', smallFastModel: 'up-model', models: [] }] };
  const gateway = createGateway({ getConfig: () => config });
  const port = await gateway.start(0);
  const base = `http://127.0.0.1:${port}`;

  // 1) upstream 404 → estimate
  const a = await ct(base, 'please estimate the tokens for this sentence locally');
  check('404 → 200', a.status === 200, `status=${a.status}`);
  check('404 → x-ccbud-tokens: estimated', a.tok === 'estimated', `tok=${a.tok}`);
  check('404 → x-ccbud-upstream-status: 404', a.up === '404', `up=${a.up}`);
  check('404 → positive input_tokens', a.body && a.body.input_tokens > 0, JSON.stringify(a.body));

  // 2) upstream supports it → pass real value through
  const b = await ct(base, 'REAL — provider implements count_tokens');
  check('real → 200', b.status === 200);
  check('real → x-ccbud-tokens: upstream', b.tok === 'upstream', `tok=${b.tok}`);
  check('real → input_tokens passed through (42)', b.body && b.body.input_tokens === 42, JSON.stringify(b.body));

  // 3) upstream 200 but not valid count_tokens JSON → estimate
  const c = await ct(base, 'BADJSON — provider returns html');
  check('bad json → estimated', c.tok === 'estimated' && c.body.input_tokens > 0, `tok=${c.tok} body=${JSON.stringify(c.body)}`);

  // 4) provider unreachable → estimate (status: error)
  config.providers[0].baseUrl = 'http://127.0.0.1:1'; // nothing listening
  const d = await ct(base, 'estimate me even though the provider is down');
  check('unreachable → 200 estimated', d.status === 200 && d.tok === 'estimated', `status=${d.status} tok=${d.tok}`);
  check('unreachable → x-ccbud-upstream-status: error', d.up === 'error', `up=${d.up}`);
  check('unreachable → positive input_tokens', d.body && d.body.input_tokens > 0, JSON.stringify(d.body));

  await gateway.stop();
  up.close();
  console.log(`\n${pass} passed, ${fail} failed`);
  process.exit(fail ? 1 : 0);
})().catch((e) => { console.error('count-tokens test crashed:', e); process.exit(1); });
