'use strict';

const api = window.ccbud;
let config = { port: 8788, activeProviderId: null, providers: [] };
let status = { running: false, port: null, connected: false, lastStartError: null, claudePath: '' };
let editingId = null;
let modalIcon = null;   // the icon being edited in the add/edit modal (emoji or image data-URL)
let dragId = null;
const stats = { total: 0, ok: 0, sumMs: 0, last: null };

const $ = (id) => document.getElementById(id);
const I = window.ccbudIcons || {};

function injectIcons(root) {
  (root || document).querySelectorAll('[data-icon]').forEach((el) => {
    const name = el.dataset.icon;
    if (I[name]) el.innerHTML = I[name];
  });
}

// Each preset declares its wire `protocol` up front — Anthropic-native endpoints (the `/anthropic`
// gateways) pass through directly; OpenAI-compatible endpoints are auto-translated. Picking a preset
// sets the protocol so the user knows immediately how their requests will be handled.
const PRESETS = {
  glm: { name: 'GLM', baseUrl: 'https://open.bigmodel.cn/api/anthropic', defaultModel: 'glm-5.2', smallFastModel: 'glm-5.2', protocol: 'anthropic' },
  deepseek: { name: 'DeepSeek', baseUrl: 'https://api.deepseek.com/anthropic', defaultModel: 'deepseek-v4-pro', smallFastModel: 'deepseek-v4-flash', protocol: 'anthropic' },
  mimo: { name: 'MiMo', baseUrl: 'https://token-plan-sgp.xiaomimimo.com/anthropic', defaultModel: 'mimo-v2.5-pro', smallFastModel: 'mimo-v2.5', protocol: 'anthropic' },
  kimi: { name: 'Kimi', baseUrl: 'https://api.kimi.com/coding', defaultModel: 'kimi-for-coding', smallFastModel: 'kimi-for-coding', protocol: 'anthropic' },
  minimax: { name: 'MiniMax', baseUrl: 'https://api.minimax.io/anthropic', defaultModel: 'MiniMax-M3', smallFastModel: 'MiniMax-M3', protocol: 'anthropic' },
  nvidia: { name: 'NVIDIA', baseUrl: 'https://integrate.api.nvidia.com/v1', defaultModel: 'z-ai/glm-5.2', smallFastModel: 'z-ai/glm-5.2', protocol: 'openai-chat' },
  openai: { name: 'OpenAI', baseUrl: 'https://api.openai.com/v1', defaultModel: 'gpt-5.2', smallFastModel: 'gpt-5.2-mini', protocol: 'openai-responses' },
  openrouter: { name: 'OpenRouter', baseUrl: 'https://openrouter.ai/api/v1', defaultModel: '', smallFastModel: '', protocol: 'openai-chat' },
  custom: { name: '', baseUrl: '', defaultModel: '', smallFastModel: '', protocol: 'anthropic' },
};
const PRESET_LABELS = { glm: 'GLM', deepseek: 'DeepSeek', mimo: 'MiMo', kimi: 'Kimi', minimax: 'MiniMax', nvidia: 'NVIDIA', openai: 'OpenAI', openrouter: 'OpenRouter', custom: '自定义' };

/* ---------- helpers ---------- */
function escapeHtml(s) {
  return String(s == null ? '' : s).replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
}
// Unified 24-hour clock (HH:MM:SS) everywhere — language-independent, so the monitor stays
// consistent across language switches and the timestamp never wraps with a 12-hour "PM" suffix.
function fmtTime(d) {
  const t = d == null ? new Date() : (d instanceof Date ? d : new Date(d));
  const p = (n) => String(n).padStart(2, '0');
  return `${p(t.getHours())}:${p(t.getMinutes())}:${p(t.getSeconds())}`;
}
function fmtNum(n) {
  n = n || 0;
  if (n < 1000) return String(n);
  if (n < 1e6) return (n / 1e3).toFixed(n < 1e4 ? 1 : 0).replace(/\.0$/, '') + 'K';
  if (n < 1e9) return (n / 1e6).toFixed(n < 1e7 ? 1 : 0).replace(/\.0$/, '') + 'M';
  return (n / 1e9).toFixed(1).replace(/\.0$/, '') + 'B';
}
// Smooth brand-tinted area sparkline; stretches to its container via preserveAspectRatio="none".
function sparkSVG(vals) {
  const W = 300, H = 46, pad = 4;
  const data = (vals && vals.length) ? vals : [0, 0];
  const n = data.length;
  const max = Math.max(1, ...data);
  const xs = (i) => (n === 1 ? W / 2 : pad + (i / (n - 1)) * (W - 2 * pad));
  const ys = (v) => H - pad - (v / max) * (H - 2 * pad - 2);
  const pts = data.map((v, i) => [xs(i), ys(v)]);
  let line = `M ${pts[0][0].toFixed(1)} ${pts[0][1].toFixed(1)}`;
  for (let i = 0; i < pts.length - 1; i++) {
    const p0 = pts[i - 1] || pts[i], p1 = pts[i], p2 = pts[i + 1], p3 = pts[i + 2] || p2;
    const c1x = p1[0] + (p2[0] - p0[0]) / 6, c1y = p1[1] + (p2[1] - p0[1]) / 6;
    const c2x = p2[0] - (p3[0] - p1[0]) / 6, c2y = p2[1] - (p3[1] - p1[1]) / 6;
    line += ` C ${c1x.toFixed(1)} ${c1y.toFixed(1)}, ${c2x.toFixed(1)} ${c2y.toFixed(1)}, ${p2[0].toFixed(1)} ${p2[1].toFixed(1)}`;
  }
  const area = `${line} L ${pts[n - 1][0].toFixed(1)} ${H - pad} L ${pts[0][0].toFixed(1)} ${H - pad} Z`;
  return `<svg viewBox="0 0 ${W} ${H}" preserveAspectRatio="none" style="width:100%;height:100%;display:block"><defs><linearGradient id="hsg" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="var(--brand)" stop-opacity="0.30"/><stop offset="1" stop-color="var(--brand)" stop-opacity="0"/></linearGradient></defs><path d="${area}" fill="url(#hsg)"/><path d="${line}" fill="none" stroke="var(--brand)" stroke-width="2" stroke-linejoin="round" stroke-linecap="round" vector-effect="non-scaling-stroke"/></svg>`;
}
const activeProvider = () => config.providers.find((p) => p.id === config.activeProviderId) || null;
function hashHue(s) { let h = 0; for (let i = 0; i < (s || '').length; i++) h = (h * 31 + s.charCodeAt(i)) % 360; return h; }
// Deterministic "random" emoji set — a custom provider with no brand logo gets a stable one.
const ICON_EMOJIS = ['🤖', '🧠', '⚡', '🚀', '🦊', '🐳', '🌟', '💎', '🔮', '🎯', '🛰️', '🧩', '🔆', '🌀', '🦁', '🐲', '🦄', '🍀', '🔥', '❄️', '🌈', '🎨', '🧪', '📡', '🛡️', '🎲', '🌶️', '🦉', '🐙', '🪐', '✨', '🌊'];
function emojiIcon(emoji, name) {
  const h = hashHue(name || '?');
  return { style: `background: linear-gradient(135deg, hsl(${h},62%,56%), hsl(${(h + 45) % 360},68%,46%))`, html: `<span class="prov-emoji">${escapeHtml(emoji)}</span>` };
}
// icon (optional): a user-set image (data:/http) or emoji; otherwise brand logo, else a default emoji.
function renderProviderIcon(name, icon) {
  if (icon && typeof icon === 'string') {
    if (/^(data:|https?:|assets\/)/.test(icon)) return { style: 'background: transparent; box-shadow: none;', html: `<img src="${escapeHtml(icon)}" class="prov-svg" alt="" style="width:100%;height:100%;object-fit:cover;display:block" />` };
    return emojiIcon(icon, name); // a chosen emoji
  }
  const n = (name || '').trim().toLowerCase();
  const brand = { kimi: ['kimi', 'moonshot', '月之'], deepseek: ['deepseek'], zhipu: ['glm', '智谱', 'bigmodel'], xiaomi: ['mimo', '小米', 'xiaomi'], zenmux: ['zenmux'], minimax: ['minimax', 'mini max', '海螺'], nvidia: ['nvidia'] };
  for (const file in brand) {
    // object-fit:contain keeps non-square logos from being stretched into the square icon slot.
    if (brand[file].some((k) => n.includes(k))) return { style: 'background: transparent; box-shadow: none;', html: `<img src="assets/${file}.svg" class="prov-svg" alt="" style="width:100%;height:100%;display:block;object-fit:contain" />` };
  }
  if (n.includes('claude') || n.includes('anthropic')) {
    const h = hashHue(name || '?');
    return { style: `background: linear-gradient(135deg, hsl(28,70%,48%), hsl(${(h + 40) % 360},75%,45%))`, html: `<svg class="prov-svg" aria-hidden="true" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M12 2L3 7v10l9 5 9-5V7l-9-5z" stroke-linecap="round"/><path d="M12 2v20 M3 12h18" stroke-linecap="round"/></svg>` };
  }
  return emojiIcon(ICON_EMOJIS[hashHue(name || '?') % ICON_EMOJIS.length], name); // default: deterministic emoji
}
function mask(t) { return !t ? I18n.t('providers.noKey') : t.length <= 10 ? '••••' : t.slice(0, 4) + '••••' + t.slice(-4); }

/* ---------- hero / status ---------- */
function showHeroNote(text, warn) {
  const n = $('heroNote');
  if (n) {
    n.textContent = text;
    n.classList.remove('hidden');
    n.classList.toggle('warn', !!warn);
  }
}
function hideHeroNote() {
  const n = $('heroNote');
  if (n) {
    n.classList.add('hidden');
  }
}

// Floating toast — sits above modals/drawers (z above everything) so a result is never
// hidden by a scrolled-out container. type: 'ok' | 'err' | 'pending'. Click to dismiss.
// Uses inline styles (not Tailwind utilities) so it renders correctly regardless of the
// compiled CSS state.
function ensureToastHost() {
  let host = document.getElementById('toastHost');
  if (!host) {
    host = document.createElement('div');
    host.id = 'toastHost';
    // Toast text can carry backend error strings (paths, upstream URLs) — keep it out of Clarity replays.
    host.setAttribute('data-clarity-mask', 'true');
    host.style.cssText = 'position:fixed;top:20px;left:50%;transform:translateX(-50%);z-index:9999;display:flex;flex-direction:column;align-items:center;gap:8px;pointer-events:none;';
    document.body.appendChild(host);
  }
  return host;
}
function showToast(text, type, opts) {
  opts = opts || {};
  const host = ensureToastHost();
  const bg = type === 'ok' ? 'var(--green)' : type === 'err' ? 'var(--red)' : 'var(--primary)';
  const el = document.createElement('div');
  el.style.cssText = `pointer-events:auto;max-width:min(520px,90vw);padding:10px 16px;border-radius:10px;background:${bg};color:#fff;font-size:13px;font-weight:600;line-height:1.5;word-break:break-word;cursor:pointer;box-shadow:0 8px 28px rgba(17,24,39,0.22);animation:panelIn 0.18s cubic-bezier(0.23,1,0.32,1);`;
  el.textContent = text;
  const dismiss = () => {
    if (el._gone) return;
    el._gone = true;
    clearTimeout(el._t);
    el.style.transition = 'opacity 0.18s ease, transform 0.18s ease';
    el.style.opacity = '0';
    el.style.transform = 'translateY(-6px)';
    setTimeout(() => el.remove(), 180);
  };
  el.addEventListener('click', dismiss);
  el.dismiss = dismiss;
  host.appendChild(el);
  // pending toasts stay until explicitly replaced; results auto-dismiss (errors linger longer).
  const ttl = opts.ttl != null ? opts.ttl : (type === 'pending' ? 0 : type === 'err' ? 6000 : 3500);
  if (ttl) el._t = setTimeout(dismiss, ttl);
  return el;
}

let heroRange = '30d';
async function renderHeroUsage() {
  const wrap = $('heroUsage');
  if (!wrap || !api.usageGet) return;
  let u; try { u = await api.usageGet(heroRange); } catch (_) { return; }
  if (!u) return;
  const port = (status.running && status.port) || config.port;
  const ep = $('heroEndpointText'); if (ep) ep.textContent = `localhost:${port}`;
  // a11y: the button's accessible name must contain its visible text (localhost:port).
  const epBtn = $('heroEndpoint'); if (epBtn) epBtn.setAttribute('aria-label', `localhost:${port} · ${I18n.t('hero.copyEndpoint')}`);
  const tk = $('heroTokens'); if (tk) tk.textContent = fmtNum(u.tokens || 0);
  const rq = $('heroReqs'); if (rq) rq.textContent = I18n.t('hero.reqsN', { n: (u.requests || 0).toLocaleString() });
  const md = $('heroModel'); if (md) md.textContent = u.favoriteModel && u.favoriteModel !== '—' ? `· ${u.favoriteModel}` : '';
  const days = heroRange === '7d' ? 7 : heroRange === '30d' ? 30 : 90;
  const series = (u.heatmap || []).slice(-days).map((c) => c.tokens || 0);
  const sp = $('heroSpark'); if (sp) sp.innerHTML = sparkSVG(series);
}
let _heroUsageT = null;
function scheduleHeroUsage() {
  clearTimeout(_heroUsageT);
  _heroUsageT = setTimeout(() => { const w = $('heroUsage'); if (status.connected && w && !w.classList.contains('hidden')) renderHeroUsage(); }, 2500);
}

// Hero state = the gateway SERVICE (running/stopped). The button stays the config-file action
// ("一键接入"/"断开" writes or restores the CLIs' configs) — independent of the service switch.
function renderHero() {
  const hero = $('hero');
  const ap = activeProvider();
  $('btnConnect').textContent = I18n.t(status.running ? 'hero.stopSvc' : 'hero.startSvc');
  if (status.running) {
    hero.classList.add('connected');
    const icon = $('heroIcon');
    if (ap) { const pi = renderProviderIcon(ap.name, ap.icon); icon.setAttribute('style', pi.style || ''); icon.innerHTML = pi.html; }
    else { icon.removeAttribute('style'); icon.innerHTML = I.connected || ''; }
    $('heroTitle').textContent = ap ? ap.name : I18n.t('hero.running');
    $('heroSub').innerHTML = ap ? I18n.t('hero.connectedVia', { name: escapeHtml(ap.name) }) : I18n.t('hero.running');
    hideHeroNote();
    $('heroUsage').classList.remove('hidden');
    renderHeroUsage();
  } else {
    hero.classList.remove('connected');
    const icon = $('heroIcon');
    icon.removeAttribute('style');
    icon.innerHTML = I.connect || '';
    $('heroTitle').textContent = I18n.t('hero.titleIdle');
    $('heroSub').textContent = I18n.t('hero.subIdle');
    hideHeroNote();
    $('heroUsage').classList.add('hidden');
  }
}

function renderStatus() {
  const chip = $('statusPill');
  if (chip) {
    chip.classList.toggle('on', !!status.running);
    const txt = chip.querySelector('.status-text');
    if (txt) {
      txt.textContent = I18n.t(status.running ? 'status.gwRunning' : 'status.gwStopped');
    }
    const bt = $('brandTitle');
    if (bt) {
      bt.classList.toggle('running', !!status.running);
    }
  }
}

// Multi-select for which coding CLIs "一键接入" wires to the gateway. Each toggle reflects
// config.connectTargets; each row's chip shows that CLI's live connected state. Codex is disabled
// (with a note) until it's installed.
function renderConnectTargets() {
  // The switch reflects the ACTUAL connection (a live on/off), not just the saved selection.
  const cc = $('fTargetClaude'), cx = $('fTargetCodex');
  if (cc) cc.checked = !!status.connectedClaude;
  if (cx) cx.checked = !!status.connectedCodex;
  const codexOk = status.codexAvailable !== false;
  const row = $('targetCodexRow'), note = $('targetCodexNote');
  if (cx) cx.disabled = !codexOk;
  if (row) row.style.opacity = codexOk ? '1' : '0.55';
  if (note) note.style.display = codexOk ? 'none' : '';
  const chip = (el, on) => { if (!el) return; el.className = 'proto-badge ' + (on ? 'proto-badge-xlate' : 'proto-badge-direct'); el.textContent = I18n.t(on ? 'settings.targetOn' : 'settings.targetOff'); };
  chip($('tgtClaudeChip'), !!status.connectedClaude);
  chip($('tgtCodexChip'), !!status.connectedCodex);
}
// Live per-CLI switch: flipping a target immediately connects/disconnects that CLI (and starts or
// stops the gateway as needed), so unchecking Claude Code actually turns it off.
async function toggleTarget(target, on) {
  if (!api.setConnectTarget) return;
  let res;
  try { res = await api.setConnectTarget(target, on); } catch (_) { res = null; }
  if (res && res.ok === false) {
    // couldn't turn on (no provider / port) → revert the switch + surface the reason
    const msg = res.reason === 'noProvider' ? I18n.t('settings.desktopNoProvider') : (res.message || I18n.t('err.opFailed'));
    try { showHeroNote(msg, true); } catch (_) {}
    const el = target === 'codex' ? $('fTargetCodex') : $('fTargetClaude');
    if (el) el.checked = !on;
    return;
  }
  config = await api.getConfig();
  status = await api.serverStatus();
  renderAll();
}
function renderConnect() {
  const port = (status.running && status.port) || config.port;
  $('endpoint').textContent = `http://localhost:${port}`;
  $('portInput').value = config.port;
  const token = config.requireToken && config.gatewayToken ? config.gatewayToken : 'ccbud-local';
  $('exportBlock').textContent = [
    `export ANTHROPIC_BASE_URL=http://localhost:${port}`,
    `export ANTHROPIC_AUTH_TOKEN=${token}`,
    '',
    I18n.t('settings.exportHint'),
  ].join('\n');
  $('claudePath').textContent = status.claudePath ? I18n.t('settings.claudePath') + status.claudePath : '';
  $('fOpenAtLogin').checked = !!config.openAtLogin;
  $('fRequireToken').checked = !!config.requireToken;
  $('fGatewayToken').value = config.gatewayToken || '';
  $('tokenRow').classList.toggle('hidden', !config.requireToken);
  if ($('fGatewayEnabled')) $('fGatewayEnabled').checked = status.gatewayEnabled !== false;
  if ($('fRetry429')) $('fRetry429').checked = !(config.retry429 && config.retry429.enabled === false);
  if ($('fInsecureTls')) $('fInsecureTls').checked = !!config.insecureSkipVerify;
  renderConnectTargets();
  const tu = config.trayUsage || { enabled: false, range: '7d' };
  $('fTrayUsage').checked = !!tu.enabled;
  $('fTrayRange').value = tu.range || '7d';
  $('trayRangeRow').classList.toggle('hidden', !tu.enabled);
  if ($('fLang')) $('fLang').value = config.language || I18n.lang;
  const se = $('startError');
  if (status.lastStartError) { se.textContent = status.lastStartError; se.classList.remove('hidden'); }
  else se.classList.add('hidden');
  renderHistoryDirs();
  renderDesktopCard();
}

/* ---------- Claude Desktop ("Third-Party Inference") integration ---------- */
async function renderDesktopCard() {
  const card = $('desktopCard');
  if (!card || !api.desktopStatus) return;
  let st;
  try { st = await api.desktopStatus(); } catch (_) { return; }
  const chip = $('desktopStatusChip');
  const btn = $('btnDesktopConnect');
  const note = $('desktopNote');
  if (!chip || !btn || !note) return;
  chip.className = 'desktop-chip text-[10.5px] font-semibold rounded-full px-2 py-0.25 ' + (st.connected ? 'bg-green-soft text-green' : 'bg-chip-bg text-muted');
  if (!st.supported || !st.installed) {
    card.classList.add('opacity-60');
    btn.disabled = true;
    btn.dataset.connected = '';
    btn.textContent = I18n.t('settings.desktopConnect');
    chip.textContent = I18n.t(st.supported ? 'settings.desktopNotInstalled' : 'settings.desktopUnsupported');
    delete note.dataset.transient;
    note.textContent = I18n.t(st.supported ? 'settings.desktopNote' : 'settings.desktopUnsupportedNote');
    return;
  }
  card.classList.remove('opacity-60');
  btn.disabled = false;
  btn.dataset.connected = st.connected ? '1' : '';
  btn.textContent = I18n.t(st.connected ? 'settings.desktopRestore' : 'settings.desktopConnect');
  chip.textContent = I18n.t(st.connected ? 'settings.desktopConnected' : 'settings.desktopDisconnected');
  // The post-action guidance (steps / restored) is marked transient so a status re-render
  // doesn't wipe it before the user has gone to System Settings.
  if (st.connected) { delete note.dataset.transient; note.textContent = I18n.t('settings.desktopConnectedNote'); }
  else if (!note.dataset.transient) { note.textContent = I18n.t('settings.desktopNote'); }
}

// The Claude Desktop status is read from the installed system profile (not a live event), so
// poll lightly while the Settings view is open — picks up an install/removal within a few seconds.
let desktopPollTimer = null;
function startDesktopPoll() {
  stopDesktopPoll();
  renderDesktopCard();
  desktopPollTimer = setInterval(() => { renderDesktopCard(); }, 4000);
}
function stopDesktopPoll() {
  if (desktopPollTimer) { clearInterval(desktopPollTimer); desktopPollTimer = null; }
}

async function renderHistoryDirs() {
  const host = $('histDirList');
  if (!host) return;
  let data; try { data = await api.historyDirs(); } catch (_) { data = { dirs: [] }; }
  // The synthetic buckets (导入 store / 回收站) are app-managed, not user work dirs — keep
  // them out of this list.
  const dirs = (data.dirs || []).filter((d) => !d.imported && !d.trash);
  host.innerHTML = dirs.map((d) => {
    const status = d.exists === false
      ? `<span class="hist-dir-warn text-red font-semibold text-[11px] shrink-0 bg-red-soft px-2.5 py-0.75 rounded-full" title="${escapeHtml(I18n.t('settings.dirMissing'))}">${escapeHtml(I18n.t('settings.dirMissing'))}</span>`
      : `<span class="hist-dir-count text-brand font-semibold text-[11px] shrink-0 bg-brand-soft px-2.5 py-0.75 rounded-full">${escapeHtml(I18n.t('settings.sessions', { n: d.sessions }))}</span>`;
    return `<div class="hist-dir-row flex items-center gap-3.5 p-3 px-4 bg-bg-elev border border-border-custom rounded-md text-[13px] shadow-sm transition-all duration-200 ease-out hover:-translate-y-0.5 hover:border-border-strong hover:shadow-card-hover [.missing_&]:opacity-70 [.missing_&_.hist-dir-label]:text-muted [.missing_&_.hist-dir-label]:line-through${d.exists === false ? ' missing' : ''}">
      <span class="hist-dir-label flex-1 font-mono truncate text-fg text-[12.5px]" title="${escapeHtml(d.projectsDir || '')}">${escapeHtml(d.label)}</span>
      ${status}
      <button class="hist-dir-del w-7 h-7 border-0 rounded-full bg-transparent text-muted cursor-pointer flex items-center justify-center shrink-0 transition-colors duration-200 hover:enabled:text-red hover:enabled:bg-red-soft disabled:opacity-25 disabled:cursor-not-allowed" data-del-dir="${escapeHtml(d.id)}" title="${escapeHtml(I18n.t('providers.remove'))}"${d.id === '~/.claude' ? ' disabled' : ''}>${I.trash || '⌫'}</button>
    </div>`;
  }).join('') || `<div class="caption text-caption text-xs">${escapeHtml(I18n.t('settings.none'))}</div>`;
}
async function addHistDirPath(v) {
  v = (v || '').trim();
  if (!v) return;
  const dirs = (config.historyDirs || []).slice();
  if (!dirs.includes(v)) dirs.push(v);
  await persist({ historyDirs: dirs });
}
async function pickHistDir() {
  if (!api.historyPickDir) return;
  let res; try { res = await api.historyPickDir(); } catch (_) { return; }
  if (!res || res.canceled || !res.path) return;
  await addHistDirPath(res.path);
}

function renderProviders() {
  const list = $('providerList');
  list.innerHTML = '';
  $('emptyProviders').classList.toggle('hidden', config.providers.length > 0);
  for (const p of config.providers) {
    const isActive = p.id === config.activeProviderId;
    const el = document.createElement('div');
    el.className = 'provider group grid grid-cols-[14px_36px_1fr_minmax(72px,auto)_auto] items-center gap-3 p-2.5 pr-3.5 pl-2.5 min-h-[60px] bg-bg-elev border border-border-custom rounded-[13px] shadow-card cursor-pointer relative transition-all duration-150 hover:border-border-strong hover:shadow-card-hover hover:-translate-y-0.25 [&.active]:border-green/38 [&.active]:bg-[color-mix(in_srgb,var(--bg-elev)_90%,var(--green)_10%)] [&.dragging]:opacity-40 [&.dragging]:scale-99 [&.drag-over]:border-brand [&.drag-over]:bg-brand-soft ' + (isActive ? 'active' : '');
    el.draggable = true;
    el.dataset.id = p.id;

    const tags = [];
    if (p.defaultModel) tags.push(`<span class="tag text-[11px] font-mono bg-chip-bg rounded-[4px] px-1.5 py-0.25 text-fg whitespace-nowrap">${escapeHtml(I18n.t('providers.tagMain'))} ${escapeHtml(p.defaultModel)}</span>`);
    if (p.smallFastModel && p.smallFastModel !== p.defaultModel) tags.push(`<span class="tag text-[11px] font-mono bg-chip-bg rounded-[4px] px-1.5 py-0.25 text-fg whitespace-nowrap">${escapeHtml(I18n.t('providers.tagFast'))} ${escapeHtml(p.smallFastModel)}</span>`);
    for (const m of p.models || []) tags.push(`<span class="tag map text-[11px] font-mono bg-brand-soft rounded-[4px] px-1.5 py-0.25 text-brand font-medium whitespace-nowrap" title="${escapeHtml(m.alias)} → ${escapeHtml(m.upstream)}">${escapeHtml(m.alias)} → ${escapeHtml(m.upstream)}</span>`);

    const iconData = renderProviderIcon(p.name, p.icon);
    // Protocol badge so the wire protocol (and whether requests are translated) is visible at a
    // glance on every provider. Anthropic (passthrough) is the quiet default; the translated ones
    // stand out.
    const proto = p.protocol || 'anthropic';
    const protoMeta = proto === 'openai-chat'
      ? { label: 'OpenAI Chat', cls: 'proto-badge-xlate' }
      : proto === 'openai-responses'
        ? { label: 'OpenAI Responses', cls: 'proto-badge-xlate' }
        : { label: 'Anthropic', cls: 'proto-badge-direct' };
    const protoBadge = `<span class="proto-badge ${protoMeta.cls}" title="${escapeHtml(I18n.t('providers.protocolTip'))}">${escapeHtml(protoMeta.label)}</span>`;
    el.innerHTML = `
      <span class="grip text-caption cursor-grab text-[12px] opacity-30 leading-none select-none group-hover:opacity-65 transition-opacity duration-150" title="${escapeHtml(I18n.t('providers.reorder'))}">⠿</span>
      <div class="prov-icon w-9 h-9 rounded-[9px] shrink-0 flex items-center justify-center text-white font-bold text-[13px] tracking-tight shadow-sm" style="${iconData.style}">${iconData.html}</div>
      <div class="pinfo min-w-0">
        <div class="pname flex items-center gap-1.5 font-semibold text-[14.5px] tracking-tight text-fg">${escapeHtml(p.name)} ${protoBadge} ${isActive ? '<span class="badge-active text-[10.5px] font-semibold text-green bg-green-soft rounded-full px-1.75 py-0.25">' + escapeHtml(I18n.t('providers.active')) + '</span>' : ''}</div>
        <div class="pmeta mt-0.5 text-xs font-mono text-caption truncate">${escapeHtml(mask(p.authToken))} · ${escapeHtml(p.baseUrl.replace(/^https?:\/\//,''))}</div>
      </div>
      <div class="pmodels flex gap-1 flex-wrap justify-end max-w-[340px]">${tags.join('') || '<span class="caption text-caption text-xs">—</span>'}</div>
      <div class="pactions flex gap-0.25">
        <button class="w-6.5 h-6.5 border-0 rounded-[6px] bg-transparent text-muted cursor-pointer flex items-center justify-center transition-all duration-100 hover:bg-chip-bg hover:text-fg" title="${escapeHtml(I18n.t('providers.test'))}" data-test="${p.id}">${I.refresh || '↻'}</button>
        <button class="w-6.5 h-6.5 border-0 rounded-[6px] bg-transparent text-muted cursor-pointer flex items-center justify-center transition-all duration-100 hover:bg-chip-bg hover:text-fg" title="${escapeHtml(I18n.t('providers.edit'))}" data-edit="${p.id}">${I.edit || '✎'}</button>
        <button class="w-6.5 h-6.5 border-0 rounded-[6px] bg-transparent text-muted cursor-pointer flex items-center justify-center transition-all duration-100 hover:bg-red-soft hover:text-red danger" title="${escapeHtml(I18n.t('providers.delete'))}" data-del="${p.id}">${I.trash || '⌫'}</button>
      </div>`;
    list.appendChild(el);
  }
}

function renderMonitor() {
  $('mStatusText').textContent = status.connected ? I18n.t('status.connected') : status.running ? I18n.t('status.running') : I18n.t('status.disconnected');
  const dot = $('mStatus').querySelector('.pulse-dot, .live-dot');
  if (dot) {
    const isLive = !!(status.connected || status.running);
    dot.classList.toggle('on', isLive);
    dot.classList.toggle('off', !isLive);
  }
  $('mEndpoint').textContent = `localhost:${(status.running && status.port) || config.port}`;
  const ap = activeProvider();
  $('mActive').textContent = ap ? ap.name : '—';
  $('mActiveUrl').textContent = ap ? ap.baseUrl : I18n.t('monitor.noService');
  $('mTotal').textContent = stats.total;
  $('mSuccess').textContent = stats.total ? I18n.t('monitor.successRate', { pct: Math.round((stats.ok / stats.total) * 100) }) : I18n.t('monitor.successRateNone');
  $('mAvg').innerHTML = stats.total ? `${Math.round(stats.sumMs / stats.total)} <span class="unit">ms</span>` : `— <span class="unit">ms</span>`;
  $('mLast').textContent = stats.last ? I18n.t('monitor.recent', { time: stats.last }) : I18n.t('monitor.recentNone');
  renderGwLogStatus();
}

function renderAll() { renderStatus(); renderHero(); renderConnect(); renderProviders(); renderMonitor(); }

/* ---------- in-app updates ---------- */
let updateState = null;
let updateBusy = false;
function show(el, on) { if (el) el.classList.toggle('hidden', !on); }
function renderUpdate() {
  const s = updateState;
  const verEl = $('updVersion'), latEl = $('updLatest'), stEl = $('updStatus'), chip = $('updateChip');
  const actions = $('updActions'), bDl = $('btnUpdateDownload'), bApply = $('btnUpdateApply'),
    bOpen = $('btnUpdateOpen'), bBrew = $('btnUpdateBrew'), notes = $('updNotes');
  if (!verEl) return;
  if (s) {
    verEl.textContent = s.runningVersion || s.shellVersion || '—';
    latEl.textContent = s.latestVersion || '—';
  }
  // reset
  [bDl, bApply, bOpen, bBrew].forEach((b) => show(b, false));
  show(actions, false); show(notes, false); show(chip, false);
  if (chip) chip.classList.remove('text-green', 'text-amber');

  if (!s) { stEl.textContent = I18n.t('about.idle'); return; }
  const staged = s.pending && s.pending.staged;
  if (staged) {
    stEl.textContent = I18n.t('about.stagedReady', { v: s.pending.version });
    chip.textContent = I18n.t('about.ready'); chip.classList.add('text-green'); show(chip, true);
    show(actions, true); show(bApply, true);
    return;
  }
  if (s.ok === false) { stEl.textContent = I18n.t('about.checkFailed', { msg: s.error || '' }); return; }
  if (!s.latestVersion || s.mode === 'unknown') { stEl.textContent = I18n.t('about.idle'); return; }
  if (s.mode === 'none') { stEl.textContent = I18n.t('about.upToDate'); chip.textContent = I18n.t('about.upToDateChip'); chip.classList.add('text-green'); show(chip, true); return; }

  // an update is available
  chip.textContent = I18n.t('about.availableChip'); chip.classList.add('text-amber'); show(chip, true);
  show(actions, true);
  if (s.notes) { notes.textContent = s.notes; show(notes, true); }
  if (s.mode === 'hot') {
    stEl.textContent = updateBusy ? I18n.t('about.downloading') : I18n.t('about.hotAvailable', { v: s.latestVersion });
    show(bDl, true); bDl.disabled = updateBusy; bDl.textContent = updateBusy ? I18n.t('about.downloading') : I18n.t('about.downloadInstall');
  } else { // full
    stEl.textContent = I18n.t('about.fullAvailable', { v: s.latestVersion });
    show(bOpen, true);
    if (s.installMethod === 'mac' || s.installMethod === 'linux') { bBrew.textContent = s.brewCommand || 'brew upgrade --cask ccbud'; show(bBrew, true); }
  }
}
async function loadUpdateState() {
  try { updateState = await api.updateState(); } catch (_) {}
  syncAutoToggles();
  renderUpdate();
}
async function checkUpdate() {
  const btn = $('btnUpdateCheck');
  if (btn) { btn.disabled = true; }
  $('updStatus').textContent = I18n.t('about.checking');
  try { updateState = await api.updateCheck(); } catch (e) { updateState = { ok: false, error: (e && e.message) || '' }; }
  if (btn) btn.disabled = false;
  renderUpdate();
}
async function downloadUpdate() {
  updateBusy = true; renderUpdate();
  let res;
  try { res = await api.updateDownload(); } catch (e) { res = { ok: false, error: (e && e.message) || '' }; }
  updateBusy = false;
  try { updateState = await api.updateState(); } catch (_) {}
  if (res && !res.ok && updateState) updateState.error = res.error;
  renderUpdate();
}
function syncAutoToggles() {
  const au = (config && config.autoUpdate) || {};
  const c = $('fAutoCheck'), d = $('fAutoDownload');
  if (c) c.checked = au.check !== false;
  if (d) d.checked = au.autoDownload !== false;
}

/* ---------- monitor stream ---------- */
function pushStreamRow(r) {
  stats.total++;
  if (r.status >= 200 && r.status < 400) stats.ok++;
  stats.sumMs += r.ms || 0;
  stats.last = fmtTime();
  renderMonitor();
  $('streamHint').textContent = I18n.t('monitor.forwarded', { n: stats.total });
  const list = $('streamList');
  const empty = list.querySelector('.state-inline, .empty');
  if (empty) empty.remove();
  const okCls = r.status >= 200 && r.status < 400 ? 'ok' : 'err';
  const row = document.createElement('div');
  row.className = 'stream-row flex items-center gap-2.5 py-2.25 px-3.5 border-b border-border-custom text-[11.5px] transition-colors duration-100 hover:bg-chip-bg last:border-b-0 [&.clickable]:cursor-pointer';
  if (r.id != null) { row.dataset.id = r.id; row.classList.add('clickable'); row.title = I18n.t('monitor.rowTitle'); }
  const agentTag = r.agentId ? `<span class="agent-tag sub text-[9px] font-bold px-1 py-0.25 rounded-[4px] leading-[1.2] shrink-0 text-muted bg-chip-bg border border-border-custom" title="${escapeHtml(I18n.t('monitor.subReq'))}">sub</span>` : '';
  row.innerHTML = `
    <span class="sdot w-1.5 h-1.5 rounded-full shrink-0 [&.ok]:bg-green [&.err]:bg-red ${okCls}"></span>
    <span class="method font-mono text-[10px] text-caption w-10">${escapeHtml(r.method || '')}</span>
    ${agentTag}
    <span class="models flex-1 min-w-0 font-mono text-[11px] flex items-center gap-1.25 overflow-hidden">
      <span class="req text-fg truncate" title="HTTP body.model">${escapeHtml(r.requestedModel || '-')}</span>
      <span class="arrow text-brand opacity-55">→</span>
      <span class="out text-muted truncate" title="${escapeHtml(I18n.t('monitor.upstreamModel'))}">${escapeHtml(r.outgoingModel || '-')}</span>
      ${r.rewritten ? `<span class="rewrite text-brand text-[10px]" title="${escapeHtml(I18n.t('monitor.rewriteTitle'))}">✎</span>` : ''}
    </span>
    <span class="prov text-caption text-[11px] max-w-[100px] truncate">${escapeHtml(r.provider || '')}</span>
    <span class="code font-mono font-semibold w-8.5 text-right [&.ok]:text-green [&.err]:text-red ${okCls}">${r.status}</span>
    <span class="ms font-mono text-caption w-12 text-right">${r.ms}ms</span>
    <span class="ts font-mono text-caption text-[10px] w-14 text-right">${fmtTime()}</span>`;
  list.insertBefore(row, list.firstChild);
  // Live window only — keep the last 100 rows (matches the backend's exchange-detail buffer).
  while (list.children.length > 100) list.removeChild(list.lastChild);
  scheduleHeroUsage();
}
/* ---------- gateway log (lifecycle + error events; backfilled from main's ring buffer) ---------- */
const gwLog = { seen: new Set(), items: [] };

// Add an entry to the local model. Live/replayed entries carry a `seq` (deduped); local renderer
// notices (provider test, save error, …) have none and are always appended.
function addGatewayLog(l) {
  if (!l) return false;
  if (l.seq != null) {
    if (gwLog.seen.has(l.seq)) return false;
    gwLog.seen.add(l.seq);
  }
  if (l.ts == null) l.ts = Date.now();
  gwLog.items.push(l);
  while (gwLog.items.length > 100) gwLog.items.shift();
  return true;
}

function renderGwLogStatus() {
  const el = $('gwLogStatus');
  if (!el) return;
  const running = !!(status.connected || status.running);
  const port = (status.running && status.port) || config.port;
  el.className = 'raw-log-badge ml-auto ' + (running ? 'on' : 'off');
  el.innerHTML = `<span class="rl-dot"></span>${escapeHtml(I18n.t(running ? 'monitor.gwRunning' : 'monitor.gwStopped'))} · localhost:${escapeHtml(String(port))}`;
}

function renderGatewayLog() {
  const el = $('rawLog');
  if (!el) return;
  if (!gwLog.items.length) {
    el.innerHTML = `<div class="raw-log-empty">${escapeHtml(I18n.t('monitor.logEmpty'))}</div>`;
    return;
  }
  const rows = gwLog.items.slice().sort((a, b) => (a.ts || 0) - (b.ts || 0)).reverse();
  el.innerHTML = rows.map((l) => {
    const lv = String(l.level || 'info');
    const t = fmtTime(l.ts);
    return `<div class="raw-log-line lv-${escapeHtml(lv)}"><span class="rl-lv">${escapeHtml(lv)}</span><span class="rl-msg">${escapeHtml(l.msg || '')}</span><span class="rl-t">${escapeHtml(t)}</span></div>`;
  }).join('');
}

function pushRawLog(l) { if (addGatewayLog(l)) renderGatewayLog(); }

// Backfill from main's ring buffer (events fire once and aren't otherwise replayed) + refresh banner.
async function refreshGatewayLog() {
  if (api.logsGet) {
    try { (await api.logsGet() || []).forEach(addGatewayLog); } catch (_) {}
  }
  renderGwLogStatus();
  renderGatewayLog();
}

/* ---------- request inspector (full headers + body of one forwarded exchange) ---------- */
let reqDrawerTab = 'req';
let reqDrawerData = null;

function fmtBytes(n) { n = n || 0; if (n < 1024) return n + ' B'; if (n < 1048576) return (n / 1024).toFixed(1) + ' KB'; return (n / 1048576).toFixed(2) + ' MB'; }
function prettyText(cap) {
  if (!cap || !cap.text) return { text: '', lang: 'plaintext' };
  let text = cap.text, lang = 'plaintext';
  const trimmed = text.trim();
  if (trimmed.startsWith('{') || trimmed.startsWith('[')) {
    try { text = JSON.stringify(JSON.parse(trimmed), null, 2); lang = 'json'; } catch (_) {}
  }
  return { text, lang };
}
function prettyBody(cap) {
  if (!cap || !cap.text) return `<div class="dr-empty p-2.5 text-caption text-[12.5px]">${escapeHtml(I18n.t('drawer.empty'))}</div>`;
  const { text, lang } = prettyText(cap);
  const note = cap.truncated ? `<div class="dr-trunc text-[11.5px] text-amber mb-1.5">${escapeHtml(I18n.t('drawer.truncated', { shown: fmtBytes(cap.bytes - cap.truncated), total: fmtBytes(cap.bytes) }))}</div>` : '';
  return note + `<pre class="dr-pre bg-[#f6f8fa] dark:bg-[#0c0e12] text-[#24292e] dark:text-[#e8edf4] border border-border-custom dark:border-white/8 rounded-sm p-3 overflow-x-auto text-xs leading-[1.55] max-h-[62vh]"><code class="language-${lang}">${escapeHtml(text)}</code></pre>`;
}
// In-body find bar (request/response body can be 100KB+) — highlight + navigate matches.
const DR_MARK_CAP = 800;
let drBodyText = '', drBodyHTML = '', drMatches = [], drMatchIdx = -1;
function drCodeEl() { const b = $('reqDrawerBody'); return b ? b.querySelector('.dr-pre code') : null; }
function updateDrCount() {
  const c = $('reqDrawerBody') && $('reqDrawerBody').querySelector('.dr-search-count');
  if (!c) return;
  const shown = Math.min(drMatches.length, DR_MARK_CAP);
  c.textContent = drMatches.length ? `${drMatchIdx + 1}/${shown}${drMatches.length > DR_MARK_CAP ? '+' : ''}` : '0/0';
}
function applyDrSearch(q) {
  const code = drCodeEl();
  if (!code) return;
  q = q || '';
  if (!q) { code.innerHTML = drBodyHTML; drMatches = []; drMatchIdx = -1; updateDrCount(); return; }
  const hay = drBodyText.toLowerCase(), needle = q.toLowerCase();
  drMatches = [];
  for (let i = hay.indexOf(needle); i !== -1; i = hay.indexOf(needle, i + needle.length)) drMatches.push(i);
  if (!drMatches.length) { code.innerHTML = escapeHtml(drBodyText); drMatchIdx = -1; updateDrCount(); return; }
  const n = Math.min(drMatches.length, DR_MARK_CAP);
  let html = '', last = 0;
  for (let k = 0; k < n; k++) {
    const pos = drMatches[k];
    html += escapeHtml(drBodyText.slice(last, pos)) + '<mark class="dr-mark">' + escapeHtml(drBodyText.slice(pos, pos + q.length)) + '</mark>';
    last = pos + q.length;
  }
  html += escapeHtml(drBodyText.slice(last));
  code.innerHTML = html;
  drMatchIdx = 0; drHighlightCurrent(); updateDrCount();
}
function drHighlightCurrent() {
  const code = drCodeEl();
  if (!code) return;
  const marks = code.querySelectorAll('.dr-mark');
  marks.forEach((m, i) => m.classList.toggle('cur', i === drMatchIdx));
  if (marks[drMatchIdx]) marks[drMatchIdx].scrollIntoView({ block: 'center' });
}
function drNavSearch(dir) {
  const n = Math.min(drMatches.length, DR_MARK_CAP);
  if (!n) return;
  drMatchIdx = (drMatchIdx + dir + n) % n;
  drHighlightCurrent(); updateDrCount();
}
function kvTable(h) {
  const keys = Object.keys(h || {});
  if (!keys.length) return `<div class="dr-empty p-2.5 text-caption text-[12.5px]">${escapeHtml(I18n.t('drawer.none'))}</div>`;
  return '<div class="dr-kv border border-border-custom rounded-sm overflow-hidden">' + keys.map((k) => `<div class="dr-kv-row flex gap-2.5 py-1.5 px-2.5 font-mono text-xs odd:bg-transparent even:bg-chip-bg"><span class="dr-k text-brand shrink-0 min-w-[150px] break-all">${escapeHtml(k)}</span><span class="dr-v text-fg break-all">${escapeHtml(Array.isArray(h[k]) ? h[k].join(', ') : h[k])}</span></div>`).join('') + '</div>';
}
// A translated exchange (client wire ≠ provider wire) exposes all four sides; passthrough keeps
// the classic two. Each tab resolves to { headers, cap, isReq, sub } for the shared body renderer:
//   creq — what the gateway RECEIVED from the client (inbound URL/headers/original body)
//   req  — what the gateway SENT upstream (real upstream URL/headers/translated body)
//   ures — what the upstream RETURNED (raw, pre-translation)
//   res  — what the gateway RETURNED to the client (translated)
function drawerTabView(d, tab) {
  const creq = d.clientReq || {};
  const ures = d.upstreamRes || {};
  switch (tab) {
    case 'creq':
      return { headers: creq.headers, cap: creq.body || d.reqBody, isReq: true, sub: `${d.method || 'POST'} ${creq.url || d.path || ''}` };
    case 'ures':
      return { headers: ures.headers, cap: ures.body, isReq: false, sub: `HTTP ${ures.status != null ? ures.status : d.status || ''}` };
    case 'res':
      return { headers: d.resHeaders, cap: d.resBody, isReq: false, sub: `HTTP ${d.status || ''}` };
    default: // 'req'
      return { headers: d.reqHeaders, cap: d.reqBody, isReq: true, sub: `${d.method || 'POST'} ${d.url || d.path || ''}` };
  }
}
function drawerTabs(d) {
  return d && d.translated
    ? [['creq', I18n.t('drawer.tabClientReq')], ['req', I18n.t('drawer.tabUpstreamReq')],
       ['ures', I18n.t('drawer.tabUpstreamRes')], ['res', I18n.t('drawer.tabClientRes')]]
    : [['req', I18n.t('drawer.req')], ['res', I18n.t('drawer.res')]];
}
const DR_TAB_CLS = 'dr-tab border-none bg-transparent text-muted font-semibold text-[13px] leading-none p-[8px_14px] rounded-t-md cursor-pointer border-b-2 border-transparent -mb-[1px] hover:text-fg [&.active]:text-brand [&.active]:border-b-brand';
function renderDrawerTabs() {
  const wrap = $('drTabs');
  if (!wrap) return;
  wrap.innerHTML = drawerTabs(reqDrawerData)
    .map(([k, label]) => `<button class="${DR_TAB_CLS}${k === reqDrawerTab ? ' active' : ''}" data-tab="${k}">${escapeHtml(label)}</button>`)
    .join('');
}
function renderReqDrawerBody() {
  const d = reqDrawerData;
  if (!d) return;
  const body = $('reqDrawerBody');
  const view = drawerTabView(d, reqDrawerTab);
  const isReq = view.isReq;
  const headers = view.headers;
  const cap = view.cap;
  const which = reqDrawerTab;
  const copyLabel = cap && cap.truncated ? I18n.t('drawer.copyPartial') : I18n.t('drawer.copy');
  const headTitle = `${escapeHtml(I18n.t(isReq ? 'drawer.reqHeaders' : 'drawer.resHeaders'))} <span class="dr-sub font-medium font-mono text-caption normal-case tracking-normal truncate">${escapeHtml(view.sub)}</span>`;
  drBodyText = prettyText(cap).text;
  drMatches = []; drMatchIdx = -1;
  const searchBar = drBodyText ? `<span class="flex-1"></span><div class="dr-search-wrap flex items-center gap-1 normal-case tracking-normal">
    <input class="dr-search w-[140px] bg-bg-input border border-border-custom rounded-md px-2 py-1 text-[11px] font-normal text-fg outline-none focus:border-primary" type="text" placeholder="${escapeHtml(I18n.t('drawer.searchBody'))}" />
    <span class="dr-search-count text-caption text-[10px] font-normal tabular-nums min-w-[36px] text-center">0/0</span>
    <button class="dr-search-prev tool-btn w-[22px] h-[22px] border border-border-custom rounded-[5px] bg-bg-elev text-muted cursor-pointer inline-flex items-center justify-center text-[11px] hover:text-fg hover:bg-chip-bg hover:border-border-strong" title="${escapeHtml(I18n.t('drawer.searchPrev'))}">↑</button>
    <button class="dr-search-next tool-btn w-[22px] h-[22px] border border-border-custom rounded-[5px] bg-bg-elev text-muted cursor-pointer inline-flex items-center justify-center text-[11px] hover:text-fg hover:bg-chip-bg hover:border-border-strong" title="${escapeHtml(I18n.t('drawer.searchNext'))}">↓</button>
  </div>` : '';
  const copyCls = drBodyText ? '' : 'ml-auto ';
  body.innerHTML = `<div class="dr-section-title flex items-center gap-2 text-xs font-bold text-fg my-4 mt-4 mb-2 uppercase tracking-wide">${headTitle}</div>${kvTable(headers)}<div class="dr-section-title flex items-center gap-2 text-xs font-bold text-fg my-4 mt-4 mb-2 uppercase tracking-wide">${escapeHtml(isReq ? I18n.t('drawer.reqBody') : I18n.t('drawer.resBody'))}${searchBar}<button class="btn btn-sm dr-copy ${copyCls}bg-bg-elev text-fg border border-border-custom rounded-[6px] px-2.25 py-1 text-[11px] font-medium normal-case tracking-normal cursor-pointer transition-all duration-150 hover:bg-chip-bg hover:border-border-strong active:scale-[0.98]" data-copy-body="${which}" title="${escapeHtml(I18n.t('drawer.copy'))}">${escapeHtml(copyLabel)}</button></div>${prettyBody(cap)}`;
  // Skip syntax highlighting on very large bodies — hljs on multi-MB text freezes the UI.
  body.querySelectorAll('pre code').forEach((b) => { if (b.textContent.length > 100000) return; try { if (window.hljs) window.hljs.highlightElement(b); } catch (_) {} });
  const codeEl = drCodeEl();
  drBodyHTML = codeEl ? codeEl.innerHTML : '';
}
async function openReqDetail(id) {
  let d = null;
  try { d = await api.monitorGet(id); } catch (_) {}
  if (!d) {
    // Entry rolled out of the bounded capture buffer — give feedback instead of a stale drawer.
    reqDrawerData = null;
    $('drMethod').textContent = '—';
    const drStatus = $('drStatus');
    if (drStatus) {
      drStatus.textContent = '';
      drStatus.classList.remove('ok', 'err');
    }
    $('drModel').textContent = '';
    $('reqMeta').innerHTML = '';
    $('reqDrawerBody').innerHTML = `<div class="dr-empty p-2.5 text-caption text-[12.5px]">${escapeHtml(I18n.t('drawer.expired'))}</div>`;
    $('reqDrawer').classList.remove('hidden');
    return;
  }
  // Translated exchanges open on the client request (what the gateway received) so the
  // before/after of the translation reads left-to-right across the tabs.
  reqDrawerData = d; reqDrawerTab = d.translated ? 'creq' : 'req';
  const ok = d.status >= 200 && d.status < 400;
  $('drMethod').textContent = d.method || 'POST';
  const drStatus = $('drStatus');
  if (drStatus) {
    drStatus.textContent = d.status != null ? d.status : '—';
    drStatus.classList.toggle('ok', ok);
    drStatus.classList.toggle('err', !ok);
  }
  $('drModel').innerHTML = `${escapeHtml(d.requestedModel || '-')} <span class="arrow">→</span> ${escapeHtml(d.outgoingModel || '-')}${d.rewritten ? ` <span class="rewrite" title="${escapeHtml(I18n.t('drawer.rewritten'))}">✎</span>` : ''}`;
  const meta = [
    [I18n.t('drawer.service'), d.provider],
    d.translated ? [I18n.t('drawer.translated'), d.translated] : null,
    d.aborted ? [I18n.t('drawer.aborted'), I18n.t('drawer.abortedVal')] : null,
    [I18n.t('drawer.latency'), d.ms != null ? d.ms + ' ms' : ''],
    [I18n.t('drawer.session'), d.sessionId ? String(d.sessionId).slice(0, 8) : ''],
    d.agentId ? [I18n.t('drawer.agent'), I18n.t('drawer.subagent')] : null,
    [I18n.t('drawer.time'), d.ts ? fmtTime(d.ts) : ''],
    d.error ? [I18n.t('drawer.error'), d.error] : null,
  ].filter((r) => r && r[1]);
  $('reqMeta').innerHTML = meta.map((r) => `<span class="dr-chip text-[11.5px] px-2.25 py-[2.5px] rounded-full bg-chip-bg text-fg"><span class="muted text-muted">${escapeHtml(r[0])}</span> ${escapeHtml(r[1])}</span>`).join('');
  renderDrawerTabs();
  renderReqDrawerBody();
  $('reqDrawer').classList.remove('hidden');
}
function closeReqDrawer() { const d = $('reqDrawer'); if (d) d.classList.add('hidden'); reqDrawerData = null; }

/* ---------- modal ---------- */
function renderPresetGrid() {
  const grid = $('presetGrid');
  grid.innerHTML = '';
  Object.keys(PRESET_LABELS).forEach((key) => {
    const b = document.createElement('button');
    b.type = 'button'; b.className = 'preset-chip bg-bg-input border border-border-custom rounded-full px-3 py-[4.5px] text-[12px] font-medium text-fg cursor-pointer transition-all duration-140 hover:border-brand hover:text-brand active:scale-[0.97]'; b.dataset.preset = key; b.textContent = key === 'custom' ? I18n.t('preset.custom') : PRESET_LABELS[key];
    grid.appendChild(b);
  });
}
function selectPreset(key) {
  document.querySelectorAll('.preset-chip').forEach((c) => c.classList.toggle('selected', c.dataset.preset === key));
  const p = PRESETS[key] || PRESETS.custom;
  $('fName').value = p.name; $('fBaseUrl').value = p.baseUrl; $('fDefaultModel').value = p.defaultModel; $('fSmallModel').value = p.smallFastModel;
  setProtocol(p.protocol || 'anthropic'); // preset declares its wire protocol up front
  modalIcon = null; // a preset uses its brand logo
  updateIconPreview();
  if (key !== 'custom') $('fToken').focus();
}
// Segmented protocol control: get/set the selected wire protocol.
function getProtocol() {
  const g = $('fProtocol'); if (!g) return 'anthropic';
  const b = g.querySelector('.proto-seg-btn.selected');
  return (b && b.dataset.proto) || 'anthropic';
}
function setProtocol(v) {
  const g = $('fProtocol'); if (!g) return;
  v = v || 'anthropic';
  g.querySelectorAll('.proto-seg-btn').forEach((b) => b.classList.toggle('selected', b.dataset.proto === v));
  syncProtocolHint();
}
// Reflect the chosen protocol as a prominent status line so the user always knows whether their
// requests pass through directly (Anthropic) or get auto-translated (OpenAI Chat / Responses).
function syncProtocolHint() {
  const badge = $('protoBadge');
  if (!badge) return;
  const v = getProtocol();
  const map = {
    'anthropic': { k: 'modal.protoBadgeDirect', cls: 'proto-badge-direct' },
    'openai-chat': { k: 'modal.protoBadgeXlate', cls: 'proto-badge-xlate' },
    'openai-responses': { k: 'modal.protoBadgeXlate', cls: 'proto-badge-xlate' },
  };
  const m = map[v] || map['anthropic'];
  badge.className = 'proto-badge ' + m.cls;
  badge.textContent = I18n.t(m.k);
}
function updateIconPreview() {
  const el = $('fIconPreview');
  const iconData = renderProviderIcon($('fName').value || '?', modalIcon);
  el.setAttribute('style', iconData.style);
  el.innerHTML = iconData.html;
}
function resizeImage(file, size) {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => {
      const img = new Image();
      img.onload = () => {
        try {
          const c = document.createElement('canvas'); c.width = size; c.height = size;
          const ctx = c.getContext('2d');
          const s = Math.min(img.width, img.height);
          ctx.drawImage(img, (img.width - s) / 2, (img.height - s) / 2, s, s, 0, 0, size, size);
          resolve(c.toDataURL('image/png'));
        } catch (e) { reject(e); }
      };
      img.onerror = reject;
      img.src = reader.result;
    };
    reader.onerror = reject;
    reader.readAsDataURL(file);
  });
}
function openIconPicker(anchor) {
  const existing = document.querySelector('.icon-picker');
  if (existing) { existing.remove(); return; }
  const pop = document.createElement('div');
  pop.className = 'icon-picker';
  pop.innerHTML =
    `<div class="ip-grid">${ICON_EMOJIS.map((e) => `<button type="button" class="ip-emoji" data-emoji="${escapeHtml(e)}">${escapeHtml(e)}</button>`).join('')}</div>` +
    `<div class="ip-actions"><button type="button" class="ip-act" data-act="upload">${escapeHtml(I18n.t('modal.iconUpload'))}</button><button type="button" class="ip-act" data-act="random">${escapeHtml(I18n.t('modal.iconRandom'))}</button><button type="button" class="ip-act" data-act="reset">${escapeHtml(I18n.t('modal.iconReset'))}</button></div>` +
    `<input type="file" accept="image/*" class="ip-file" hidden />`;
  document.body.appendChild(pop);
  const r = anchor.getBoundingClientRect();
  let x = Math.max(10, Math.min(Math.round(r.left + r.width / 2 - pop.offsetWidth / 2), window.innerWidth - pop.offsetWidth - 10));
  let y = Math.round(r.bottom + 8);
  if (y + pop.offsetHeight > window.innerHeight - 10) y = Math.max(10, Math.round(r.top - pop.offsetHeight - 8));
  pop.style.left = x + 'px'; pop.style.top = y + 'px';
  const close = () => { pop.remove(); document.removeEventListener('mousedown', onDoc); document.removeEventListener('keydown', onKey); };
  const onDoc = (e) => { if (!pop.contains(e.target) && !anchor.contains(e.target)) close(); };
  const onKey = (e) => { if (e.key === 'Escape') { e.stopPropagation(); close(); } };
  setTimeout(() => { document.addEventListener('mousedown', onDoc); document.addEventListener('keydown', onKey); }, 0);
  pop.addEventListener('click', (e) => {
    const em = e.target.closest('.ip-emoji');
    if (em) { modalIcon = em.dataset.emoji; updateIconPreview(); close(); return; }
    const act = e.target.closest('.ip-act');
    if (!act) return;
    if (act.dataset.act === 'random') { modalIcon = ICON_EMOJIS[Math.floor(Math.random() * ICON_EMOJIS.length)]; updateIconPreview(); close(); }
    else if (act.dataset.act === 'reset') { modalIcon = null; updateIconPreview(); close(); }
    else if (act.dataset.act === 'upload') { pop.querySelector('.ip-file').click(); }
  });
  pop.querySelector('.ip-file').addEventListener('change', (e) => {
    const f = e.target.files && e.target.files[0];
    if (!f) return;
    resizeImage(f, 72).then((d) => { modalIcon = d; updateIconPreview(); close(); }).catch(() => close());
  });
}
function addMapRow(alias = '', upstream = '') {
  const row = document.createElement('div');
  row.className = 'map-row flex items-center gap-1.75';
  const mapInputCls = 'flex-1 min-w-0 bg-bg-input border border-border-custom rounded-md px-2 py-1.5 text-fg font-mono text-[12px] outline-none transition-colors duration-120 focus:border-primary';
  row.innerHTML = `
    <input class="m-alias ${mapInputCls}" placeholder="${escapeHtml(I18n.t('modal.aliasPlaceholder'))}" />
    <span class="map-arrow text-caption shrink-0">→</span>
    <input class="m-upstream ${mapInputCls}" placeholder="${escapeHtml(I18n.t('modal.upstreamPlaceholder'))}" />
    <button class="icon-btn m-del w-6 h-6 p-0 shrink-0 flex items-center justify-center border-0 rounded-md bg-transparent text-muted cursor-pointer transition-colors duration-140 hover:bg-red-soft hover:text-red" type="button">✕</button>`;
  row.querySelector('.m-alias').value = alias;
  row.querySelector('.m-upstream').value = upstream;
  row.querySelector('.m-del').addEventListener('click', () => row.remove());
  $('mapRows').appendChild(row);
}
function openModal(provider) {
  editingId = provider ? provider.id : null;
  modalIcon = provider ? (provider.icon || null) : null;
  $('modalTitle').textContent = provider ? I18n.t('modal.editTitle') : I18n.t('modal.addTitle');
  document.querySelectorAll('.preset-chip').forEach((c) => c.classList.remove('selected'));
  $('fName').value = provider ? provider.name : '';
  $('fBaseUrl').value = provider ? provider.baseUrl : '';
  $('fToken').value = provider ? provider.authToken : '';
  $('fToken').type = 'password'; $('fTokenToggle').textContent = I18n.t('modal.show');
  $('fDefaultModel').value = provider ? provider.defaultModel : '';
  $('fSmallModel').value = provider ? provider.smallFastModel : '';
  $('fMapDefault').checked = provider ? provider.mapDefaultModels !== false : true;
  setProtocol((provider && provider.protocol) || 'anthropic');
  $('mapRows').innerHTML = '';
  if (provider && provider.models) provider.models.forEach((m) => addMapRow(m.alias, m.upstream));
  if (!$('mapRows').children.length) addMapRow(); // always show one empty row to add into
  const mapDetails = $('mapRows').closest('details');
  if (mapDetails) mapDetails.open = true;
  updateIconPreview();
  $('modal').classList.remove('hidden');
  $('fName').focus();
}
function closeModal() { $('modal').classList.add('hidden'); editingId = null; }
function collectProvider() {
  const models = [];
  $('mapRows').querySelectorAll('.map-row').forEach((row) => {
    const alias = row.querySelector('.m-alias').value.trim();
    const upstream = row.querySelector('.m-upstream').value.trim();
    if (alias || upstream) models.push({ alias, upstream });
  });
  const p = {
    name: $('fName').value.trim() || I18n.t('providers.unnamed'),
    baseUrl: $('fBaseUrl').value.trim(),
    authToken: $('fToken').value.trim(),
    defaultModel: $('fDefaultModel').value.trim(),
    smallFastModel: $('fSmallModel').value.trim(),
    mapDefaultModels: $('fMapDefault').checked,
    protocol: getProtocol(),
    models,
  };
  if (modalIcon) p.icon = modalIcon;
  if (editingId) p.id = editingId;
  return p;
}

/* ---------- actions ---------- */
async function refresh() {
  config = await api.getConfig();
  status = await api.serverStatus();
  // Reconcile the boot language (from localStorage) with the persisted config truth.
  try {
    if (config.language && config.language !== I18n.lang) { I18n.setLang(config.language); I18n.apply(document); }
    else localStorage.setItem('ccbud-lang', I18n.lang);
  } catch (_) {}
  renderAll();
  refreshGatewayLog();
}
async function persist(patch) {
  try { config = await api.saveConfig(Object.assign({}, config, patch)); }
  catch (e) { pushRawLog({ level: 'error', msg: I18n.t('err.saveFailed', { msg: (e && e.message ? e.message : e) }) }); }
  status = await api.serverStatus();
  renderAll();
}
// The hero button is the gateway SERVICE switch (start/stop). CLI config wiring lives in
// Settings → connect targets.
async function toggleConnect() {
  const btn = $('btnConnect');
  const on = !status.running;
  btn.disabled = true;
  let res;
  try { res = await api.gatewaySetEnabled(on); } catch (_) { res = null; }
  btn.disabled = false;
  config = await api.getConfig();
  status = await api.serverStatus();
  renderAll();
  if (res && res.ok === false) {
    showHeroNote(res.message || I18n.t('err.opFailed'), true);
  }
}
function copyFeedback(btn, text) {
  const orig = btn.dataset.copyOrig || (btn.dataset.copyOrig = btn.textContent);
  api.copy(text);
  btn.textContent = I18n.t('copy.copiedCheck');
  clearTimeout(btn._t);
  btn._t = setTimeout(() => (btn.textContent = orig), 1500);
}
// Lightweight styled confirm dialog → Promise<boolean>. For actions that are easy to mis-trigger.
function confirmDialog({ title, message, confirmText, cancelText, danger }) {
  return new Promise((resolve) => {
    const ov = document.createElement('div');
    ov.className = 'overlay fixed inset-0 bg-black/35 flex items-center justify-center z-[200] backdrop-blur-md';
    ov.innerHTML = `<div class="bg-bg-elev border border-border-custom rounded-[14px] shadow-card-hover w-[348px] max-w-[88vw] p-5 flex flex-col gap-2.5" style="animation:panelIn 0.18s cubic-bezier(0.23,1,0.32,1)">
      <h3 class="text-[14px] font-semibold text-fg tracking-tight">${escapeHtml(title || '')}</h3>
      <p class="text-[12.5px] text-caption leading-[1.55]">${escapeHtml(message || '')}</p>
      <div class="flex justify-end gap-2 mt-2">
        <button class="cd-cancel bg-bg-elev text-fg border border-border-custom rounded-md px-3.5 py-1.5 text-[12px] font-medium cursor-pointer transition-all duration-140 hover:bg-chip-bg hover:border-border-strong active:scale-[0.97]">${escapeHtml(cancelText || I18n.t('modal.cancel'))}</button>
        <button class="cd-ok border-none rounded-md px-3.5 py-1.5 text-[12px] font-semibold text-white cursor-pointer transition-all duration-140 active:scale-[0.97] ${danger ? 'bg-red' : 'bg-primary hover:bg-primary-hover'}">${escapeHtml(confirmText || I18n.t('modal.cancel'))}</button>
      </div>
    </div>`;
    document.body.appendChild(ov);
    const done = (v) => { document.removeEventListener('keydown', onKey); ov.remove(); resolve(v); };
    const onKey = (e) => { if (e.key === 'Escape') { e.preventDefault(); done(false); } else if (e.key === 'Enter') { e.preventDefault(); done(true); } };
    ov.querySelector('.cd-ok').addEventListener('click', () => done(true));
    ov.querySelector('.cd-cancel').addEventListener('click', () => done(false));
    ov.addEventListener('mousedown', (e) => { if (e.target === ov) done(false); });
    document.addEventListener('keydown', onKey);
    setTimeout(() => { const b = ov.querySelector('.cd-ok'); if (b) b.focus(); }, 0);
  });
}
function genToken() {
  const a = new Uint8Array(18);
  crypto.getRandomValues(a);
  return 'ccbud_' + Array.from(a).map((b) => b.toString(16).padStart(2, '0')).join('');
}
function switchView(view) {
  document.querySelectorAll('#tabs .nav-item, #tabs .seg-btn').forEach((b) => b.classList.toggle('active', b.dataset.view === view));

  // Smooth fade between views
  const viewIds = {
    providers: 'view-providers',
    monitor: 'view-monitor',
    conversations: 'view-conversations',
    settings: 'view-settings',
  };
  const views = Object.values(viewIds).map((id) => $(id));

  const current = views.find(el => el && !el.classList.contains('hidden'));
  const targetId = viewIds[view] || 'view-providers';
  const target = $(targetId);

  const doSwitch = () => {
    views.forEach(el => {
      if (!el) return;
      const isTarget = el === target;
      el.classList.toggle('hidden', !isTarget);
      if (!isTarget) {
        el.style.transition = '';
        el.style.opacity = '';
      }
    });
    $('btnAdd').classList.toggle('hidden', view !== 'providers');
    const emptyAdd = $('btnAddEmpty');
    if (emptyAdd) emptyAdd.classList.toggle('hidden', view !== 'providers');

    if (target) {
      target.style.transition = 'none';
      target.style.opacity = '0';
      // Restart the fade on the next frame instead of `void target.offsetWidth` — that read forced a
      // synchronous full-document layout on every view switch (costly on the heavy 对话 view; traced).
      requestAnimationFrame(() => {
        target.style.transition = 'opacity 0.22s cubic-bezier(0.23, 1, 0.32, 1)';
        target.style.opacity = '1';
        setTimeout(() => { if (target) target.style.transition = ''; }, 280);
      });
    }

    if (view === 'conversations' && window.ccbudConversations) window.ccbudConversations.onShow();
    if (view === 'monitor') refreshGatewayLog();
    if (view === 'settings') startDesktopPoll(); else stopDesktopPoll();
    // Lock the window to a fixed, non-resizable size on Settings; restore it elsewhere.
    if (api.setSettingsMode) api.setSettingsMode(view === 'settings');
    // 对话 needs the wide 3-column layout (min 1300); other views can be narrower (900) so a wide
    // window doesn't leave big side gaps. Switching to 对话 auto-grows the window to ≥1300.
    if (api.setViewMinWidth) api.setViewMinWidth(view === 'conversations' ? 1300 : 900);
  };

  if (current && current !== target) {
    current.style.transition = 'opacity 0.12s ease';
    current.style.opacity = '0';
    setTimeout(() => {
      current.style.transition = '';
      current.style.opacity = '';
      doSwitch();
    }, 110);
  } else {
    doSwitch();
  }
}
function applyTheme(t) {
  document.documentElement.setAttribute('data-theme', t);
  try { localStorage.setItem('ccbud-theme', t); } catch (_) {}
  const dark = t === 'dark';
  const hd = document.getElementById('hljs-dark');
  const hl = document.getElementById('hljs-light');
  if (hd) hd.disabled = !dark;
  if (hl) hl.disabled = dark;
  // Theme-toggle icon reflects the current mode: sun in light, moon in dark.
  const tbIcon = document.querySelector('#btnTheme [data-icon]');
  if (tbIcon) { const nm = dark ? 'moon' : 'theme'; tbIcon.dataset.icon = nm; if (I[nm]) tbIcon.innerHTML = I[nm]; }
}

/* ---------- drag reorder ---------- */
function wireDrag() {
  const list = $('providerList');
  list.addEventListener('dragstart', (e) => {
    const card = e.target.closest('.provider'); if (!card) return;
    dragId = card.dataset.id; card.classList.add('dragging');
  });
  list.addEventListener('dragend', (e) => {
    const card = e.target.closest('.provider'); if (card) card.classList.remove('dragging');
    document.querySelectorAll('.provider.drag-over').forEach((c) => c.classList.remove('drag-over'));
  });
  list.addEventListener('dragover', (e) => {
    e.preventDefault();
    const card = e.target.closest('.provider');
    document.querySelectorAll('.provider.drag-over').forEach((c) => c.classList.remove('drag-over'));
    if (card && card.dataset.id !== dragId) card.classList.add('drag-over');
  });
  list.addEventListener('drop', async (e) => {
    e.preventDefault();
    const card = e.target.closest('.provider');
    if (!card || !dragId || card.dataset.id === dragId) return;
    const ids = config.providers.map((p) => p.id);
    const from = ids.indexOf(dragId), to = ids.indexOf(card.dataset.id);
    if (from < 0 || to < 0) return;
    const reordered = config.providers.slice();
    const [moved] = reordered.splice(from, 1);
    reordered.splice(to, 0, moved);
    await persist({ providers: reordered });
  });
}

/* ---------- wire up ---------- */
// Settings sub-nav: keep the main panel focused on one section at a time.
function switchSettings(pane) {
  const nav = $('settingsNav');
  if (nav) nav.querySelectorAll('.settings-subnav-item').forEach((b) => b.classList.toggle('active', b.dataset.settings === pane));
  const panes = $('settingsPanes');
  if (panes) panes.querySelectorAll('[data-pane]').forEach((p) => p.classList.toggle('hidden', p.dataset.pane !== pane));
  // Refresh the live cards the moment their section is revealed.
  if (pane === 'desktop') renderDesktopCard();
  if (pane === 'about') loadUpdateState();
}

function bind() {
  if ($('appLogo') && I.logo) $('appLogo').innerHTML = I.logo(30);
  injectIcons();

  $('tabs').addEventListener('click', (e) => {
    const btn = e.target.closest('.nav-item, .seg-btn');
    if (btn && btn.dataset.view) switchView(btn.dataset.view);
  });
  const settingsNav = $('settingsNav');
  if (settingsNav) settingsNav.addEventListener('click', (e) => {
    const b = e.target.closest('.settings-subnav-item');
    if (b && b.dataset.settings) switchSettings(b.dataset.settings);
  });
  // Settings sub-nav collapse (icons-only, auto-shrinks width) — persisted like the main sidebar.
  const subnavBtn = $('btnSubnavCollapse');
  if (settingsNav && subnavBtn) {
    try {
      if (localStorage.getItem('ccbud-subnav-collapsed') === '1') {
        settingsNav.classList.add('collapsed');
        const ic = subnavBtn.querySelector('[data-icon]');
        if (ic && I.chevronRight) ic.innerHTML = I.chevronRight;
      }
    } catch (_) {}
    subnavBtn.addEventListener('click', (e) => {
      e.stopPropagation();
      const collapsed = settingsNav.classList.toggle('collapsed');
      const ic = subnavBtn.querySelector('[data-icon]');
      if (ic) ic.innerHTML = collapsed ? (I.chevronRight || '›') : (I.chevronLeft || '‹');
      try { localStorage.setItem('ccbud-subnav-collapsed', collapsed ? '1' : '0'); } catch (_) {}
    });
  }
  $('btnTheme').addEventListener('click', () => {
    const cur = document.documentElement.getAttribute('data-theme') || 'light';
    applyTheme(cur === 'light' ? 'dark' : 'light');
  });

  // Main sidebar collapse (affects all views)
  const sidebar = document.querySelector('.sidebar');
  const collapseBtn = $('btnCollapseSidebar');
  if (collapseBtn && sidebar) {
    // restore
    try {
      if (localStorage.getItem('ccbud-sidebar-collapsed') === '1') {
        sidebar.classList.add('collapsed');
        const icon = collapseBtn.querySelector('[data-icon]');
        if (icon && I.chevronRight) icon.innerHTML = I.chevronRight;
      }
    } catch (_) {}
    collapseBtn.addEventListener('click', () => {
      const isCollapsed = sidebar.classList.toggle('collapsed');
      const icon = collapseBtn.querySelector('[data-icon]');
      if (icon) icon.innerHTML = isCollapsed ? (I.chevronRight || '›') : (I.chevronLeft || '‹');
      try { localStorage.setItem('ccbud-sidebar-collapsed', isCollapsed ? '1' : '0'); } catch (_) {}
    });
  }
  $('btnConnect').addEventListener('click', toggleConnect);
  const heroRanges = $('heroRanges');
  if (heroRanges) heroRanges.addEventListener('click', (e) => {
    const b = e.target.closest('[data-hrange]');
    if (!b) return;
    heroRange = b.dataset.hrange;
    heroRanges.querySelectorAll('.seg-btn').forEach((x) => x.classList.toggle('active', x === b));
    renderHeroUsage();
  });
  const heroEndpoint = $('heroEndpoint');
  if (heroEndpoint) heroEndpoint.addEventListener('click', () => {
    const port = (status.running && status.port) || config.port;
    if (api.copy) api.copy(`http://localhost:${port}`);
    const t = $('heroEndpointText');
    if (t) { const restore = `localhost:${port}`; t.textContent = I18n.t('copy.copiedCheck'); clearTimeout(t._t); t._t = setTimeout(() => { t.textContent = restore; }, 1400); }
  });

  $('portInput').addEventListener('change', async (e) => {
    const port = Number(e.target.value);
    if (!Number.isInteger(port) || port < 1 || port > 65535) {
      e.target.value = config.port;
      pushRawLog({ level: 'error', msg: I18n.t('err.portInvalid') });
      return;
    }
    await persist({ port });
  });
  $('btnCopyExport').addEventListener('click', (e) => copyFeedback(e.currentTarget, $('exportBlock').textContent));
  document.querySelectorAll('[data-copy]').forEach((b) => b.addEventListener('click', () => copyFeedback(b, $(b.getAttribute('data-copy')).textContent)));

  $('fOpenAtLogin').addEventListener('change', (e) => persist({ openAtLogin: e.target.checked }));
  $('fRequireToken').addEventListener('change', (e) => {
    const requireToken = e.target.checked;
    const patch = { requireToken };
    if (requireToken && !config.gatewayToken) patch.gatewayToken = genToken();
    persist(patch);
  });
  $('fGatewayToken').addEventListener('change', (e) => persist({ gatewayToken: e.target.value.trim() }));
  $('btnGenToken').addEventListener('click', () => persist({ gatewayToken: genToken(), requireToken: true }));
  if ($('fTargetClaude')) $('fTargetClaude').addEventListener('change', (e) => toggleTarget('claude', e.target.checked));
  if ($('fTargetCodex')) $('fTargetCodex').addEventListener('change', (e) => toggleTarget('codex', e.target.checked));
  if ($('fGatewayEnabled')) $('fGatewayEnabled').addEventListener('change', async (e) => {
    const on = e.target.checked;
    let res;
    try { res = await api.gatewaySetEnabled(on); } catch (_) { res = null; }
    if (res && res.ok === false) {
      e.target.checked = !on; // couldn't bind the port → revert + surface
      try { showHeroNote(res.message || I18n.t('err.opFailed'), true); } catch (_) {}
    }
    config = await api.getConfig();
    status = await api.serverStatus();
    renderAll();
  });
  if ($('fRetry429')) $('fRetry429').addEventListener('change', (e) => persist({ retry429: Object.assign({}, config.retry429, { enabled: e.target.checked }) }));
  if ($('fInsecureTls')) $('fInsecureTls').addEventListener('change', (e) => persist({ insecureSkipVerify: e.target.checked }));
  $('fTrayUsage').addEventListener('change', (e) => persist({ trayUsage: { enabled: e.target.checked, range: $('fTrayRange').value } }));
  $('fTrayRange').addEventListener('change', (e) => persist({ trayUsage: { enabled: $('fTrayUsage').checked, range: e.target.value } }));
  if ($('fLang')) $('fLang').addEventListener('change', async (e) => {
    const language = e.target.value;
    I18n.setLang(language);            // updates <html lang> + localStorage['ccbud-lang']
    I18n.apply(document);              // static data-i18n nodes
    renderAll();                       // dynamic strings (hero/status/monitor/providers/settings)
    if (window.ccbudConversations && window.ccbudConversations.setLang) window.ccbudConversations.setLang();
    await persist({ language });       // → config:save → main rebuilds tray on next open
  });

  // History directories — primary action opens a native picker (hidden dirs shown)
  const btnPickHistDir = $('btnPickHistDir');
  if (btnPickHistDir) btnPickHistDir.addEventListener('click', pickHistDir);
  // (Manual text input removed in favor of native dialog picker)
  const histDirList = $('histDirList');
  if (histDirList) histDirList.addEventListener('click', async (e) => {
    const btn = e.target.closest('[data-del-dir]');
    if (!btn || btn.disabled) return;
    const id = btn.dataset.delDir;
    if (id === '~/.claude') return;
    const dirs = (config.historyDirs || []).filter((d) => d !== id);
    await persist({ historyDirs: dirs.length ? dirs : ['~/.claude'] });
  });

  const btnDesktop = $('btnDesktopConnect');
  if (btnDesktop) btnDesktop.addEventListener('click', async () => {
    const note = $('desktopNote');
    const wasConnected = btnDesktop.dataset.connected === '1';
    btnDesktop.disabled = true;
    btnDesktop.textContent = I18n.t(wasConnected ? 'settings.desktopRestoring' : 'settings.desktopConnecting');
    let res;
    try { res = wasConnected ? await api.desktopDisconnect() : await api.desktopConnect(); }
    catch (e) { res = { ok: false, message: (e && e.message) || '' }; }
    if (note) {
      note.dataset.transient = '1';
      if (wasConnected) {
        if (res && res.cancelled) { delete note.dataset.transient; }
        else note.textContent = I18n.t(res && res.removed ? 'settings.desktopRestored' : 'settings.desktopRestoreSteps');
      } else if (res && res.ok) {
        note.textContent = I18n.t('settings.desktopSteps');
      } else {
        const reason = res && res.reason;
        note.textContent = reason === 'noProvider' ? I18n.t('settings.desktopNoProvider')
          : reason === 'notInstalled' ? I18n.t('settings.desktopNotInstalled')
          : (res && res.message) || I18n.t('err.opFailed');
        delete note.dataset.transient;
      }
    }
    btnDesktop.disabled = false;
    setTimeout(renderDesktopCard, 1800);
  });
  const btnDesktopRefresh = $('btnDesktopRefresh');
  if (btnDesktopRefresh) btnDesktopRefresh.addEventListener('click', () => {
    const n = $('desktopNote');
    if (n) delete n.dataset.transient; // show the true current state, not a lingering action note
    renderDesktopCard();
  });
  // Re-check the moment the window regains focus (e.g. right after approving in System Settings),
  // in case the background poll was throttled while ccbud was in the background.
  window.addEventListener('focus', () => { if (desktopPollTimer) { renderDesktopCard(); } });

  $('btnAdd').addEventListener('click', () => openModal(null));
  const btnAddEmpty = $('btnAddEmpty');
  if (btnAddEmpty) btnAddEmpty.addEventListener('click', () => openModal(null));

  $('providerList').addEventListener('click', async (e) => {
    // Resolve the actual button (the click may land on the inner SVG icon).
    const btn = e.target.closest('button');
    if (btn && btn.dataset.edit) { openModal(config.providers.find((p) => p.id === btn.dataset.edit)); return; }
    if (btn && btn.dataset.del) { if (confirm(I18n.t('providers.confirmDelete'))) { config = await api.deleteProvider(btn.dataset.del); renderAll(); } return; }
    if (btn && btn.dataset.test) {
      const p = config.providers.find((pp) => pp.id === btn.dataset.test);
      const orig = btn.innerHTML; // preserve the SVG icon, restore it after
      btn.innerHTML = '…'; btn.disabled = true;
      const res = await api.testProvider(p);
      btn.disabled = false; btn.innerHTML = res.ok ? '✓' : '✗';
      pushRawLog({ level: res.ok ? 'info' : 'error', msg: I18n.t('modal.testLog', { name: p.name, msg: res.message }) });
      setTimeout(() => { btn.innerHTML = orig; }, 1800);
      return;
    }
    if (btn) return; // some other button — ignore
    // click anywhere else on the card → set it as the active service
    const card = e.target.closest('.provider');
    if (!card || !card.dataset.id) return;
    if (card.dataset.id === config.activeProviderId) return; // already active — nothing to switch
    // While the gateway is running, switching is easy to mis-trigger and would re-point every new
    // Claude Code session — so confirm first.
    if (status.connected) {
      const p = config.providers.find((pp) => pp.id === card.dataset.id);
      const ok = await confirmDialog({
        title: I18n.t('switch.confirmTitle', { name: p ? p.name : '' }),
        message: I18n.t('switch.confirmMsg'),
        confirmText: I18n.t('switch.confirmOk'),
      });
      if (!ok) return;
    }
    config = await api.setActive(card.dataset.id);
    renderAll();
  });
  wireDrag();

  $('modalClose').addEventListener('click', closeModal);
  $('btnCancel').addEventListener('click', closeModal);
  $('presetGrid').addEventListener('click', (e) => { if (e.target.dataset.preset) selectPreset(e.target.dataset.preset); });
  { const fp = $('fProtocol'); if (fp) fp.addEventListener('click', (e) => { const b = e.target.closest('.proto-seg-btn'); if (b) setProtocol(b.dataset.proto); }); }
  $('fName').addEventListener('input', updateIconPreview);
  const fIconPreview = $('fIconPreview');
  if (fIconPreview && fIconPreview.parentElement) fIconPreview.parentElement.addEventListener('click', () => openIconPicker(fIconPreview));
  $('btnAddMap').addEventListener('click', () => addMapRow());
  $('fTokenToggle').addEventListener('click', () => {
    const f = $('fToken'); const show = f.type === 'password';
    f.type = show ? 'text' : 'password'; $('fTokenToggle').textContent = show ? I18n.t('modal.hide') : I18n.t('modal.show');
  });
  $('btnSave').addEventListener('click', async () => {
    const p = collectProvider();
    if (!p.baseUrl) {
      showToast(I18n.t('modal.fillUrl'), 'err');
      return;
    }
    config = await api.upsertProvider(p);
    closeModal(); renderAll();
  });
  $('btnTest').addEventListener('click', async () => {
    // Surface the result as a floating toast — the in-modal alert sits at the bottom of a
    // scrollable sheet and was easily hidden, leaving users unsure whether the test ran.
    const pending = showToast(I18n.t('modal.testing'), 'pending');
    const res = await api.testProvider(collectProvider());
    let msg;
    if (res.reason === 'baseUrlEmpty') msg = I18n.t('err.baseUrlEmpty');
    else if (res.reason === 'baseUrlInvalid') msg = I18n.t('err.baseUrlInvalid');
    else if (res.reason === 'timeout') msg = I18n.t('err.timeout');
    else if (res.ok) msg = I18n.t('err.testOk', { model: res.model || '' });
    else msg = res.message || ('HTTP ' + (res.status || ''));
    if (pending) pending.dismiss();
    showToast((res.ok ? '✓ ' : '✗ ') + msg, res.ok ? 'ok' : 'err');
  });

  $('btnClearLog').addEventListener('click', () => {
    $('streamList').innerHTML = `<div class="state-inline p-4 text-center text-[11.5px] text-caption">${escapeHtml(I18n.t('monitor.streamEmpty'))}</div>`;
    gwLog.items.length = 0; gwLog.seen.clear();
    renderGatewayLog();
    stats.total = stats.ok = stats.sumMs = 0; stats.last = null;
    $('streamHint').textContent = I18n.t('monitor.waitingDots');
    renderMonitor();
    if (api.monitorClear) api.monitorClear();
    if (api.logsClear) api.logsClear();
    closeReqDrawer();
  });

  // Request inspector: click a stream row to open its full captured exchange.
  const streamList = $('streamList');
  if (streamList) streamList.addEventListener('click', (e) => {
    const row = e.target.closest('.stream-row');
    if (row && row.dataset.id) openReqDetail(row.dataset.id);
  });
  const rdClose = $('reqDrawerClose');
  if (rdClose) rdClose.addEventListener('click', closeReqDrawer);
  const reqDrawer = $('reqDrawer');
  if (reqDrawer) reqDrawer.addEventListener('click', (e) => { if (e.target === reqDrawer) closeReqDrawer(); });
  // Tabs are re-rendered per exchange (2 or 4 of them) — delegate on the container.
  const drTabsWrap = $('drTabs');
  if (drTabsWrap) drTabsWrap.addEventListener('click', (e) => {
    const t = e.target.closest('.dr-tab');
    if (!t) return;
    reqDrawerTab = t.dataset.tab;
    drTabsWrap.querySelectorAll('.dr-tab').forEach((x) => x.classList.toggle('active', x === t));
    renderReqDrawerBody();
  });
  const reqDrawerBody = $('reqDrawerBody');
  if (reqDrawerBody) {
    reqDrawerBody.addEventListener('click', (e) => {
      if (e.target.closest('.dr-search-prev')) { drNavSearch(-1); return; }
      if (e.target.closest('.dr-search-next')) { drNavSearch(1); return; }
      const cb = e.target.closest('[data-copy-body]');
      if (cb && reqDrawerData) {
        const cap = drawerTabView(reqDrawerData, cb.dataset.copyBody).cap;
        api.copy((cap && cap.text) || '');
        cb.textContent = I18n.t('copy.copied'); setTimeout(() => { cb.textContent = I18n.t('drawer.copy'); }, 1200);
      }
    });
    let _drSearchT = null;
    reqDrawerBody.addEventListener('input', (e) => {
      if (!e.target.classList || !e.target.classList.contains('dr-search')) return;
      const v = e.target.value;
      clearTimeout(_drSearchT);
      _drSearchT = setTimeout(() => applyDrSearch(v), 110);
    });
    reqDrawerBody.addEventListener('keydown', (e) => {
      if (!e.target.classList || !e.target.classList.contains('dr-search')) return;
      if (e.key === 'Enter') { e.preventDefault(); drNavSearch(e.shiftKey ? -1 : 1); }
      else if (e.key === 'Escape') {
        if (e.target.value) { e.preventDefault(); e.stopPropagation(); e.target.value = ''; applyDrSearch(''); }
        else e.target.blur();
      }
    });
  }
  document.addEventListener('keydown', (e) => { if (e.key === 'Escape' && reqDrawer && !reqDrawer.classList.contains('hidden')) closeReqDrawer(); });

  api.onRequest((r) => pushStreamRow(r));
  api.onLog((l) => pushRawLog(l));
  api.onStatus((s) => { status = s; renderAll(); });

  // ----- in-app updates -----
  const bUpdCheck = $('btnUpdateCheck');
  if (bUpdCheck) bUpdCheck.addEventListener('click', checkUpdate);
  const bUpdDl = $('btnUpdateDownload');
  if (bUpdDl) bUpdDl.addEventListener('click', downloadUpdate);
  const bUpdApply = $('btnUpdateApply');
  if (bUpdApply) bUpdApply.addEventListener('click', async () => {
    const ok = await confirmDialog({ title: I18n.t('about.restartTitle'), message: I18n.t('about.restartMsg'), confirmText: I18n.t('about.restartNow') });
    if (ok) api.updateApply();
  });
  const bUpdOpen = $('btnUpdateOpen');
  if (bUpdOpen) bUpdOpen.addEventListener('click', () => api.openExternal((updateState && updateState.releaseUrl) || 'https://github.com/ccbud/ccbud/releases/latest'));
  const bUpdBrew = $('btnUpdateBrew');
  if (bUpdBrew) bUpdBrew.addEventListener('click', (e) => copyFeedback(e.currentTarget, (updateState && updateState.brewCommand) || 'brew upgrade --cask ccbud'));
  const bRepo = $('btnRepo');
  if (bRepo) bRepo.addEventListener('click', () => api.openExternal('https://github.com/ccbud/ccbud'));
  const bReleases = $('btnReleases');
  if (bReleases) bReleases.addEventListener('click', () => api.openExternal('https://github.com/ccbud/ccbud/releases'));
  const fAutoCheck = $('fAutoCheck');
  if (fAutoCheck) fAutoCheck.addEventListener('change', async (e) => { config.autoUpdate = await api.updateSetAuto({ check: e.target.checked }); });
  const fAutoDownload = $('fAutoDownload');
  if (fAutoDownload) fAutoDownload.addEventListener('change', async (e) => { config.autoUpdate = await api.updateSetAuto({ autoDownload: e.target.checked }); });

  if (api.onUpdateState) api.onUpdateState((s) => { updateState = s; renderUpdate(); });
  if (api.onUpdateStaged) api.onUpdateStaged(() => { loadUpdateState(); pushRawLog({ level: 'info', msg: I18n.t('about.stagedLog') }); });
  if (api.onUpdateOpenPane) api.onUpdateOpenPane(() => { switchView('settings'); switchSettings('about'); checkUpdate(); });
}

try { applyTheme(localStorage.getItem('ccbud-theme') || 'light'); } catch (_) { applyTheme('light'); }
// Apply the UI language synchronously before first paint (mirrors theme; no flash). Boot from
// localStorage; the async refresh() then reconciles with config.language (the source of truth).
function bootLang() {
  let l = '';
  try { l = localStorage.getItem('ccbud-lang') || ''; } catch (_) {}
  if (!l) {
    const nav = (navigator.language || 'en').toLowerCase();
    l = nav.startsWith('zh') ? ((/-(tw|hk|mo)\b/.test(nav) || nav.includes('hant')) ? 'zh-TW' : 'zh')
      : nav.startsWith('ja') ? 'ja' : nav.startsWith('ko') ? 'ko' : 'en';
  }
  try { I18n.setLang(l); I18n.apply(document); } catch (_) {}
}
bootLang();
renderPresetGrid();
bind();
refresh();
