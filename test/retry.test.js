'use strict';

/**
 * Issue #13 — upstream 429 auto-retry. The gateway should swallow a few 429s (low-concurrency
 * providers) and retry before surfacing the rate-limit to the client; once attempts run out the
 * 429 passes through honestly. Also covers the pure retryDelay() backoff/Retry-After helper.
 */

const http = require('http');
const { createGateway, retryDelay } = require('../src/main/proxy');

let pass = 0, fail = 0;
const check = (n, c, d) => { if (c) { pass++; console.log(`  \x1b[32mPASS\x1b[0m ${n}`); } else { fail++; console.log(`  \x1b[31mFAIL\x1b[0m ${n}${d ? ' — ' + d : ''}`); } };

function upstream(handler) {
  return new Promise((resolve) => { const s = http.createServer(handler); s.listen(0, '127.0.0.1', () => resolve(s)); });
}
function post(base, body) {
  return fetch(base + '/v1/messages', {
    method: 'POST',
    headers: { 'content-type': 'application/json', authorization: 'Bearer client', 'anthropic-version': '2023-06-01' },
    body: JSON.stringify(body),
  }).then((r) => r.text().then((t) => { let j = null; try { j = JSON.parse(t); } catch (_) {} return { status: r.status, json: j }; }));
}
const provider = (port) => ({ id: 'a', name: 'A', baseUrl: `http://127.0.0.1:${port}`, authToken: 'k', defaultModel: 'up-model', smallFastModel: 'up-model', mapDefaultModels: true, models: [] });
const okPayload = JSON.stringify({ id: 'msg_x', type: 'message', model: 'up-model', content: [{ type: 'text', text: 'pong' }], usage: { input_tokens: 1, output_tokens: 1 } });

(async () => {
  // --- retryDelay() pure unit checks ---
  check('retryDelay: numeric Retry-After → seconds→ms', retryDelay('2', 0, 500) === 2000);
  check('retryDelay: Retry-After capped at 30s', retryDelay('9999', 0, 500) === 30000);
  check('retryDelay: backoff grows with attempt', retryDelay(undefined, 0, 100) === 100 && retryDelay(undefined, 2, 100) === 400);
  check('retryDelay: backoff capped at 8s', retryDelay(undefined, 20, 1000) === 8000);
  check('retryDelay: HTTP-date in the past → 0', retryDelay('Wed, 01 Jan 2020 00:00:00 GMT', 0, 500) === 0);

  // --- Case 1: two 429s then 200 → client sees the eventual 200 ---
  let hits = 0;
  const up1 = await upstream((req, res) => {
    hits++;
    if (hits <= 2) { res.writeHead(429, { 'content-type': 'application/json' }); res.end(JSON.stringify({ type: 'error', error: { type: 'rate_limit_error' } })); return; }
    res.writeHead(200, { 'content-type': 'application/json' }); res.end(okPayload);
  });
  const cfg1 = { port: 0, activeProviderId: 'a', retry429: { enabled: true, max: 3, baseMs: 5 }, providers: [provider(up1.address().port)] };
  const g1 = createGateway({ getConfig: () => cfg1 });
  const reqEvents = [];
  g1.on('request', (r) => reqEvents.push(r));
  const r1 = await post(`http://127.0.0.1:${await g1.start(0)}`, { model: 'up-model', max_tokens: 8, messages: [{ role: 'user', content: 'ping' }] });
  check('retries through 429s → final 200', r1.status === 200, `status=${r1.status}`);
  check('upstream hit 3 times (2 retries + success)', hits === 3, `hits=${hits}`);
  check('exactly one request event emitted (retries are silent)', reqEvents.length === 1, `events=${reqEvents.length}`);
  await g1.stop(); up1.close();

  // --- Case 2: persistent 429 with retries exhausted → 429 passes through ---
  let hits2 = 0;
  const up2 = await upstream((req, res) => { hits2++; res.writeHead(429, { 'content-type': 'application/json' }); res.end(JSON.stringify({ type: 'error', error: { type: 'rate_limit_error' } })); });
  const cfg2 = { port: 0, activeProviderId: 'a', retry429: { enabled: true, max: 2, baseMs: 5 }, providers: [provider(up2.address().port)] };
  const g2 = createGateway({ getConfig: () => cfg2 });
  const r2 = await post(`http://127.0.0.1:${await g2.start(0)}`, { model: 'up-model', max_tokens: 8, messages: [{ role: 'user', content: 'ping' }] });
  check('exhausted retries → 429 surfaced to client', r2.status === 429, `status=${r2.status}`);
  check('attempted 1 initial + 2 retries = 3 upstream hits', hits2 === 3, `hits=${hits2}`);
  await g2.stop(); up2.close();

  // --- Case 3: retry disabled → first 429 passes straight through, no retry ---
  let hits3 = 0;
  const up3 = await upstream((req, res) => { hits3++; res.writeHead(429, { 'content-type': 'application/json' }); res.end(JSON.stringify({ type: 'error' })); });
  const cfg3 = { port: 0, activeProviderId: 'a', retry429: { enabled: false, max: 3, baseMs: 5 }, providers: [provider(up3.address().port)] };
  const g3 = createGateway({ getConfig: () => cfg3 });
  const r3 = await post(`http://127.0.0.1:${await g3.start(0)}`, { model: 'up-model', max_tokens: 8, messages: [{ role: 'user', content: 'ping' }] });
  check('retry disabled → 429 passes through immediately', r3.status === 429, `status=${r3.status}`);
  check('retry disabled → upstream hit exactly once', hits3 === 1, `hits=${hits3}`);
  await g3.stop(); up3.close();

  console.log(`\n${pass} passed, ${fail} failed`);
  process.exit(fail ? 1 : 0);
})().catch((e) => { console.error('retry test crashed:', e); process.exit(1); });
