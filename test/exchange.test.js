'use strict';

/**
 * Offline integration test for the monitor inspector capture path: spins up a fake upstream,
 * forwards a buffered + a streaming request through the gateway, and asserts the captured
 * exchange has the full request/response bodies AND that the real upstream token is redacted.
 */

const http = require('http');
const { createGateway } = require('../src/main/proxy');

let pass = 0, fail = 0;
const check = (n, c, d) => { if (c) { pass++; console.log(`  \x1b[32mPASS\x1b[0m ${n}`); } else { fail++; console.log(`  \x1b[31mFAIL\x1b[0m ${n}${d ? ' — ' + d : ''}`); } };

const SECRET = 'sk-UPSTREAM-SECRET-DO-NOT-LEAK';

function startUpstream() {
  return new Promise((resolve) => {
    const srv = http.createServer((req, res) => {
      let body = '';
      req.on('data', (c) => (body += c));
      req.on('end', () => {
        const wantsStream = body.includes('"stream":true') || body.includes('"stream": true');
        if (wantsStream) {
          res.writeHead(200, { 'content-type': 'text/event-stream' });
          res.write('event: message_start\ndata: {"type":"message_start","message":{"id":"msg_x","model":"up-model","usage":{"input_tokens":3}}}\n\n');
          res.write('event: message_delta\ndata: {"type":"message_delta","usage":{"output_tokens":5}}\n\n');
          res.write('event: message_stop\ndata: {"type":"message_stop"}\n\n');
          res.end();
        } else {
          const payload = JSON.stringify({ id: 'msg_y', type: 'message', model: 'up-model', content: [{ type: 'text', text: 'pong' }], usage: { input_tokens: 3, output_tokens: 1 } });
          res.writeHead(200, { 'content-type': 'application/json' });
          res.end(payload);
        }
      });
    });
    srv.listen(0, '127.0.0.1', () => resolve(srv));
  });
}

function post(base, body) {
  return fetch(base + '/v1/messages', {
    method: 'POST',
    headers: { 'content-type': 'application/json', authorization: 'Bearer client-dummy', 'anthropic-version': '2023-06-01' },
    body: JSON.stringify(body),
  }).then((r) => r.text().then((t) => ({ status: r.status, text: t })));
}

(async () => {
  const up = await startUpstream();
  const upPort = up.address().port;
  const config = {
    port: 0,
    activeProviderId: 'fake',
    providers: [{ id: 'fake', name: 'Fake', baseUrl: `http://127.0.0.1:${upPort}`, authToken: SECRET, defaultModel: 'up-model', smallFastModel: 'up-model', mapDefaultModels: true, models: [] }],
  };
  const gateway = createGateway({ getConfig: () => config });
  const exchanges = [];
  gateway.on('exchange', (e) => exchanges.push(e));
  const port = await gateway.start(0);
  const base = `http://127.0.0.1:${port}`;

  // buffered
  const a = await post(base, { model: 'up-model', max_tokens: 8, messages: [{ role: 'user', content: 'ping' }] });
  check('buffered → 200', a.status === 200, `status=${a.status}`);

  // streaming
  const b = await post(base, { model: 'up-model', max_tokens: 8, stream: true, messages: [{ role: 'user', content: 'ping' }] });
  check('stream → 200', b.status === 200);

  await new Promise((r) => setTimeout(r, 150));

  check('captured 2 exchanges', exchanges.length === 2, `n=${exchanges.length}`);
  check('every exchange has id + status + ms', exchanges.every((e) => e.id != null && e.status === 200 && typeof e.ms === 'number'));

  const buffered = exchanges.find((e) => e.resBody && e.resBody.text.includes('pong'));
  check('buffered response body captured', !!buffered, 'should contain pong');
  check('buffered request body captured', !!(buffered && buffered.reqBody.text.includes('"messages"')));

  const streamed = exchanges.find((e) => e.resBody && e.resBody.text.includes('message_start'));
  check('streaming response body captured (raw SSE)', !!streamed);
  check('streaming captured message_stop too', !!(streamed && streamed.resBody.text.includes('message_stop')));

  const allHeaders = exchanges.map((e) => JSON.stringify(e.reqHeaders) + JSON.stringify(e.resHeaders)).join('');
  check('SECRET upstream token NEVER appears in captured headers', !allHeaders.includes(SECRET), 'token leaked!');
  check('authorization + x-api-key are redacted', exchanges.every((e) => e.reqHeaders.authorization === '••••••（已隐藏）' && e.reqHeaders['x-api-key'] === '••••••（已隐藏）'));

  // truncation accounting on a small body = none
  check('reqBody reports byte count', exchanges.every((e) => e.reqBody.bytes > 0 && e.reqBody.truncated === 0));

  await gateway.stop();
  up.close();
  console.log(`\n${pass} passed, ${fail} failed`);
  process.exit(fail ? 1 : 0);
})().catch((e) => { console.error('exchange test crashed:', e); process.exit(1); });
