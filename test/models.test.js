'use strict';

/**
 * /v1/models augmentation: pass the upstream list through but ADD the user's alias models;
 * synthesize from aliases when the provider returns an error or is unreachable.
 */

const http = require('http');
const { createGateway } = require('../src/main/proxy');

let pass = 0, fail = 0;
const check = (n, c, d) => { if (c) { pass++; console.log(`  \x1b[32mPASS\x1b[0m ${n}`); } else { fail++; console.log(`  \x1b[31mFAIL\x1b[0m ${n}${d ? ' — ' + d : ''}`); } };

function upstream(handler) {
  return new Promise((resolve) => { const s = http.createServer(handler); s.listen(0, '127.0.0.1', () => resolve(s)); });
}
function getModels(base) {
  return fetch(base + '/v1/models', { headers: { authorization: 'Bearer client' } })
    .then((r) => r.text().then((t) => { let j = null; try { j = JSON.parse(t); } catch (_) {} return { status: r.status, json: j }; }));
}
const provider = (port) => ({ id: 'a', name: 'A', baseUrl: `http://127.0.0.1:${port}`, authToken: 'k', defaultModel: 'up-default', smallFastModel: 'up-small', models: [{ alias: 'claude-opus-4.8[1m]', upstream: 'up-model' }, { alias: 'claude-mini', upstream: 'up-small' }] });
const ids = (r) => ((r.json && r.json.data) || []).map((m) => m.id);

(async () => {
  // --- Case 1: provider HAS /v1/models → passthrough + merge aliases ---
  const up1 = await upstream((req, res) => {
    if (req.method === 'GET' && req.url.replace(/\?.*$/, '') === '/v1/models') {
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(JSON.stringify({ data: [{ type: 'model', id: 'up-model', display_name: 'Up' }], has_more: false }));
    } else { res.writeHead(404); res.end('{}'); }
  });
  const cfg1 = { port: 0, activeProviderId: 'a', providers: [provider(up1.address().port)] };
  const g1 = createGateway({ getConfig: () => cfg1 });
  const r1 = await getModels(`http://127.0.0.1:${await g1.start(0)}`);
  check('passthrough → 200', r1.status === 200, `status=${r1.status}`);
  check('upstream model kept', ids(r1).includes('up-model'));
  check('alias #1 added', ids(r1).includes('claude-opus-4.8[1m]'));
  check('alias #2 added', ids(r1).includes('claude-mini'));
  check('aliases listed before upstream models', ids(r1).indexOf('claude-mini') < ids(r1).indexOf('up-model'));
  check('entries have anthropic model shape', (r1.json.data[0].type === 'model' && 'display_name' in r1.json.data[0]));
  await g1.stop(); up1.close();

  // --- Case 2: provider returns 404 (no endpoint) → synthesize from aliases ---
  const up2 = await upstream((req, res) => { res.writeHead(404, { 'content-type': 'application/json' }); res.end(JSON.stringify({ type: 'error' })); });
  const cfg2 = { port: 0, activeProviderId: 'a', providers: [provider(up2.address().port)] };
  const g2 = createGateway({ getConfig: () => cfg2 });
  const r2 = await getModels(`http://127.0.0.1:${await g2.start(0)}`);
  check('404 upstream → synthesized 200', r2.status === 200, `status=${r2.status}`);
  check('synth contains aliases', ids(r2).includes('claude-opus-4.8[1m]') && ids(r2).includes('claude-mini'));
  check('synth does NOT include upstream-only model', !ids(r2).includes('up-model'));
  await g2.stop(); up2.close();

  // --- Case 3: provider unreachable → synthesize from aliases ---
  const cfg3 = { port: 0, activeProviderId: 'a', providers: [provider(1)] }; // port 1 → connection refused
  const g3 = createGateway({ getConfig: () => cfg3 });
  const r3 = await getModels(`http://127.0.0.1:${await g3.start(0)}`);
  check('unreachable upstream → synthesized 200', r3.status === 200, `status=${r3.status}`);
  check('synth aliases present', ids(r3).includes('claude-opus-4.8[1m]'));
  await g3.stop();

  // --- Case 4: no aliases configured → synth falls back to provider's real models ---
  const up4 = await upstream((req, res) => { res.writeHead(404); res.end('{}'); });
  const cfg4 = { port: 0, activeProviderId: 'a', providers: [{ id: 'a', name: 'A', baseUrl: `http://127.0.0.1:${up4.address().port}`, authToken: 'k', defaultModel: 'glm-5.1', smallFastModel: 'glm-air', models: [] }] };
  const g4 = createGateway({ getConfig: () => cfg4 });
  const r4 = await getModels(`http://127.0.0.1:${await g4.start(0)}`);
  check('no-alias synth → 200', r4.status === 200);
  check('no-alias synth falls back to default+small', ids(r4).includes('glm-5.1') && ids(r4).includes('glm-air'), JSON.stringify(ids(r4)));
  await g4.stop(); up4.close();

  console.log(`\n${pass} passed, ${fail} failed`);
  process.exit(fail ? 1 : 0);
})().catch((e) => { console.error('models test crashed:', e); process.exit(1); });
