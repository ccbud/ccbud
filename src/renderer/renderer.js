'use strict';

const api = window.clawdy;
let config = { port: 8788, activeProviderId: null, providers: [] };
let status = { running: false, port: null, connected: false, lastStartError: null, claudePath: '' };
let editingId = null;
let dragId = null;
const stats = { total: 0, ok: 0, sumMs: 0, last: null };

const $ = (id) => document.getElementById(id);
const I = window.ClawdyIcons || {};

function injectIcons(root) {
  (root || document).querySelectorAll('[data-icon]').forEach((el) => {
    const name = el.dataset.icon;
    if (I[name]) el.innerHTML = I[name];
  });
}

const PRESETS = {
  glm: { name: '智谱 GLM', baseUrl: 'https://open.bigmodel.cn/api/anthropic', defaultModel: 'glm-5.1', smallFastModel: 'glm-5.1' },
  deepseek: { name: 'DeepSeek', baseUrl: 'https://api.deepseek.com/anthropic', defaultModel: 'deepseek-chat', smallFastModel: 'deepseek-chat' },
  mimo: { name: '小米 MiMo', baseUrl: 'https://token-plan-sgp.xiaomimimo.com/anthropic', defaultModel: '', smallFastModel: '' },
  kimi: { name: '月之暗面 Kimi', baseUrl: 'https://api.moonshot.cn/anthropic', defaultModel: 'kimi-k2-0905-preview', smallFastModel: 'kimi-k2-0905-preview' },
  custom: { name: '', baseUrl: '', defaultModel: '', smallFastModel: '' },
};
const PRESET_LABELS = { glm: '智谱 GLM', deepseek: 'DeepSeek', mimo: '小米 MiMo', kimi: '月之暗面 Kimi', custom: '自定义' };

/* ---------- helpers ---------- */
function escapeHtml(s) {
  return String(s == null ? '' : s).replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
}
const activeProvider = () => config.providers.find((p) => p.id === config.activeProviderId) || null;
function hashHue(s) { let h = 0; for (let i = 0; i < (s || '').length; i++) h = (h * 31 + s.charCodeAt(i)) % 360; return h; }
function renderProviderIcon(name) {
  const n = (name || '').trim().toLowerCase();

  if (n.includes('kimi') || n.includes('moonshot') || n.includes('月之')) {
    return {
      style: 'background: transparent; box-shadow: none;',
      html: `<img src="assets/kimi.svg" class="prov-svg" style="width: 100%; height: 100%; display: block;" />`
    };
  }
  if (n.includes('deepseek')) {
    return {
      style: 'background: transparent; box-shadow: none;',
      html: `<img src="assets/deepseek.svg" class="prov-svg" style="width: 100%; height: 100%; display: block;" />`
    };
  }
  if (n.includes('glm') || n.includes('智谱') || n.includes('bigmodel')) {
    return {
      style: 'background: transparent; box-shadow: none;',
      html: `<img src="assets/zhipu.svg" class="prov-svg" style="width: 100%; height: 100%; display: block;" />`
    };
  }
  if (n.includes('mimo') || n.includes('小米') || n.includes('xiaomi')) {
    return {
      style: 'background: transparent; box-shadow: none;',
      html: `<img src="assets/xiaomi.svg" class="prov-svg" style="width: 100%; height: 100%; display: block;" />`
    };
  }
  if (n.includes('zenmux')) {
    return {
      style: 'background: transparent; box-shadow: none;',
      html: `<img src="assets/zenmux.svg" class="prov-svg" style="width: 100%; height: 100%; display: block;" />`
    };
  }

  let svgContent = '';
  let themeColor = 'hsl(215, 100%, 60%)'; // default blue

  if (n.includes('claude') || n.includes('anthropic')) {
    themeColor = 'hsl(28, 70%, 48%)'; // Anthropic Bronze
    svgContent = `<path d="M12 2L3 7v10l9 5 9-5V7l-9-5z M12 2v20 M3 12h18" stroke-linecap="round"/>`;
  } else {
    // Custom generic high-tech cube icon
    const h = hashHue(name || '?');
    themeColor = `hsl(${h}, 70%, 52%)`;
    svgContent = `<path d="M12 2L3 7v10l9 5 9-5V7l-9-5z" stroke-linecap="round"/><path d="M12 22V12M12 12L3 7M12 12l9-5" stroke-linecap="round"/>`;
  }

  const h = hashHue(name || '?');
  const gradient = `background: linear-gradient(135deg, ${themeColor}, hsl(${(h + 40) % 360}, 75%, 45%))`;
  
  return {
    style: gradient,
    html: `<svg class="prov-svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">${svgContent}</svg>`
  };
}
function mask(t) { return !t ? '未填密钥' : t.length <= 10 ? '••••' : t.slice(0, 4) + '••••' + t.slice(-4); }

/* ---------- hero / status ---------- */
function showHeroNote(text, warn) {
  const n = $('heroNote');
  n.textContent = text;
  n.className = 'hero-hint' + (warn ? ' warn' : '');
}
function hideHeroNote() { $('heroNote').className = 'hero-hint hidden'; }

function renderHero() {
  const hero = $('hero');
  const ap = activeProvider();
  if (status.connected) {
    hero.classList.add('connected');
    $('heroIcon').innerHTML = I.connected || '';
    $('heroTitle').textContent = ap ? ap.name : '已接入';
    $('heroSub').innerHTML = ap ? `经 <b>${escapeHtml(ap.name)}</b> 转发 · 点卡片切换` : '网关运行中';
    $('btnConnect').textContent = '断开';
    showHeroNote('保持运行。切换服务后重启 Claude Code 新会话生效。', false);
  } else {
    hero.classList.remove('connected');
    $('heroIcon').innerHTML = I.connect || '';
    $('heroTitle').textContent = '未接入';
    $('heroSub').textContent = '选择服务，一键写入 Claude Code 配置';
    $('btnConnect').textContent = '一键接入';
    hideHeroNote();
  }
}

function renderStatus() {
  const chip = $('statusPill');
  if (status.connected) {
    chip.className = 'status-chip on';
    chip.innerHTML = '<span class="status-dot"></span><span class="status-text">已接入</span>';
    $('brandTitle').classList.add('running');
  } else {
    chip.className = 'status-chip';
    chip.innerHTML = '<span class="status-dot"></span><span class="status-text">未接入</span>';
    $('brandTitle').classList.remove('running');
  }
}

function renderConnect() {
  const port = (status.running && status.port) || config.port;
  $('endpoint').textContent = `http://localhost:${port}`;
  $('portInput').value = config.port;
  const token = config.requireToken && config.gatewayToken ? config.gatewayToken : 'clawdy-local';
  $('exportBlock').textContent = [
    `export ANTHROPIC_BASE_URL=http://localhost:${port}`,
    `export ANTHROPIC_AUTH_TOKEN=${token}`,
    '',
    '# 一般无需手动设置，点上方「一键接入」即可',
  ].join('\n');
  $('claudePath').textContent = status.claudePath ? 'Claude 配置：' + status.claudePath : '';
  $('fOpenAtLogin').checked = !!config.openAtLogin;
  $('fRequireToken').checked = !!config.requireToken;
  $('fGatewayToken').value = config.gatewayToken || '';
  $('tokenRow').classList.toggle('hidden', !config.requireToken);
  const tu = config.trayUsage || { enabled: false, range: '7d' };
  $('fTrayUsage').checked = !!tu.enabled;
  $('fTrayRange').value = tu.range || '7d';
  $('trayRangeRow').classList.toggle('hidden', !tu.enabled);
  const se = $('startError');
  if (status.lastStartError) { se.textContent = status.lastStartError; se.classList.remove('hidden'); }
  else se.classList.add('hidden');
}

function renderProviders() {
  const list = $('providerList');
  list.innerHTML = '';
  $('emptyProviders').classList.toggle('hidden', config.providers.length > 0);
  for (const p of config.providers) {
    const isActive = p.id === config.activeProviderId;
    const el = document.createElement('div');
    el.className = 'provider' + (isActive ? ' active' : '');
    el.draggable = true;
    el.dataset.id = p.id;

    const tags = [];
    if (p.defaultModel) tags.push(`<span class="tag">主 ${escapeHtml(p.defaultModel)}</span>`);
    if (p.smallFastModel && p.smallFastModel !== p.defaultModel) tags.push(`<span class="tag">快 ${escapeHtml(p.smallFastModel)}</span>`);
    for (const m of p.models || []) tags.push(`<span class="tag map">${escapeHtml(m.alias)} → ${escapeHtml(m.upstream)}</span>`);

    const iconData = renderProviderIcon(p.name);
    el.innerHTML = `
      <span class="grip" title="拖动排序">⠿</span>
      <div class="prov-icon" style="${iconData.style}">${iconData.html}</div>
      <div class="pinfo">
        <div class="pname">${escapeHtml(p.name)} ${isActive ? '<span class="badge-active">活跃</span>' : ''}</div>
        <div class="pmeta">${escapeHtml(mask(p.authToken))} · ${escapeHtml(p.baseUrl.replace(/^https?:\/\//,''))}</div>
      </div>
      <div class="pmodels">${tags.join('') || '<span class="caption">—</span>'}</div>
      <div class="pactions">
        <button title="测试" data-test="${p.id}">${I.refresh || '↻'}</button>
        <button title="编辑" data-edit="${p.id}">${I.edit || '✎'}</button>
        <button title="删除" class="danger" data-del="${p.id}">${I.trash || '⌫'}</button>
      </div>`;
    list.appendChild(el);
  }
}

function renderMonitor() {
  $('mStatusText').textContent = status.connected ? '已接入' : status.running ? '运行中' : '未接入';
  $('mStatus').querySelector('.pulse-dot, .live-dot').className = 'pulse-dot ' + (status.connected || status.running ? 'on' : 'off');
  $('mEndpoint').textContent = `localhost:${(status.running && status.port) || config.port}`;
  const ap = activeProvider();
  $('mActive').textContent = ap ? ap.name : '—';
  $('mActiveUrl').textContent = ap ? ap.baseUrl : '未选择服务';
  $('mTotal').textContent = stats.total;
  $('mSuccess').textContent = stats.total ? `成功率 ${Math.round((stats.ok / stats.total) * 100)}%` : '成功率 —';
  $('mAvg').innerHTML = stats.total ? `${Math.round(stats.sumMs / stats.total)} <span class="unit">ms</span>` : `— <span class="unit">ms</span>`;
  $('mLast').textContent = stats.last ? `最近 ${stats.last}` : '最近 —';
}

function renderAll() { renderStatus(); renderHero(); renderConnect(); renderProviders(); renderMonitor(); }

/* ---------- monitor stream ---------- */
function pushStreamRow(r) {
  stats.total++;
  if (r.status >= 200 && r.status < 400) stats.ok++;
  stats.sumMs += r.ms || 0;
  stats.last = new Date().toLocaleTimeString();
  renderMonitor();
  $('streamHint').textContent = `已转发 ${stats.total} 次`;
  const list = $('streamList');
  const empty = list.querySelector('.state-inline, .empty');
  if (empty) empty.remove();
  const okCls = r.status >= 200 && r.status < 400 ? 'ok' : 'err';
  const row = document.createElement('div');
  row.className = 'stream-row';
  if (r.id != null) { row.dataset.id = r.id; row.classList.add('clickable'); row.title = '点击查看完整请求 / 响应'; }
  const agentTag = r.agentId ? '<span class="agent-tag sub" title="子代理请求">子</span>' : '';
  row.innerHTML = `
    <span class="sdot ${okCls}"></span>
    <span class="method">${escapeHtml(r.method || '')}</span>
    ${agentTag}
    <span class="models">
      <span class="req" title="HTTP body.model">${escapeHtml(r.requestedModel || '-')}</span>
      <span class="arrow">→</span>
      <span class="out" title="上游实际模型">${escapeHtml(r.outgoingModel || '-')}</span>
      ${r.rewritten ? '<span class="rewrite" title="响应 model 已改回客户端名称">✎</span>' : ''}
    </span>
    <span class="prov">${escapeHtml(r.provider || '')}</span>
    <span class="code ${okCls}">${r.status}</span>
    <span class="ms">${r.ms}ms</span>
    <span class="ts">${new Date().toLocaleTimeString()}</span>`;
  list.insertBefore(row, list.firstChild);
  while (list.children.length > 120) list.removeChild(list.lastChild);
}
function pushRawLog(l) {
  const el = $('rawLog');
  const line = document.createElement('div');
  line.textContent = `[${new Date().toLocaleTimeString()}] ${l.level || 'info'} — ${l.msg}`;
  el.insertBefore(line, el.firstChild);
  while (el.children.length > 120) el.removeChild(el.lastChild);
}

/* ---------- request inspector (full headers + body of one forwarded exchange) ---------- */
let reqDrawerTab = 'req';
let reqDrawerData = null;

function fmtBytes(n) { n = n || 0; if (n < 1024) return n + ' B'; if (n < 1048576) return (n / 1024).toFixed(1) + ' KB'; return (n / 1048576).toFixed(2) + ' MB'; }
function prettyBody(cap) {
  if (!cap || !cap.text) return '<div class="dr-empty">（空）</div>';
  let text = cap.text, lang = 'plaintext';
  const trimmed = text.trim();
  if (trimmed.startsWith('{') || trimmed.startsWith('[')) {
    try { text = JSON.stringify(JSON.parse(trimmed), null, 2); lang = 'json'; } catch (_) {}
  }
  const note = cap.truncated ? `<div class="dr-trunc">仅显示前 ${fmtBytes(cap.bytes - cap.truncated)} / 共 ${fmtBytes(cap.bytes)}（已截断）</div>` : '';
  return note + `<pre class="dr-pre"><code class="language-${lang}">${escapeHtml(text)}</code></pre>`;
}
function kvTable(h) {
  const keys = Object.keys(h || {});
  if (!keys.length) return '<div class="dr-empty">（无）</div>';
  return '<div class="dr-kv">' + keys.map((k) => `<div class="dr-kv-row"><span class="dr-k">${escapeHtml(k)}</span><span class="dr-v">${escapeHtml(Array.isArray(h[k]) ? h[k].join(', ') : h[k])}</span></div>`).join('') + '</div>';
}
function renderReqDrawerBody() {
  const d = reqDrawerData;
  if (!d) return;
  const body = $('reqDrawerBody');
  const isReq = reqDrawerTab === 'req';
  const headers = isReq ? d.reqHeaders : d.resHeaders;
  const cap = isReq ? d.reqBody : d.resBody;
  const which = isReq ? 'req' : 'res';
  const copyLabel = cap && cap.truncated ? '复制(部分)' : '复制';
  const headTitle = isReq
    ? `请求头 <span class="dr-sub">${escapeHtml(d.method || 'POST')} ${escapeHtml(d.url || d.path || '')}</span>`
    : `响应头 <span class="dr-sub">HTTP ${escapeHtml(d.status || '')}</span>`;
  body.innerHTML = `<div class="dr-section-title">${headTitle}</div>${kvTable(headers)}<div class="dr-section-title">${isReq ? '请求体' : '响应体'} <button class="btn btn-sm dr-copy" data-copy-body="${which}" title="复制已捕获内容">${copyLabel}</button></div>${prettyBody(cap)}`;
  // Skip syntax highlighting on very large bodies — hljs on multi-MB text freezes the UI.
  body.querySelectorAll('pre code').forEach((b) => { if (b.textContent.length > 100000) return; try { if (window.hljs) window.hljs.highlightElement(b); } catch (_) {} });
}
async function openReqDetail(id) {
  let d = null;
  try { d = await api.monitorGet(id); } catch (_) {}
  if (!d) {
    // Entry rolled out of the bounded capture buffer — give feedback instead of a stale drawer.
    reqDrawerData = null;
    $('drMethod').textContent = '—';
    $('drStatus').textContent = ''; $('drStatus').className = 'dr-status';
    $('drModel').textContent = '';
    $('reqMeta').innerHTML = '';
    $('reqDrawerBody').innerHTML = '<div class="dr-empty">该请求的详情已不可用（仅保留最近 30 条转发的完整报文）。</div>';
    $('reqDrawer').classList.remove('hidden');
    return;
  }
  reqDrawerData = d; reqDrawerTab = 'req';
  const ok = d.status >= 200 && d.status < 400;
  $('drMethod').textContent = d.method || 'POST';
  $('drStatus').textContent = d.status != null ? d.status : '—';
  $('drStatus').className = 'dr-status ' + (ok ? 'ok' : 'err');
  $('drModel').innerHTML = `${escapeHtml(d.requestedModel || '-')} <span class="arrow">→</span> ${escapeHtml(d.outgoingModel || '-')}${d.rewritten ? ' <span class="rewrite" title="响应已改回客户端名称">✎</span>' : ''}`;
  const meta = [
    ['服务', d.provider],
    ['耗时', d.ms != null ? d.ms + ' ms' : ''],
    ['会话', d.sessionId ? String(d.sessionId).slice(0, 8) : ''],
    d.agentId ? ['代理', '子代理'] : null,
    ['时间', d.ts ? new Date(d.ts).toLocaleTimeString() : ''],
    d.error ? ['错误', d.error] : null,
  ].filter((r) => r && r[1]);
  $('reqMeta').innerHTML = meta.map((r) => `<span class="dr-chip"><span class="muted">${escapeHtml(r[0])}</span> ${escapeHtml(r[1])}</span>`).join('');
  document.querySelectorAll('.dr-tab').forEach((t) => t.classList.toggle('active', t.dataset.tab === reqDrawerTab));
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
    b.type = 'button'; b.className = 'preset-chip'; b.dataset.preset = key; b.textContent = PRESET_LABELS[key];
    grid.appendChild(b);
  });
}
function selectPreset(key) {
  document.querySelectorAll('.preset-chip').forEach((c) => c.classList.toggle('selected', c.dataset.preset === key));
  const p = PRESETS[key] || PRESETS.custom;
  $('fName').value = p.name; $('fBaseUrl').value = p.baseUrl; $('fDefaultModel').value = p.defaultModel; $('fSmallModel').value = p.smallFastModel;
  updateIconPreview();
  if (key !== 'custom') $('fToken').focus();
}
function updateIconPreview() {
  const name = $('fName').value || '?';
  const el = $('fIconPreview');
  const iconData = renderProviderIcon(name);
  el.setAttribute('style', iconData.style);
  el.innerHTML = iconData.html;
}
function addMapRow(alias = '', upstream = '') {
  const row = document.createElement('div');
  row.className = 'map-row';
  row.innerHTML = `
    <input class="m-alias" placeholder="别名 例如 claude-opus-4.8[1m]" />
    <span class="map-arrow">→</span>
    <input class="m-upstream" placeholder="上游模型 例如 glm-5.2[1m]" />
    <button class="icon-btn m-del" type="button">✕</button>`;
  row.querySelector('.m-alias').value = alias;
  row.querySelector('.m-upstream').value = upstream;
  row.querySelector('.m-del').addEventListener('click', () => row.remove());
  $('mapRows').appendChild(row);
}
function openModal(provider) {
  editingId = provider ? provider.id : null;
  $('modalTitle').textContent = provider ? '编辑服务' : '添加服务';
  document.querySelectorAll('.preset-chip').forEach((c) => c.classList.remove('selected'));
  $('fName').value = provider ? provider.name : '';
  $('fBaseUrl').value = provider ? provider.baseUrl : '';
  $('fToken').value = provider ? provider.authToken : '';
  $('fToken').type = 'password'; $('fTokenToggle').textContent = '显示';
  $('fDefaultModel').value = provider ? provider.defaultModel : '';
  $('fSmallModel').value = provider ? provider.smallFastModel : '';
  $('fMapDefault').checked = provider ? provider.mapDefaultModels !== false : true;
  $('mapRows').innerHTML = '';
  if (provider && provider.models) provider.models.forEach((m) => addMapRow(m.alias, m.upstream));
  if (!$('mapRows').children.length) addMapRow(); // always show one empty row to add into
  const mapDetails = $('mapRows').closest('details');
  if (mapDetails) mapDetails.open = true;
  updateIconPreview();
  $('testResult').className = 'alert hidden';
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
    name: $('fName').value.trim() || '未命名',
    baseUrl: $('fBaseUrl').value.trim(),
    authToken: $('fToken').value.trim(),
    defaultModel: $('fDefaultModel').value.trim(),
    smallFastModel: $('fSmallModel').value.trim(),
    mapDefaultModels: $('fMapDefault').checked,
    models,
  };
  if (editingId) p.id = editingId;
  return p;
}

/* ---------- actions ---------- */
async function refresh() {
  config = await api.getConfig();
  status = await api.serverStatus();
  renderAll();
}
async function persist(patch) {
  try { config = await api.saveConfig(Object.assign({}, config, patch)); }
  catch (e) { pushRawLog({ level: 'error', msg: '保存失败：' + (e && e.message ? e.message : e) }); }
  status = await api.serverStatus();
  renderAll();
}
async function toggleConnect() {
  const btn = $('btnConnect');
  const wasConnected = status.connected;
  btn.disabled = true;
  btn.textContent = wasConnected ? '断开中…' : '接入中…';
  const res = wasConnected ? await api.disconnect() : await api.connect();
  btn.disabled = false;
  status = await api.serverStatus();
  renderAll();
    if (!res.ok) showHeroNote(res.message || '操作失败', true);
}
function copyFeedback(btn, text) {
  const orig = btn.dataset.copyOrig || (btn.dataset.copyOrig = btn.textContent);
  api.copy(text);
  btn.textContent = '已复制 ✓';
  clearTimeout(btn._t);
  btn._t = setTimeout(() => (btn.textContent = orig), 1500);
}
function genToken() {
  const a = new Uint8Array(18);
  crypto.getRandomValues(a);
  return 'clawdy_' + Array.from(a).map((b) => b.toString(16).padStart(2, '0')).join('');
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
      void target.offsetWidth;
      target.style.transition = 'opacity 0.22s cubic-bezier(0.23, 1, 0.32, 1)';
      target.style.opacity = '1';
      setTimeout(() => {
        if (target) target.style.transition = '';
      }, 280);
    }

    if (view === 'conversations' && window.ClawdyConversations) window.ClawdyConversations.onShow();
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
  try { localStorage.setItem('clawdy-theme', t); } catch (_) {}
  const dark = t === 'dark';
  const hd = document.getElementById('hljs-dark');
  const hl = document.getElementById('hljs-light');
  if (hd) hd.disabled = !dark;
  if (hl) hl.disabled = dark;
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
function bind() {
  if ($('appLogo') && I.logo) $('appLogo').innerHTML = I.logo(30);
  injectIcons();

  $('tabs').addEventListener('click', (e) => {
    const btn = e.target.closest('.nav-item, .seg-btn');
    if (btn && btn.dataset.view) switchView(btn.dataset.view);
  });
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
      if (localStorage.getItem('clawdy-sidebar-collapsed') === '1') {
        sidebar.classList.add('collapsed');
        const icon = collapseBtn.querySelector('[data-icon]');
        if (icon && I.chevronRight) icon.innerHTML = I.chevronRight;
      }
    } catch (_) {}
    collapseBtn.addEventListener('click', () => {
      const isCollapsed = sidebar.classList.toggle('collapsed');
      const icon = collapseBtn.querySelector('[data-icon]');
      if (icon) icon.innerHTML = isCollapsed ? (I.chevronRight || '›') : (I.chevronLeft || '‹');
      try { localStorage.setItem('clawdy-sidebar-collapsed', isCollapsed ? '1' : '0'); } catch (_) {}
    });
  }
  $('btnConnect').addEventListener('click', toggleConnect);

  $('portInput').addEventListener('change', async (e) => {
    const port = Number(e.target.value);
    if (!Number.isInteger(port) || port < 1 || port > 65535) {
      e.target.value = config.port;
      pushRawLog({ level: 'error', msg: '端口无效：请输入 1–65535 的整数' });
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
  $('fTrayUsage').addEventListener('change', (e) => persist({ trayUsage: { enabled: e.target.checked, range: $('fTrayRange').value } }));
  $('fTrayRange').addEventListener('change', (e) => persist({ trayUsage: { enabled: $('fTrayUsage').checked, range: e.target.value } }));

  $('btnAdd').addEventListener('click', () => openModal(null));
  const btnAddEmpty = $('btnAddEmpty');
  if (btnAddEmpty) btnAddEmpty.addEventListener('click', () => openModal(null));

  $('providerList').addEventListener('click', async (e) => {
    // Resolve the actual button (the click may land on the inner SVG icon).
    const btn = e.target.closest('button');
    if (btn && btn.dataset.edit) { openModal(config.providers.find((p) => p.id === btn.dataset.edit)); return; }
    if (btn && btn.dataset.del) { if (confirm('确定删除这个服务？')) { config = await api.deleteProvider(btn.dataset.del); renderAll(); } return; }
    if (btn && btn.dataset.test) {
      const p = config.providers.find((pp) => pp.id === btn.dataset.test);
      const orig = btn.innerHTML; // preserve the SVG icon, restore it after
      btn.innerHTML = '…'; btn.disabled = true;
      const res = await api.testProvider(p);
      btn.disabled = false; btn.innerHTML = res.ok ? '✓' : '✗';
      pushRawLog({ level: res.ok ? 'info' : 'error', msg: `测试「${p.name}」: ${res.message}` });
      setTimeout(() => { btn.innerHTML = orig; }, 1800);
      return;
    }
    if (btn) return; // some other button — ignore
    // click anywhere else on the card → set it as the active service
    const card = e.target.closest('.provider');
    if (card && card.dataset.id) { config = await api.setActive(card.dataset.id); renderAll(); }
  });
  wireDrag();

  $('modalClose').addEventListener('click', closeModal);
  $('btnCancel').addEventListener('click', closeModal);
  $('modal').addEventListener('click', (e) => { if (e.target.id === 'modal') closeModal(); });
  $('presetGrid').addEventListener('click', (e) => { if (e.target.dataset.preset) selectPreset(e.target.dataset.preset); });
  $('fName').addEventListener('input', updateIconPreview);
  $('btnAddMap').addEventListener('click', () => addMapRow());
  $('fTokenToggle').addEventListener('click', () => {
    const f = $('fToken'); const show = f.type === 'password';
    f.type = show ? 'text' : 'password'; $('fTokenToggle').textContent = show ? '隐藏' : '显示';
  });
  $('btnSave').addEventListener('click', async () => {
    const p = collectProvider();
    if (!p.baseUrl) { const tr = $('testResult'); tr.className = 'alert err'; tr.textContent = '请填写 API 地址'; return; }
    config = await api.upsertProvider(p);
    closeModal(); renderAll();
  });
  $('btnTest').addEventListener('click', async () => {
    const tr = $('testResult'); tr.className = 'alert pending'; tr.textContent = '测试中…';
    const res = await api.testProvider(collectProvider());
    tr.className = 'alert ' + (res.ok ? 'ok' : 'err'); tr.textContent = (res.ok ? '✓ ' : '✗ ') + res.message;
  });

  $('btnClearLog').addEventListener('click', () => {
    $('streamList').innerHTML = '<div class="state-inline">接入网关后，转发记录将实时显示</div>';
    $('rawLog').innerHTML = '';
    stats.total = stats.ok = stats.sumMs = 0; stats.last = null;
    $('streamHint').textContent = '等待请求…';
    renderMonitor();
    if (api.monitorClear) api.monitorClear();
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
  document.querySelectorAll('.dr-tab').forEach((t) => t.addEventListener('click', () => {
    reqDrawerTab = t.dataset.tab;
    document.querySelectorAll('.dr-tab').forEach((x) => x.classList.toggle('active', x === t));
    renderReqDrawerBody();
  }));
  const reqDrawerBody = $('reqDrawerBody');
  if (reqDrawerBody) reqDrawerBody.addEventListener('click', (e) => {
    const cb = e.target.closest('[data-copy-body]');
    if (cb && reqDrawerData) {
      const cap = cb.dataset.copyBody === 'req' ? reqDrawerData.reqBody : reqDrawerData.resBody;
      api.copy((cap && cap.text) || '');
      cb.textContent = '已复制'; setTimeout(() => { cb.textContent = '复制'; }, 1200);
    }
  });
  document.addEventListener('keydown', (e) => { if (e.key === 'Escape' && reqDrawer && !reqDrawer.classList.contains('hidden')) closeReqDrawer(); });

  api.onRequest((r) => pushStreamRow(r));
  api.onLog((l) => pushRawLog(l));
  api.onStatus((s) => { status = s; renderAll(); });
}

try { applyTheme(localStorage.getItem('clawdy-theme') || 'light'); } catch (_) { applyTheme('light'); }
renderPresetGrid();
bind();
refresh();
