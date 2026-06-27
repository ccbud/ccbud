'use strict';

/**
 * Bundled Microsoft Presidio integration — local PII detection + redaction for the gateway.
 *
 * Approach (per product decision): the app brings its own Python via `uv`, installs the Presidio
 * source (analyzer + anonymizer) into a private venv, and spawns the two Flask services locally.
 * The proxy then runs outbound request text through analyze → anonymize before forwarding, so
 * sensitive content never leaves the machine. v1 = redact-only (outbound); reversible round-trip
 * is a later phase. Chinese: regex recognizers (email/card/IP/IBAN/crypto/phone) work cross-language
 * now; a Chinese spaCy model is a later enhancement.
 */

const fs = require('fs');
const path = require('path');
const os = require('os');
const http = require('http');
const { spawn, execFile } = require('child_process');

const ROOT = path.join(os.homedir(), 'Library', 'Application Support', 'ccbud', 'presidio-env');
const USER_VENV = path.join(ROOT, 'venv');
const NLP_CONF = path.join(ROOT, 'nlp-sm.yaml');
const SETUP_LOG = path.join(ROOT, 'setup.log');
// Repo root (src/main → ..) so dev can use the prepare:presidio output under vendor/ with no packaging.
const REPO_ROOT = path.join(__dirname, '..', '..');

const IS_WIN = process.platform === 'win32';
// venv layout differs by OS: POSIX → venv/bin/python, Windows → venv\Scripts\python.exe.
function venvPy(venv) { return IS_WIN ? path.join(venv, 'Scripts', 'python.exe') : path.join(venv, 'bin', 'python'); }
// BUNDLED runtime = a self-contained, relocatable standalone Python (python-build-standalone)
// shipped under resourcesPath/presidio-env/python, with Presidio installed into its OWN
// site-packages (no venv → no pyvenv.cfg / symlink-to-host problems). This is the shipping path.
function bundledPy() {
  const roots = [
    process.env.CCBUD_PRESIDIO_ENV,
    process.resourcesPath && path.join(process.resourcesPath, 'presidio-env'),
    path.join(REPO_ROOT, 'vendor', 'presidio-env'), // dev: `npm run prepare:presidio` output
  ].filter(Boolean);
  for (const r of roots) {
    // standalone Python layout: POSIX → python/bin/python3.12 ; Windows → python\python.exe
    const py = IS_WIN ? path.join(r, 'python', 'python.exe') : path.join(r, 'python', 'bin', 'python3.12');
    try { if (fs.existsSync(py)) return py; } catch (_) {}
  }
  return null;
}
// Prefer the bundled standalone Python; fall back to the dev/user venv (created by setup()).
function pyExe() { return bundledPy() || venvPy(USER_VENV); }

// Uncommon high ports (>60000) to avoid clashing with common dev servers. The start sequence
// force-frees these and falls back to the next pair if one can't come up.
const PORT_CANDIDATES = [[61902, 61901], [62514, 62513], [63771, 63770]];
let ANALYZER_PORT = PORT_CANDIDATES[0][0];
let ANONYMIZER_PORT = PORT_CANDIDATES[0][1];

const ANALYZE_SCORE_THRESHOLD = 0.4;

// v1 default: the reliable regex/checksum recognizers, which are language-agnostic (work on
// Chinese text too). The spaCy-NER entities (PERSON/LOCATION/ORGANIZATION/DATE_TIME/NRP/URL) are
// excluded for now — the English model misfires on Chinese; they return with the Chinese model.
const DEFAULT_ENTITIES = [
  'CREDIT_CARD', 'CRYPTO', 'IBAN_CODE', 'US_SSN', 'US_BANK_NUMBER', 'US_ITIN',
  'EMAIL_ADDRESS', 'PHONE_NUMBER', 'IP_ADDRESS', 'MEDICAL_LICENSE',
  'US_PASSPORT', 'US_DRIVER_LICENSE',
];
// NER tier (opt-in) — produced by the spaCy NER pipe; English-quality with the small model, hence
// off by default. PERSON/LOCATION/NRP are the meaningful personal entities.
const NER_ENTITIES = ['PERSON', 'LOCATION', 'NRP'];

let procAnalyzer = null;
let procAnonymizer = null;
let setupProc = null;
let lastError = null;

/* ---------- console log buffer (streamed to the renderer) ---------- */
const _logBuf = [];
const LOG_MAX = 500;
let _logSink = null;
function setLogSink(fn) { _logSink = typeof fn === 'function' ? fn : null; }
function pushLog(line) {
  const entry = String(line);
  _logBuf.push(entry);
  if (_logBuf.length > LOG_MAX) _logBuf.splice(0, _logBuf.length - LOG_MAX);
  if (_logSink) { try { _logSink(entry); } catch (_) {} }
}
function getLogs() { return _logBuf.slice(); }
function clearLogs() { _logBuf.length = 0; }

/* ---------- findings buffer (detected PII, for the Findings table) ---------- */
const _findBuf = [];
const FIND_MAX = 200;
let _findSink = null;
function setFindingsSink(fn) { _findSink = typeof fn === 'function' ? fn : null; }
function pushFinding(f) {
  _findBuf.push(f);
  if (_findBuf.length > FIND_MAX) _findBuf.splice(0, _findBuf.length - FIND_MAX);
  if (_findSink) { try { _findSink(f); } catch (_) {} }
}
function getFindings() { return _findBuf.slice(); }
function clearFindings() { _findBuf.length = 0; }

const isMac = () => process.platform === 'darwin';
function uvPath() {
  const c = [process.env.CCBUD_UV, path.join(os.homedir(), '.local/bin/uv'), '/opt/homebrew/bin/uv', '/usr/local/bin/uv'].filter(Boolean);
  return c.find((p) => { try { return fs.existsSync(p); } catch (_) { return false; } }) || 'uv';
}
// Presidio source — bundled with the packaged app; the local checkout in dev.
function sourceDir() {
  const c = [
    process.env.CCBUD_PRESIDIO_SRC,
    process.resourcesPath && path.join(process.resourcesPath, 'presidio'),
    path.join(REPO_ROOT, 'vendor', 'presidio-src'),   // dev: prepare:presidio output
    path.join(REPO_ROOT, 'assets', 'presidio-src'),   // dev: official entry points bundled in-repo
    path.join(os.homedir(), 'code', 'presidio'),       // legacy: external checkout
  ].filter(Boolean);
  return c.find((p) => { try { return fs.existsSync(path.join(p, 'presidio-analyzer', 'app.py')); } catch (_) { return false; } }) || null;
}

const envReady = () => { try { return fs.existsSync(pyExe()); } catch (_) { return false; } };

/* ---------- env setup (uv venv + install + small model) ---------- */
function writeNlpConf() {
  fs.mkdirSync(ROOT, { recursive: true });
  // Two-language spaCy engine (en + zh) so the NER tier covers Chinese names/places/orgs.
  fs.writeFileSync(NLP_CONF,
    'nlp_engine_name: spacy\n' +
    'models:\n' +
    '  - lang_code: en\n    model_name: en_core_web_sm\n' +
    '  - lang_code: zh\n    model_name: zh_core_web_sm\n');
}
// Returns 'ready' | 'installing' | 'missing-source' | 'idle'
function setupState() {
  if (envReady()) return 'ready';
  if (setupProc) return 'installing';
  return sourceDir() ? 'idle' : 'missing-source';
}
function setup() {
  if (envReady() || setupProc) return { ok: true, state: setupState() };
  const src = sourceDir();
  if (!src) { lastError = 'presidio source not found'; return { ok: false, reason: 'missing-source' }; }
  fs.mkdirSync(ROOT, { recursive: true });
  const uv = uvPath();
  const script = [
    'set -e',
    `export PATH="$HOME/.local/bin:$PATH"`,
    `"${uv}" venv --python 3.12 "${USER_VENV}"`,
    `"${uv}" pip install --python "${USER_VENV}" "${src}/presidio-analyzer[server]" "${src}/presidio-anonymizer[server]"`,
    // uv venvs have no pip, so install the spaCy model WHEEL directly (matches spaCy 3.8.x).
    `"${uv}" pip install --python "${USER_VENV}" "https://github.com/explosion/spacy-models/releases/download/en_core_web_sm-3.8.0/en_core_web_sm-3.8.0-py3-none-any.whl"`,
    'echo CCBUD_SETUP_DONE',
  ].join('\n');
  const out = fs.openSync(SETUP_LOG, 'a');
  setupProc = spawn('/bin/bash', ['-c', script], { stdio: ['ignore', out, out] });
  setupProc.on('exit', (code) => {
    setupProc = null;
    if (code !== 0) lastError = `setup exited ${code} (see ${SETUP_LOG})`;
  });
  return { ok: true, state: 'installing' };
}

/* ---------- service lifecycle ---------- */
function spawnService(appRel, port, extraEnv) {
  const src = sourceDir();
  const appDir = path.join(src, appRel);
  const env = Object.assign({}, process.env, { PORT: String(port), PYTHONUNBUFFERED: '1' }, extraEnv || {});
  const tag = appRel.replace('presidio-', '');
  pushLog(`[${tag}] starting on :${port}`);
  const p = spawn(pyExe(), ['app.py'], { cwd: appDir, env, stdio: ['ignore', 'pipe', 'pipe'] });
  const wire = (stream) => {
    let buf = '';
    stream.setEncoding('utf8');
    stream.on('data', (chunk) => {
      buf += chunk;
      let i;
      while ((i = buf.indexOf('\n')) >= 0) {
        const line = buf.slice(0, i).replace(/\s+$/, '');
        buf = buf.slice(i + 1);
        if (line) pushLog(`[${tag}] ${line}`);
      }
    });
  };
  wire(p.stdout); wire(p.stderr);
  p.on('exit', (code, sig) => pushLog(`[${tag}] exited (code ${code}${sig ? ', ' + sig : ''})`));
  p.on('error', (e) => pushLog(`[${tag}] spawn error: ${e.message}`));
  return p;
}
function sleep(ms) { return new Promise((r) => setTimeout(r, ms)); }
// Force-free a TCP port — kills any lingering listener (e.g. an orphaned service from a crash/-9).
function killPort(port) {
  return new Promise((resolve) => {
    if (IS_WIN) {
      // Free the port on Windows: find the PID LISTENING on it (netstat) and force-kill it.
      const cmd = `for /f "tokens=5" %a in ('netstat -ano ^| findstr :${port} ^| findstr LISTENING') do taskkill /PID %a /F`;
      try { execFile('cmd.exe', ['/c', cmd], { timeout: 5000 }, () => resolve()); } catch (_) { resolve(); }
      return;
    }
    const cmd = `PATH="/usr/sbin:/usr/bin:/bin:/sbin:$PATH" lsof -ti tcp:${port} | xargs kill -9 2>/dev/null || true`;
    try { execFile('/bin/sh', ['-c', cmd], { timeout: 4000 }, () => resolve()); } catch (_) { resolve(); }
  });
}
function killProcs() {
  for (const p of [procAnalyzer, procAnonymizer]) { try { if (p && p.exitCode === null) p.kill('SIGKILL'); } catch (_) {} }
  procAnalyzer = null; procAnonymizer = null;
}
// Wait until both services are healthy, or bail early if a process dies (e.g. port still in use).
async function waitReady(timeoutMs) {
  const deadline = Date.now() + (timeoutMs || 30000);
  while (Date.now() < deadline) {
    if (await healthy()) return 'healthy';
    if ((procAnalyzer && procAnalyzer.exitCode !== null) || (procAnonymizer && procAnonymizer.exitCode !== null)) return 'dead';
    await sleep(500);
  }
  return 'timeout';
}
async function start() {
  if (!envReady()) return { ok: false, reason: 'env-not-ready' };
  if (!sourceDir()) return { ok: false, reason: 'missing-source' };
  writeNlpConf();
  killProcs();                               // every (re)start force-kills any prior services first
  for (let i = 0; i < PORT_CANDIDATES.length; i++) {
    [ANALYZER_PORT, ANONYMIZER_PORT] = PORT_CANDIDATES[i];
    await killPort(ANALYZER_PORT);
    await killPort(ANONYMIZER_PORT);
    procAnalyzer = spawnService('presidio-analyzer', ANALYZER_PORT, { NLP_CONF_FILE: NLP_CONF });
    procAnonymizer = spawnService('presidio-anonymizer', ANONYMIZER_PORT, {});
    pushLog('[gateway] waiting for services to load the model…');
    const r = await waitReady(45000); // generous: cold start loads two spaCy models (en + zh)
    if (r === 'healthy') { lastError = null; pushLog('[gateway] services healthy — content filter active'); return { ok: true }; }
    pushLog(`[gateway] start failed on :${ANALYZER_PORT}/:${ANONYMIZER_PORT} (${r}) — switching port`);
    killProcs();
  }
  lastError = 'services did not become healthy';
  pushLog('[gateway] could not start on any candidate port — see output above');
  return { ok: false, reason: 'unhealthy' };
}
function stop() {
  const wasRunning = !!(procAnalyzer || procAnonymizer);
  const ports = [ANALYZER_PORT, ANONYMIZER_PORT];
  killProcs();
  if (wasRunning) pushLog('[gateway] stopped');
  for (const p of ports) killPort(p);        // free the ports in case anything orphaned
}
function processesAlive() {
  return !!(procAnalyzer && procAnalyzer.exitCode === null && procAnonymizer && procAnonymizer.exitCode === null);
}

/* ---------- HTTP helpers ---------- */
function postJson(port, pathname, body, timeoutMs) {
  return new Promise((resolve, reject) => {
    const data = Buffer.from(JSON.stringify(body));
    const req = http.request(
      { host: '127.0.0.1', port, path: pathname, method: 'POST', headers: { 'content-type': 'application/json', 'content-length': data.length } },
      (res) => {
        let buf = '';
        res.on('data', (c) => (buf += c));
        res.on('end', () => {
          if (res.statusCode >= 200 && res.statusCode < 300) {
            try { resolve(JSON.parse(buf)); } catch (e) { reject(e); }
          } else reject(new Error(`HTTP ${res.statusCode}: ${buf.slice(0, 200)}`));
        });
      }
    );
    req.on('error', reject);
    req.setTimeout(timeoutMs || 8000, () => req.destroy(new Error('timeout')));
    req.end(data);
  });
}
function getHealth(port) {
  return new Promise((resolve) => {
    const req = http.get({ host: '127.0.0.1', port, path: '/health', timeout: 2000 }, (res) => {
      res.resume();
      resolve(res.statusCode === 200);
    });
    req.on('error', () => resolve(false));
    req.on('timeout', () => { req.destroy(); resolve(false); });
  });
}
async function healthy() {
  const [a, b] = await Promise.all([getHealth(ANALYZER_PORT), getHealth(ANONYMIZER_PORT)]);
  return a && b;
}
async function waitHealthy(timeoutMs) {
  const deadline = Date.now() + (timeoutMs || 20000);
  while (Date.now() < deadline) {
    if (await healthy()) return true;
    await new Promise((r) => setTimeout(r, 600));
  }
  return false;
}

/* ---------- the proxy client: detect + redact one text ---------- */
// Claude Code resends the whole conversation each turn, so cache text→redacted to avoid re-work.
const _cache = new Map();
const CACHE_MAX = 4000;

// opts: { language, ner, llm, ollamaUrl, ollamaModel }
async function redactText(text, opts) {
  opts = opts || {};
  if (!text || typeof text !== 'string' || !text.trim()) return text;
  const key = (opts.ner ? 'N' : '') + (opts.llm ? 'L' : '') + (opts.deidentify || 'replace') + (opts.threshold != null ? opts.threshold : '') + ' ' + text;
  if (_cache.has(key)) { const v = _cache.get(key); _cache.delete(key); _cache.set(key, v); return v; }
  let out = await _presidioRedact(text, opts);                       // tier 1+2: regex (+ NER)
  if (opts.llm && opts.ollamaUrl) { try { out = await llmRedact(out, opts); } catch (_) {} } // tier 3
  _cache.set(key, out);
  if (_cache.size > CACHE_MAX) _cache.delete(_cache.keys().next().value);
  return out;
}

// Map the chosen de-identification approach to a Presidio anonymizer operator.
function buildAnonymizers(approach) {
  switch (approach) {
    case 'redact': return { DEFAULT: { type: 'redact' } };
    case 'mask': return { DEFAULT: { type: 'mask', masking_char: '*', chars_to_mask: 40, from_end: false } };
    case 'hash': return { DEFAULT: { type: 'hash' } };
    default: return null; // 'replace' → <ENTITY> (Presidio default)
  }
}
// Presidio's regex recognizers (card / email / phone / IBAN / SSN …) are bounded by `\b`. Python's
// Unicode `\w` counts CJK as word chars, so a number that runs straight into Chinese ("卡号是4716…")
// has NO word boundary and never matches — only a space makes it fire. So for detection we insert a
// space at every CJK↔alphanumeric edge, then map the spans back onto the ORIGINAL text (preserving it
// exactly — the inserted spaces are detection-only). No-op for text without CJK.
const CJK_RE = /[㐀-䶿一-鿿豈-﫿぀-ヿ가-힯]/;
function insertCjkBoundaries(text) {
  if (!CJK_RE.test(text)) return { spaced: text, map: null };
  const an = (c) => (c >= '0' && c <= '9') || (c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z');
  const cjk = (c) => CJK_RE.test(c);
  let out = '';
  const map = []; // map[k] = index in `text` of out[k]
  for (let i = 0; i < text.length; i++) {
    const c = text[i];
    if (i > 0) { const p = text[i - 1]; if ((cjk(p) && an(c)) || (an(p) && cjk(c))) { out += ' '; map.push(i); } }
    out += c; map.push(i);
  }
  return { spaced: out, map };
}

// Local (Node-side) recognizers for high-value PII that Presidio's US-centric defaults miss:
// mainland-China resident ID (18-digit) and mobile numbers. Matched on the ORIGINAL text.
const LOCAL_RECOGNIZERS = [
  { entity: 'CN_ID_CARD', re: /(?<!\d)[1-9]\d{5}(?:19|20)\d{2}(?:0[1-9]|1[0-2])(?:0[1-9]|[12]\d|3[01])\d{3}[\dXx](?!\d)/g },
  { entity: 'PHONE_NUMBER', re: /(?<!\d)1[3-9]\d{9}(?!\d)/g },
];
function localSpans(text) {
  const out = [];
  for (const rec of LOCAL_RECOGNIZERS) {
    rec.re.lastIndex = 0;
    let m;
    while ((m = rec.re.exec(text)) !== null) {
      out.push({ entity_type: rec.entity, start: m.index, end: m.index + m[0].length, score: 0.95 });
    }
  }
  return out;
}

// One /analyze call → array of presidio results {entity_type,start,end,score,…}; [] on empty/error.
async function analyzeSpans(text, language, entities, threshold) {
  if (!text || !text.trim()) return [];
  try {
    const r = await postJson(ANALYZER_PORT, '/analyze', { text, language, score_threshold: threshold, entities });
    return Array.isArray(r) ? r : [];
  } catch (_) { return []; }
}
async function _presidioRedact(text, opts) {
  const threshold = (typeof opts.threshold === 'number' && opts.threshold >= 0) ? opts.threshold : ANALYZE_SCORE_THRESHOLD;
  const hasCjk = CJK_RE.test(text);
  const spans = [];

  // Tier 1 — regex/checksum recognizers, language-agnostic. Run on the CJK-spaced copy so `\b` fires
  // next to Chinese, then map spans back onto the ORIGINAL text. (The spaced copy isn't real Chinese,
  // so it always uses the `en` engine.)
  const { spaced, map } = insertCjkBoundaries(text);
  const regex = await analyzeSpans(spaced, hasCjk ? 'en' : (opts.language || 'en'), DEFAULT_ENTITIES, threshold);
  for (const r of regex) {
    if (!map) { spans.push(r); continue; }
    const start = map[r.start];
    const end = (r.end - 1 < map.length) ? map[r.end - 1] + 1 : text.length;
    spans.push(Object.assign({}, r, { start, end }));
  }

  // Tier 2 — NER (opt-in) runs on the ORIGINAL text (spacing would break tokenization); Chinese text
  // uses the zh model, everything else the en model. Coordinates already match the original text.
  if (opts.ner) {
    const ner = await analyzeSpans(text, hasCjk ? 'zh' : (opts.language || 'en'), NER_ENTITIES, threshold);
    for (const r of ner) spans.push(r);
  }

  // Local recognizers (China ID / mobile) on the original text — Presidio's defaults miss these.
  for (const r of localSpans(text)) spans.push(r);

  // De-dup identical spans (regex + NER can occasionally land on the same range).
  const seen = new Set();
  const merged = spans.filter((r) => { const k = r.entity_type + ':' + r.start + ':' + r.end; if (seen.has(k)) return false; seen.add(k); return true; });
  if (!merged.length) return text;

  // Capture findings for the Findings table (entity type + matched span + confidence). Local only.
  for (const r of merged) {
    pushFinding({ entity: r.entity_type, text: text.slice(r.start, r.end), start: r.start, end: r.end, score: Math.round((r.score || 0) * 100) / 100, ts: Date.now() });
  }
  const body = { text, analyzer_results: merged };
  const anonymizers = buildAnonymizers(opts.deidentify);
  if (anonymizers) body.anonymizers = anonymizers;
  const out = await postJson(ANONYMIZER_PORT, '/anonymize', body);
  return (out && typeof out.text === 'string') ? out.text : text;
}

// LLM tier — ask a local Ollama model for the exact PII substrings, then redact those spans in Node
// (we redact what it returns rather than trusting a full LLM rewrite). Best-effort: errors are
// swallowed by the caller so the regex/NER result still stands.
async function llmRedact(text, opts) {
  const base = String(opts.ollamaUrl || '').replace(/\/+$/, '');
  if (!base) return text;
  const model = opts.ollamaModel || 'qwen2.5:7b';
  const sys =
    'Identify every piece of personally identifiable or sensitive information in the user text '
    + '(person names, ID/passport numbers, addresses, phone numbers, bank/account numbers, emails, etc.). '
    + 'Respond ONLY with compact JSON {"pii": ["<exact substring>", ...]} — substrings exactly as they appear.';
  const res = await postJsonAbs(base + '/api/chat', {
    model, stream: false, format: 'json',
    messages: [{ role: 'system', content: sys }, { role: 'user', content: text }],
  }, 20000);
  let spans = [];
  try {
    const c = res && res.message && res.message.content;
    const j = typeof c === 'string' ? JSON.parse(c) : c;
    spans = Array.isArray(j) ? j : ((j && (j.pii || j.entities || j.items)) || []);
  } catch (_) {}
  let out = text;
  for (const s of spans) if (typeof s === 'string' && s.trim().length > 1 && out.includes(s)) out = out.split(s).join('<PII>');
  return out;
}

function postJsonAbs(urlStr, body, timeoutMs) {
  return new Promise((resolve, reject) => {
    let u; try { u = new URL(urlStr); } catch (e) { return reject(e); }
    const data = Buffer.from(JSON.stringify(body));
    const lib = u.protocol === 'https:' ? require('https') : http;
    const req = lib.request(
      { hostname: u.hostname, port: u.port || (u.protocol === 'https:' ? 443 : 80), path: u.pathname + u.search, method: 'POST', headers: { 'content-type': 'application/json', 'content-length': data.length } },
      (res) => { let buf = ''; res.on('data', (c) => (buf += c)); res.on('end', () => { if (res.statusCode >= 200 && res.statusCode < 300) { try { resolve(JSON.parse(buf)); } catch (e) { reject(e); } } else reject(new Error('HTTP ' + res.statusCode)); }); }
    );
    req.on('error', reject);
    req.setTimeout(timeoutMs || 20000, () => req.destroy(new Error('timeout')));
    req.end(data);
  });
}

async function status() {
  return {
    // Bundled Python ships per-platform (extraResources), so any OS with the env can run it.
    supported: true,
    setup: setupState(),
    running: processesAlive() && (await healthy()),
    error: lastError,
    analyzerPort: ANALYZER_PORT,
    anonymizerPort: ANONYMIZER_PORT,
  };
}

module.exports = {
  setup, setupState, envReady, start, stop, healthy, status, redactText,
  getLogs, clearLogs, setLogSink,
  getFindings, clearFindings, setFindingsSink,
  ANALYZER_PORT, ANONYMIZER_PORT, sourceDir, uvPath,
};
