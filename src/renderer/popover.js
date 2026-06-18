'use strict';

const api = window.clawdy;
let range = '7d';
let tab = 'overview';

const $ = (id) => document.getElementById(id);

function fmt(n) {
  n = n || 0;
  if (n < 1000) return String(n);
  if (n < 1e6) return (n / 1e3).toFixed(n < 1e4 ? 1 : 0).replace(/\.0$/, '') + 'K';
  if (n < 1e9) return (n / 1e6).toFixed(n < 1e7 ? 1 : 0).replace(/\.0$/, '') + 'M';
  return (n / 1e9).toFixed(1).replace(/\.0$/, '') + 'B';
}
function hourLabel(h) {
  if (h == null) return '—';
  const ap = h < 12 ? 'AM' : 'PM';
  const hh = h % 12 === 0 ? 12 : h % 12;
  return `${hh} ${ap}`;
}
function esc(s) { return String(s == null ? '' : s).replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c])); }

async function render() {
  const u = await api.usageGet(range);
  $('sTokens').textContent = fmt(u.tokens);
  $('sReq').textContent = u.requests.toLocaleString();
  $('sDays').textContent = u.activeDays;
  $('sProv').textContent = u.favoriteProvider || '—';
  $('sCur').innerHTML = `${u.currentStreak}<span class="u">天</span>`;
  $('sLong').innerHTML = `${u.longestStreak}<span class="u">天</span>`;
  $('sPeak').textContent = u.peakHour == null ? '—' : hourLabel(u.peakHour);
  $('sModel').textContent = u.favoriteModel || '—';

  // heatmap (column-major: 7 rows = weekday, columns = weeks)
  const hm = $('heatmap');
  hm.innerHTML = '';
  for (const c of u.heatmap) {
    const cell = document.createElement('div');
    cell.className = 'hm-cell lv' + c.level;
    cell.title = `${c.date}: ${fmt(c.tokens)} tokens`;
    hm.appendChild(cell);
  }

  // fun comparison (The Hobbit ≈ 130k tokens)
  const books = u.tokens / 130000;
  $('popNote').textContent = u.tokens > 0
    ? (books >= 1 ? `≈ 用掉了 ${books >= 10 ? Math.round(books) : books.toFixed(1)} 本《霍比特人》的文字量` : '继续用，攒够一本《霍比特人》～')
    : '还没有用量数据，接入并对话后这里会更新。';

  // models tab
  const ml = $('modelList');
  ml.innerHTML = '';
  if (!u.byModel.length) ml.innerHTML = '<div class="empty small">暂无数据</div>';
  for (const m of u.byModel.slice(0, 12)) {
    const row = document.createElement('div');
    row.className = 'model-row';
    row.innerHTML = `
      <div class="model-name">${esc(m.model)}</div>
      <div class="model-bar"><div class="model-bar-fill" style="width:${Math.max(2, Math.round(m.pct * 100))}%"></div></div>
      <div class="model-tok mono">${fmt(m.tokens)}</div>`;
    ml.appendChild(row);
  }
}

async function renderStatus() {
  const s = await api.serverStatus();
  $('popStatus').querySelector('.live-dot').className = 'live-dot ' + (s.connected ? 'on' : 'off');
  $('popStatusText').textContent = s.connected ? '已接入' : '未接入';
  $('popConnect').textContent = s.connected ? '断开' : '一键接入';
  $('popConnect').dataset.connected = s.connected ? '1' : '';
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
  render();
}

function bind() {
  $('popTabs').addEventListener('click', (e) => { if (e.target.dataset.tab) setTab(e.target.dataset.tab); });
  $('popRanges').addEventListener('click', (e) => { if (e.target.dataset.range) setRange(e.target.dataset.range); });
  $('popConnect').addEventListener('click', async (e) => {
    e.target.disabled = true;
    if (e.target.dataset.connected) await api.disconnect(); else await api.connect();
    e.target.disabled = false;
    renderStatus();
  });
  $('popOpen').addEventListener('click', () => api.openMain());
  $('popQuit').addEventListener('click', () => api.quitApp());
  // main process tells us to refresh whenever the popover is shown
  if (api.onPopoverShow) api.onPopoverShow(() => { applyTheme(); render(); renderStatus(); });
}

function applyTheme() {
  try { document.documentElement.setAttribute('data-theme', localStorage.getItem('clawdy-theme') || 'light'); } catch (_) {}
}

applyTheme();
bind();
render();
renderStatus();
