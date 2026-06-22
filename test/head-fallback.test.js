'use strict';

/**
 * `HEAD /` liveness-probe fallback.
 *
 * Claude Desktop/Code probes the endpoint with `HEAD /`. Some Anthropic-compatible
 * upstreams don't implement it and answer 404, which makes the client treat the endpoint
 * as down. The gateway must:
 *   - forward the probe honestly first,
 *   - on a 404, substitute a 200 (flagged via x-ccbud-fallback header) so the probe passes,
 *   - and NOT alter: a genuine upstream 200, a non-root HEAD 404, or GET / 404.
 */

const http = require('http');
const { createGateway } = require('../src/main/proxy');

let pass = 0, fail = 0;
const check = (n, c, d) => { if (c) { pass++; console.log(`  \x1b[32mPASS\x1b[0m ${n}`); } else { fail++; console.log(`  \x1b[31mFAIL\x1b[0m ${n}${d ? ' — ' + d : ''}`); } };

const received = [];
function startUpstream() {
  return new Promise((resolve) => {
    const srv = http.createServer((req, res) => {
      received.push({ method: req.method, url: req.url });
      const path = req.url.replace(/\?.*$/, '');
      if (req.method === 'HEAD') {
        // HEAD never carries a body; root + unknown paths 404, /ok succeeds.
        if (path === '/ok') res.writeHead(200, { 'x-real': 'yes' });
        else res.writeHead(404);
        res.end();
        return;
      }
      res.writeHead(404, { 'content-type': 'text/plain' });
      res.end('nope');
    });
    srv.listen(0, '127.0.0.1', () => resolve(srv));
  });
}

function probe(base, path, method) {
  return fetch(base + path, { method }).then((r) => ({
    status: r.status,
    fb: r.headers.get('x-ccbud-fallback'),
    up: r.headers.get('x-ccbud-upstream-status'),
  }));
}

(async () => {
  const up = await startUpstream();
  const upPort = up.address().port;
  const config = {
    activeProviderId: 'fake',
    providers: [{ id: 'fake', name: 'Fake', baseUrl: `http://127.0.0.1:${upPort}`, authToken: 'sk-x', defaultModel: 'up-model', smallFastModel: 'up-model', models: [] }],
  };
  const gateway = createGateway({ getConfig: () => config });
  const requests = [];
  gateway.on('request', (r) => requests.push(r));
  const port = await gateway.start(0);
  const base = `http://127.0.0.1:${port}`;

  // 1) HEAD / → upstream 404 → gateway substitutes 200 + flags the bypass
  const a = await probe(base, '/', 'HEAD');
  check('HEAD / → 200 (fallback)', a.status === 200, `status=${a.status}`);
  check('HEAD / carries x-ccbud-fallback header', a.fb === 'head-root-404-to-200', `fb=${a.fb}`);
  check('HEAD / reports real upstream status (404)', a.up === '404', `up=${a.up}`);
  check('probe was forwarded honestly first', received.some((r) => r.method === 'HEAD' && r.url === '/'), JSON.stringify(received));

  // 2) genuine upstream 200 → passthrough, NOT flagged
  const b = await probe(base, '/ok', 'HEAD');
  check('HEAD /ok → 200 passthrough', b.status === 200, `status=${b.status}`);
  check('genuine 200 NOT flagged as fallback', !b.fb, `fb=${b.fb}`);

  // 3) non-root HEAD that 404s → left as 404 (we only special-case the root probe)
  const c = await probe(base, '/other', 'HEAD');
  check('HEAD /other → 404 (not masked)', c.status === 404, `status=${c.status}`);
  check('non-root HEAD 404 has no fallback header', !c.fb, `fb=${c.fb}`);

  // 4) GET / that 404s → left as 404 (only HEAD / is special)
  const d = await probe(base, '/', 'GET');
  check('GET / → 404 (not masked)', d.status === 404, `status=${d.status}`);
  check('GET / has no fallback header', !d.fb, `fb=${d.fb}`);

  await new Promise((r) => setTimeout(r, 120));
  const headRootLog = requests.find((r) => r.method === 'HEAD' && r.path === '/');
  check('HEAD / logged with substituted status 200', !!headRootLog && headRootLog.status === 200, headRootLog ? `status=${headRootLog.status}` : 'no log');

  await gateway.stop();
  up.close();
  console.log(`\n${pass} passed, ${fail} failed`);
  process.exit(fail ? 1 : 0);
})().catch((e) => { console.error('head-fallback test crashed:', e); process.exit(1); });
