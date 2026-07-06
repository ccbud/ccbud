'use strict';

const api = window.ccbud;
let range = '7d';
let tab = 'overview';
let heatmapReady = false;

const $ = (id) => document.getElementById(id);
const L = (k, p) => (window.I18n ? window.I18n.t(k, p) : k);

function fmt(n) {
  n = n || 0;
  if (n < 1000) return String(n);
  if (n < 1e6) return (n / 1e3).toFixed(n < 1e4 ? 1 : 0).replace(/\.0$/, '') + 'K';
  if (n < 1e9) return (n / 1e6).toFixed(n < 1e7 ? 1 : 0).replace(/\.0$/, '') + 'M';
  return (n / 1e9).toFixed(1).replace(/\.0$/, '') + 'B';
}
function hourLabel(h) {
  if (h == null) return '—';
  const tag = window.I18n ? window.I18n.localeTag : 'en-US';
  try { return new Date(2000, 0, 1, h).toLocaleTimeString(tag, { hour: 'numeric' }); }
  catch (_) { const ap = h < 12 ? 'AM' : 'PM'; const hh = h % 12 === 0 ? 12 : h % 12; return `${hh} ${ap}`; }
}
function esc(s) { return String(s == null ? '' : s).replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c])); }
function fmtDate(d) {
  try {
    const dt = new Date(`${d}T00:00:00`);
    if (isNaN(dt)) return d;
    return dt.toLocaleDateString(window.I18n ? window.I18n.localeTag : 'en-US', { year: 'numeric', month: 'short', day: 'numeric' });
  } catch (_) { return d; }
}

// Instant, styled heatmap tooltip (replaces the slow/ugly native title).
let _tip = null;
function showHeatTip(cell) {
  if (!_tip) { _tip = document.createElement('div'); _tip.className = 'hm-tip'; document.body.appendChild(_tip); }
  _tip.innerHTML = `<div class="hm-tip-d">${esc(cell.dataset.date || '')}</div><div class="hm-tip-v">${esc(cell.dataset.val || '')}</div>`;
  _tip.classList.add('show');
  const r = cell.getBoundingClientRect();
  const tw = _tip.offsetWidth, th = _tip.offsetHeight;
  let x = Math.max(6, Math.min(r.left + r.width / 2 - tw / 2, window.innerWidth - tw - 6));
  let y = r.top - th - 7;
  if (y < 4) y = r.bottom + 7;
  _tip.style.left = `${Math.round(x)}px`;
  _tip.style.top = `${Math.round(y)}px`;
}
function hideHeatTip() { if (_tip) _tip.classList.remove('show'); }

async function renderHeatmap() {
  let u;
  try {
    u = await api.usageGet('all');
  } catch (_) {
    u = { heatmap: [] };
  }
  const hm = $('heatmap');
  if (!hm) return;
  hm.innerHTML = '';
  const levelBgs = {
    0: 'bg-[#c6ccd8] dark:bg-white/14',
    1: 'bg-[#5856d6]/34 dark:bg-[#7d7aff]/32',
    2: 'bg-[#5856d6]/55 dark:bg-[#7d7aff]/54',
    3: 'bg-[#5856d6]/76 dark:bg-[#7d7aff]/76',
    4: 'bg-brand dark:bg-[#7d7aff]'
  };
  if (u && u.heatmap) {
    for (const c of u.heatmap) {
      const cell = document.createElement('div');
      cell.className = `hm-cell lv${c.level} rounded-[3px] transition-colors duration-200 ${levelBgs[c.level] || levelBgs[0]}`;
      cell.dataset.date = fmtDate(c.date);
      cell.dataset.val = `${fmt(c.tokens)} ${L('pop.tokensUnit')}`;
      hm.appendChild(cell);
    }
  }
  heatmapReady = true;
}

async function renderStats() {
  let u;
  try {
    u = await api.usageGet(range);
  } catch (_) {
    u = { tokens: 0, requests: 0, activeDays: 0, favoriteProvider: '—', currentStreak: 0, longestStreak: 0, peakHour: null, favoriteModel: '—', byModel: [] };
  }
  if (!u) u = { tokens: 0, requests: 0, activeDays: 0, favoriteProvider: '—', currentStreak: 0, longestStreak: 0, peakHour: null, favoriteModel: '—', byModel: [] };
  $('sTokens').textContent = fmt(u.tokens);
  $('sReq').textContent = (u.requests || 0).toLocaleString();
  $('sDays').textContent = u.activeDays || 0;
  const elProv = $('sProv');
  const fullProv = u.favoriteProvider && u.favoriteProvider !== '—' ? u.favoriteProvider : '';
  elProv.textContent = u.favoriteProvider || '—';
  if (elProv.parentElement) {
    if (fullProv) elProv.parentElement.setAttribute('data-tip', fullProv);
    else elProv.parentElement.removeAttribute('data-tip');
  }
  $('sCur').innerHTML = `${u.currentStreak || 0}<span class="text-[10px] text-muted font-normal ml-0.5">${esc(L('time.unitDay'))}</span>`;
  $('sLong').innerHTML = `${u.longestStreak || 0}<span class="text-[10px] text-muted font-normal ml-0.5">${esc(L('time.unitDay'))}</span>`;
  $('sPeak').textContent = u.peakHour == null ? '—' : hourLabel(u.peakHour);
  const elModel = $('sModel');
  const fullModel = u.favoriteModel && u.favoriteModel !== '—' ? u.favoriteModel : '';
  elModel.textContent = u.favoriteModel || '—';
  if (elModel.parentElement) {
    if (fullModel) elModel.parentElement.setAttribute('data-tip', fullModel);
    else elModel.parentElement.removeAttribute('data-tip');
  }

  const ml = $('modelList');
  if (ml) {
    ml.innerHTML = '';
    const byModel = u.byModel || [];
    if (!byModel.length) ml.innerHTML = `<div class="empty small text-center text-xs text-muted leading-relaxed py-4">${esc(L('pop.noData'))}</div>`;
    for (const m of byModel.slice(0, 12)) {
      const row = document.createElement('div');
      row.className = 'model-row flex items-center gap-2';
      row.innerHTML = `
        <div class="model-name w-[120px] text-[11px] font-mono truncate text-fg" title="${esc(m.model)}">${esc(m.model)}</div>
        <div class="model-bar flex-1 h-[5px] bg-chip-bg rounded-[3px] overflow-hidden"><div class="model-bar-fill h-full bg-brand" style="width:${Math.max(2, Math.round((m.pct || 0) * 100))}%"></div></div>
        <div class="model-tok mono w-11 text-right text-[10px] text-caption font-mono">${fmt(m.tokens)}</div>`;
      ml.appendChild(row);
    }
  }
}

async function render() {
  if (!heatmapReady) await renderHeatmap();
  await renderStats();
}

async function renderStatus() {
  const s = await api.serverStatus();
  const dot = $('popStatus').querySelector('.pulse-dot, .live-dot');
  dot.className = 'pulse-dot w-1.75 h-1.75 rounded-full shrink-0 ' + (s.running ? 'on bg-green animate-[pulse_2s_infinite]' : 'off bg-muted');
  $('popStatusText').textContent = s.running ? L('status.gwRunning') : L('status.gwStopped');
  $('popConnect').textContent = s.running ? L('pop.svcStop') : L('pop.svcStart');
  $('popConnect').dataset.running = s.running ? '1' : '';
}

function setTab(t) {
  tab = t;
  document.querySelectorAll('#popTabs .seg-btn').forEach((b) => b.classList.toggle('active', b.dataset.tab === t));
  $('tab-overview').classList.toggle('hidden', t !== 'overview');
  $('tab-models').classList.toggle('hidden', t !== 'models');
}
function setRange(r) {
  range = r;
  document.querySelectorAll('#popRanges .seg-btn').forEach((b) => b.classList.toggle('active', b.dataset.range === r));
  renderStats();
}

function bind() {
  $('popTabs').addEventListener('click', (e) => { if (e.target.dataset.tab) setTab(e.target.dataset.tab); });
  $('popRanges').addEventListener('click', (e) => { if (e.target.dataset.range) setRange(e.target.dataset.range); });
  $('popConnect').addEventListener('click', async (e) => {
    e.target.disabled = true;
    try { await api.gatewaySetEnabled(!e.target.dataset.running); } catch (_) {}
    e.target.disabled = false;
    renderStatus();
  });
  $('popOpen').addEventListener('click', () => api.openMain());
  $('popQuit').addEventListener('click', () => api.quitApp());
  const hm = $('heatmap');
  if (hm) {
    hm.addEventListener('mouseover', (e) => { const c = e.target.closest('.hm-cell'); if (c) showHeatTip(c); });
    hm.addEventListener('mouseleave', hideHeatTip);
  }
  if (api.onPopoverShow) {
    api.onPopoverShow(async () => {
      applyTheme();
      applyLang();
      heatmapReady = false;
      await render();
      renderStatus();
    });
  }
}

function applyTheme() {
  try { document.documentElement.setAttribute('data-theme', localStorage.getItem('ccbud-theme') || 'light'); } catch (_) {}
}
// The popover is a separate window; it reads the language from shared localStorage (set by the
// main window) on load and on every show, so a language change propagates the next time it opens.
function applyLang() {
  try {
    let l = localStorage.getItem('ccbud-lang') || '';
    if (!l) {
      const nav = (navigator.language || 'en').toLowerCase();
      l = nav.startsWith('zh') ? ((/-(tw|hk|mo)\b/.test(nav) || nav.includes('hant')) ? 'zh-TW' : 'zh')
        : nav.startsWith('ja') ? 'ja' : nav.startsWith('ko') ? 'ko' : 'en';
    }
    if (window.I18n) { window.I18n.setLang(l); window.I18n.apply(document); }
  } catch (_) {}
}

applyTheme();
applyLang();
bind();
render();
renderStatus();