'use strict';

const api = window.clawdy;
let config = { port: 8788, activeProviderId: null, providers: [] };
let status = { running: false, port: null, connected: false, lastStartError: null, claudePath: '' };
let editingId = null;
let dragId = null;
const stats = { total: 0, ok: 0, sumMs: 0, last: null };

const $ = (id) => document.getElementById(id);

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
function iconStyle(name) { const h = hashHue(name || '?'); return `background:linear-gradient(135deg,hsl(${h} 70% 58%),hsl(${(h + 40) % 360} 70% 48%))`; }
function iconLetter(name) { return escapeHtml(((name || '?').trim()[0] || '?').toUpperCase()); }
function mask(t) { return !t ? '未填密钥' : t.length <= 10 ? '••••' : t.slice(0, 4) + '••••' + t.slice(-4); }

/* ---------- hero / status ---------- */
function showHeroNote(text, warn) {
  const n = $('heroNote');
  n.textContent = text;
  n.className = 'hero-note' + (warn ? ' warn' : '');
}
function hideHeroNote() { $('heroNote').className = 'hero-note hidden'; }

function renderHero() {
  const hero = $('hero');
  const ap = activeProvider();
  if (status.connected) {
    hero.classList.add('connected');
    $('heroIcon').textContent = '✓';
    $('heroTitle').textContent = '已接入 Claude Code';
    $('heroSub').innerHTML = ap ? `正在通过 <b>${escapeHtml(ap.name)}</b> 工作 · 切换下方服务即时生效` : '已接入（请在下方选择一个服务）';
    $('btnConnect').textContent = '断开接入';
    showHeroNote('保持 Clawdy 运行即可持续工作。首次接入或更换端口后，需重启 Claude Code（新开会话）生效。', false);
  } else {
    hero.classList.remove('connected');
    $('heroIcon').textContent = '⏻';
    $('heroTitle').textContent = '未接入 Claude Code';
    $('heroSub').textContent = '打开开关，让 Claude Code 自动使用你选中的服务，随时一键切换或关闭。';
    $('btnConnect').textContent = '一键接入';
    hideHeroNote();
  }
}

function renderStatus() {
  const pill = $('statusPill');
  if (status.connected) {
    pill.className = 'pill pill-on';
    pill.innerHTML = '<span class="dot"></span>已接入';
    $('brandTitle').classList.add('running');
  } else {
    pill.className = 'pill pill-off';
    pill.innerHTML = '<span class="dot"></span>未接入';
    $('brandTitle').classList.remove('running');
  }
}

function renderConnect() {
  const port = (status.running && status.port) || config.port;
  $('endpoint').textContent = `http://127.0.0.1:${port}`;
  $('portInput').value = config.port;
  const token = config.requireToken && config.gatewayToken ? config.gatewayToken : 'clawdy-local';
  $('exportBlock').textContent = [
    `export ANTHROPIC_BASE_URL=http://127.0.0.1:${port}`,
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
  if (status.lastStartError) { se.textContent = '⚠ ' + status.lastStartError; se.classList.remove('hidden'); }
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
    el.innerHTML = `
      <span class="grip" title="拖动排序">⠿</span>
      <span class="radio-wrap"><input type="radio" name="active" ${isActive ? 'checked' : ''} data-act="${p.id}" title="设为正在使用" /></span>
      <div class="prov-icon" style="${iconStyle(p.name)}">${iconLetter(p.name)}</div>
      <div class="pinfo">
        <div class="pname">${escapeHtml(p.name)} ${isActive ? '<span class="badge-active">正在使用</span>' : ''}</div>
        <div class="purl" data-url="${escapeHtml(p.baseUrl)}">${escapeHtml(p.baseUrl)} · ${escapeHtml(mask(p.authToken))}</div>
        <div class="pmodels">${tags.join('') || '<span class="muted small">未配置模型</span>'}</div>
      </div>
      <div class="pright"><div class="pactions">
        <button class="btn btn-sm" data-test="${p.id}">测试</button>
        <button class="btn btn-sm" data-edit="${p.id}">编辑</button>
        <button class="btn btn-sm btn-danger" data-del="${p.id}">删除</button>
      </div></div>`;
    list.appendChild(el);
  }
}

function renderMonitor() {
  $('mStatusText').textContent = status.connected ? '已接入' : status.running ? '运行中' : '未接入';
  $('mStatus').querySelector('.live-dot').className = 'live-dot ' + (status.connected || status.running ? 'on' : 'off');
  $('mEndpoint').textContent = `127.0.0.1:${(status.running && status.port) || config.port}`;
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
  const empty = list.querySelector('.empty');
  if (empty) empty.remove();
  const okCls = r.status >= 200 && r.status < 400 ? 'ok' : 'err';
  const row = document.createElement('div');
  row.className = 'stream-row';
  row.innerHTML = `
    <span class="sdot ${okCls}"></span>
    <span class="method">${escapeHtml(r.method || '')}</span>
    <span class="models">
      <span class="req">${escapeHtml(r.requestedModel || '-')}</span>
      <span class="arrow">→</span>
      <span class="out">${escapeHtml(r.outgoingModel || '-')}</span>
      ${r.rewritten ? '<span class="rewrite" title="响应模型名已改回">✎</span>' : ''}
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
  el.setAttribute('style', iconStyle(name));
  el.textContent = iconLetter(name);
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
  updateIconPreview();
  $('testResult').className = 'banner hidden';
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
  if (!res.ok) showHeroNote('⚠ ' + (res.message || '操作失败'), true);
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
  document.querySelectorAll('.seg-btn').forEach((b) => b.classList.toggle('active', b.dataset.view === view));
  $('view-providers').classList.toggle('hidden', view !== 'providers');
  $('view-monitor').classList.toggle('hidden', view !== 'monitor');
  $('btnAdd').classList.toggle('hidden', view !== 'providers');
}
function applyTheme(t) {
  document.documentElement.setAttribute('data-theme', t);
  try { localStorage.setItem('clawdy-theme', t); } catch (_) {}
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
  $('tabs').addEventListener('click', (e) => { if (e.target.dataset.view) switchView(e.target.dataset.view); });
  $('btnTheme').addEventListener('click', () => {
    const cur = document.documentElement.getAttribute('data-theme') || 'light';
    applyTheme(cur === 'light' ? 'dark' : 'light');
  });
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

  $('providerList').addEventListener('click', async (e) => {
    const t = e.target;
    if (t.dataset.edit) openModal(config.providers.find((p) => p.id === t.dataset.edit));
    else if (t.dataset.del) { if (confirm('确定删除这个服务？')) { config = await api.deleteProvider(t.dataset.del); renderAll(); } }
    else if (t.dataset.test) {
      const p = config.providers.find((pp) => pp.id === t.dataset.test);
      t.textContent = '测试中…'; t.disabled = true;
      const res = await api.testProvider(p);
      t.disabled = false; t.textContent = res.ok ? '✓ 正常' : '✗ 失败';
      pushRawLog({ level: res.ok ? 'info' : 'error', msg: `测试「${p.name}」: ${res.message}` });
      setTimeout(() => (t.textContent = '测试'), 2500);
    } else if (t.classList.contains('purl') && t.dataset.url) api.openExternal(t.dataset.url);
  });
  $('providerList').addEventListener('change', async (e) => {
    if (e.target.dataset.act) { config = await api.setActive(e.target.dataset.act); renderAll(); }
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
    if (!p.baseUrl) { const tr = $('testResult'); tr.className = 'banner err'; tr.textContent = '请填写服务地址（API 地址）'; return; }
    config = await api.upsertProvider(p);
    closeModal(); renderAll();
  });
  $('btnTest').addEventListener('click', async () => {
    const tr = $('testResult'); tr.className = 'banner pending'; tr.textContent = '测试中…';
    const res = await api.testProvider(collectProvider());
    tr.className = 'banner ' + (res.ok ? 'ok' : 'err'); tr.textContent = (res.ok ? '✓ ' : '✗ ') + res.message;
  });

  $('btnClearLog').addEventListener('click', () => {
    $('streamList').innerHTML = '<div class="empty small">还没有请求。</div>';
    $('rawLog').innerHTML = '';
    stats.total = stats.ok = stats.sumMs = 0; stats.last = null;
    $('streamHint').textContent = '等待请求…';
    renderMonitor();
  });

  api.onRequest((r) => pushStreamRow(r));
  api.onLog((l) => pushRawLog(l));
  api.onStatus((s) => { status = s; renderAll(); });
}

try { applyTheme(localStorage.getItem('clawdy-theme') || 'light'); } catch (_) { applyTheme('light'); }
renderPresetGrid();
bind();
refresh();
