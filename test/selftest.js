'use strict';

/**
 * End-to-end self test for the ccbud gateway core, hitting the REAL upstream
 * (bigmodel / GLM). Run with:  node test/selftest.js
 *
 * Verifies: passthrough, streaming, alias rewrite, default-model mapping,
 * and response model name rewriting in both buffered JSON and SSE.
 */

const { createGateway } = require('../src/main/proxy');

const GLM = {
  id: 'glm',
  name: 'BigModel GLM',
  // The legacy JS proxy appends the inbound `/v1/messages` path itself. The Tauri preset stores
  // the fully versioned base because its protocol router appends only `/messages`.
  baseUrl: process.env.CCBUD_TEST_BASEURL || 'https://open.bigmodel.cn/api/anthropic',
  authToken: process.env.CCBUD_TEST_TOKEN || '', // never commit a real key — set via env to run live tests
  defaultModel: 'glm-5.1',
  smallFastModel: 'glm-5.1',
  mapDefaultModels: true,
  models: [{ alias: 'claude-opus-4.8[1m]', upstream: 'glm-5.1' }],
};

const config = { port: 0, activeProviderId: 'glm', providers: [GLM] };
const gateway = createGateway({ getConfig: () => config });
const reqEvents = [];
gateway.on('request', (r) => reqEvents.push(r));
const exchanges = [];
gateway.on('exchange', (e) => exchanges.push(e));

let pass = 0;
let fail = 0;
function check(name, cond, detail) {
  if (cond) {
    pass++;
    console.log(`  \x1b[32mPASS\x1b[0m ${name}`);
  } else {
    fail++;
    console.log(`  \x1b[31mFAIL\x1b[0m ${name}${detail ? ' — ' + detail : ''}`);
  }
}

async function postJson(base, body) {
  const r = await fetch(base + '/v1/messages', {
    method: 'POST',
    headers: {
      'content-type': 'application/json',
      authorization: 'Bearer dummy-client-token',
      'anthropic-version': '2023-06-01',
    },
    body: JSON.stringify(body),
  });
  const text = await r.text();
  let json = null;
  try {
    json = JSON.parse(text);
  } catch (_) {}
  return { status: r.status, text, json };
}

async function postStream(base, body) {
  const r = await fetch(base + '/v1/messages', {
    method: 'POST',
    headers: {
      'content-type': 'application/json',
      authorization: 'Bearer dummy-client-token',
      'anthropic-version': '2023-06-01',
    },
    body: JSON.stringify(body),
  });
  const text = await r.text();
  return { status: r.status, text, ct: r.headers.get('content-type') };
}

function firstModelInSse(sse) {
  const m = sse.match(/event:\s*message_start[\s\S]*?data:\s*(\{.*\})/);
  if (!m) return null;
  try {
    return JSON.parse(m[1]).message.model;
  } catch (_) {
    return null;
  }
}

(async () => {
  const port = await gateway.start(0);
  const base = `http://127.0.0.1:${port}`;
  console.log(`gateway up on ${base}\n`);

  console.log('Routing unit checks:');
  // Distinct PRIMARY/LIGHTWEIGHT tiers + a second (inactive) provider, so we can tell the
  // mapping targets apart and prove routing stays on the ACTIVE provider (issue #10).
  const cfg2 = {
    port: 0,
    activeProviderId: 'main',
    providers: [
      { id: 'main', name: 'Main', baseUrl: 'http://127.0.0.1:1', authToken: 'k', defaultModel: 'big-model', smallFastModel: 'small-model', mapDefaultModels: true, models: [{ alias: 'my-alias', upstream: 'aliased-up' }] },
      { id: 'other', name: 'Other', baseUrl: 'http://127.0.0.1:2', authToken: 'k', defaultModel: 'other-big', smallFastModel: 'other-small', mapDefaultModels: true, models: [{ alias: 'other-alias', upstream: 'other-up' }] },
    ],
  };
  check(
    'active-provider alias resolves to its upstream',
    (() => {
      const r = gateway._resolveRouting('claude-opus-4.8[1m]', config);
      return r && r.outgoingModel === 'glm-5.1' && r.clientFacingModel === 'claude-opus-4.8[1m]';
    })()
  );
  check(
    'real model passes through untouched',
    (() => {
      const r = gateway._resolveRouting('glm-5.1', config);
      return r && r.outgoingModel === 'glm-5.1' && r.clientFacingModel === 'glm-5.1';
    })()
  );
  check(
    'claude small tier (haiku) → LIGHTWEIGHT model',
    (() => {
      const r = gateway._resolveRouting('claude-3-5-haiku-20241022', cfg2);
      return r && r.outgoingModel === 'small-model' && r.clientFacingModel === 'claude-3-5-haiku-20241022';
    })()
  );
  check(
    'claude main tier (opus/sonnet) → PRIMARY model',
    (() => {
      const r = gateway._resolveRouting('claude-sonnet-4-6', cfg2);
      return r && r.outgoingModel === 'big-model' && r.clientFacingModel === 'claude-sonnet-4-6';
    })()
  );
  check(
    'unconfigured foreign model → LIGHTWEIGHT (issue #10 catch-all)',
    (() => {
      const r = gateway._resolveRouting('gpt-4-turbo', cfg2);
      return r && r.outgoingModel === 'small-model' && r.clientFacingModel === 'gpt-4-turbo';
    })()
  );
  check(
    'a model the provider really has (via /v1/models) passes through',
    (() => {
      const r = gateway._resolveRouting('glm-5.2', cfg2, new Set(['glm-5.2']));
      return r && r.outgoingModel === 'glm-5.2' && r.clientFacingModel === 'glm-5.2';
    })()
  );
  check(
    'routing stays on the ACTIVE provider — an inactive provider\'s alias is NOT followed',
    (() => {
      const r = gateway._resolveRouting('other-alias', cfg2); // belongs to inactive "other"
      // not an alias/real model of "main" → treated as unknown → mapped onto main's LIGHTWEIGHT
      return r && r.provider.id === 'main' && r.outgoingModel === 'small-model';
    })()
  );
  check(
    'mapDefaultModels:false forwards unknown names untouched',
    (() => {
      const off = { port: 0, activeProviderId: 'm', providers: [{ id: 'm', name: 'M', baseUrl: 'http://127.0.0.1:1', authToken: 'k', defaultModel: 'big', smallFastModel: 'small', mapDefaultModels: false, models: [] }] };
      const r = gateway._resolveRouting('whatever-x', off);
      return r && r.outgoingModel === 'whatever-x' && r.clientFacingModel === 'whatever-x';
    })()
  );
  check(
    'codex sentinel gpt-5.5-ccbud routes to the PRIMARY model',
    (() => {
      const r = gateway._resolveRouting('gpt-5.5-ccbud', cfg2);
      return r && r.outgoingModel === 'big-model' && r.clientFacingModel === 'gpt-5.5-ccbud';
    })()
  );

  if (!GLM.authToken) {
    console.log('\n(skipping live upstream checks — set CCBUD_TEST_TOKEN to run them)');
    await gateway.stop();
    console.log(`\n${pass} passed, ${fail} failed`);
    process.exit(fail ? 1 : 0);
  }

  console.log('\nLive upstream checks (real GLM):');

  // A) passthrough non-stream
  const a = await postJson(base, {
    model: 'glm-5.1',
    max_tokens: 32,
    messages: [{ role: 'user', content: 'say hi' }],
  });
  check('A passthrough non-stream → 200', a.status === 200, `status=${a.status} ${a.text.slice(0, 120)}`);
  check('A response is a message with content', !!(a.json && a.json.type === 'message' && a.json.content));

  // B) passthrough stream
  const b = await postStream(base, {
    model: 'glm-5.1',
    max_tokens: 32,
    stream: true,
    messages: [{ role: 'user', content: 'say hi' }],
  });
  check('B stream → text/event-stream', (b.ct || '').includes('text/event-stream'), `ct=${b.ct}`);
  check('B stream contains message_start', b.text.includes('event: message_start'));

  // C) alias non-stream → response model rewritten back to alias
  const c = await postJson(base, {
    model: 'claude-opus-4.8[1m]',
    max_tokens: 32,
    messages: [{ role: 'user', content: 'say hi' }],
  });
  check('C alias non-stream → 200', c.status === 200, `status=${c.status} ${c.text.slice(0, 160)}`);
  check(
    'C response model rewritten to alias',
    !!(c.json && c.json.model === 'claude-opus-4.8[1m]'),
    `model=${c.json && c.json.model}`
  );

  // D) alias stream → message_start model rewritten back to alias
  const d = await postStream(base, {
    model: 'claude-opus-4.8[1m]',
    max_tokens: 32,
    stream: true,
    messages: [{ role: 'user', content: 'say hi' }],
  });
  const dModel = firstModelInSse(d.text);
  check('D alias stream → message_start model rewritten', dModel === 'claude-opus-4.8[1m]', `model=${dModel}`);

  // E) claude default name mapped, response rewritten back to the default name
  const e = await postJson(base, {
    model: 'claude-sonnet-4-20250514',
    max_tokens: 32,
    messages: [{ role: 'user', content: 'say hi' }],
  });
  check('E claude-default non-stream → 200', e.status === 200, `status=${e.status} ${e.text.slice(0, 160)}`);
  check(
    'E response model rewritten back to requested default name',
    !!(e.json && e.json.model === 'claude-sonnet-4-20250514'),
    `model=${e.json && e.json.model}`
  );

  // F) gateway access token enforcement
  config.requireToken = true;
  config.gatewayToken = 'secret-token-xyz';
  const fWrong = await fetch(base + '/v1/messages', {
    method: 'POST',
    headers: { 'content-type': 'application/json', authorization: 'Bearer nope' },
    body: JSON.stringify({ model: 'glm-5.1', max_tokens: 8, messages: [{ role: 'user', content: 'hi' }] }),
  });
  check('F wrong gateway token → 401', fWrong.status === 401, `status=${fWrong.status}`);
  const fOk = await fetch(base + '/v1/messages', {
    method: 'POST',
    headers: { 'content-type': 'application/json', authorization: 'Bearer secret-token-xyz', 'anthropic-version': '2023-06-01' },
    body: JSON.stringify({ model: 'glm-5.1', max_tokens: 8, messages: [{ role: 'user', content: 'hi' }] }),
  });
  check('F correct gateway token → 200', fOk.status === 200, `status=${fOk.status}`);
  config.requireToken = false;
  config.gatewayToken = '';

  // G) token usage captured from real responses (non-stream + stream)
  await new Promise((r) => setTimeout(r, 300)); // let stream finishLog fire
  const withInput = reqEvents.filter((e) => e.inputTokens > 0);
  const withOutput = reqEvents.filter((e) => e.outputTokens > 0);
  check('G captured input tokens from responses', withInput.length > 0, `events=${reqEvents.length}`);
  check('G captured output tokens from responses', withOutput.length > 0);
  check('G captured usage from a STREAM response', reqEvents.some((e) => e.outputTokens > 0));

  // H) monitor inspector captures full exchanges with the upstream token REDACTED
  await new Promise((r) => setTimeout(r, 300));
  check('H captured >=1 exchange', exchanges.length > 0, `n=${exchanges.length}`);
  check('H every exchange has an id', exchanges.every((e) => e.id != null));
  check('H captured a request body', exchanges.some((e) => e.reqBody && e.reqBody.text && e.reqBody.text.includes('messages')));
  check('H captured a response body', exchanges.some((e) => e.resBody && e.resBody.text && e.resBody.text.length > 0));
  const allHeaders = exchanges.map((e) => JSON.stringify(e.reqHeaders || {}) + JSON.stringify(e.resHeaders || {})).join('');
  check('H upstream token is REDACTED in captured headers', !allHeaders.includes(GLM.authToken) && allHeaders.includes('已隐藏'), 'token must never appear');

  await gateway.stop();
  console.log(`\n${pass} passed, ${fail} failed`);
  process.exit(fail ? 1 : 0);
})().catch((e) => {
  console.error('selftest crashed:', e);
  process.exit(1);
});
