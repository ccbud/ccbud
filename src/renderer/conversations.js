'use strict';

/* "对话" view — reads Claude Code's on-disk session history (~/.claude/projects) directly
   and renders it claude-code-history-viewer style: projects → sessions tree, a rich message
   timeline (text / thinking / per-tool cards + results / diffs / code / images), live-follow
   for active sessions, per-session stats, and in-conversation search. */
(function () {
  const api = window.clawdy;
  if (!api) return;
  const $ = (id) => document.getElementById(id);

  let projects = [];      // [{ cwd, name, sessions:[...], lastActivity }]
  let openId = null;
  let openFile = null;
  let search = '';
  let listTimer = null;
  let collapsed = new Set(); // collapsed project cwds
  let lastRender = { file: null, count: -1 };

  try { collapsed = new Set(JSON.parse(localStorage.getItem('clawdy-collapsed-projects') || '[]')); } catch (_) {}
  function persistCollapsed() { try { localStorage.setItem('clawdy-collapsed-projects', JSON.stringify([...collapsed])); } catch (_) {} }

  // Detail search state
  let detailSearchHighlights = [];
  let detailSearchIndex = -1;

  if (window.marked && window.marked.setOptions) {
    window.marked.setOptions({ gfm: true, breaks: true });
    // Defense-in-depth: never pass raw HTML from model/user text through to the DOM.
    try {
      window.marked.use({ renderer: { html: (tok) => esc(typeof tok === 'string' ? tok : (tok && tok.text) || '') } });
    } catch (_) {}
  }

  /* ---------- helpers ---------- */
  function esc(s) {
    return String(s == null ? '' : s).replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
  }
  function fmtTok(n) {
    n = n || 0;
    if (n < 1000) return String(n);
    if (n < 1e6) return (n / 1e3).toFixed(n < 1e4 ? 1 : 0).replace(/\.0$/, '') + 'K';
    return (n / 1e6).toFixed(1).replace(/\.0$/, '') + 'M';
  }
  function truncate(s, n) { s = String(s == null ? '' : s); return s.length > n ? s.slice(0, n) + `\n… (+${s.length - n} 字符)` : s; }
  function md(text) { try { return window.marked ? window.marked.parse(String(text || '')) : esc(text); } catch (_) { return esc(text); } }
  function normContent(c) {
    if (typeof c === 'string') return c ? [{ type: 'text', text: c }] : [];
    return Array.isArray(c) ? c : [];
  }
  function projName(cwd) { return cwd ? cwd.split('/').filter(Boolean).pop() : null; }
  function isLive(ts) { return ts && (Date.now() - ts) < 90000; }

  function relTime(ts) {
    if (!ts) return '';
    const d = Date.now() - ts;
    if (d < 60000) return '刚刚';
    if (d < 3600000) return Math.floor(d / 60000) + ' 分钟前';
    if (d < 86400000) return Math.floor(d / 3600000) + ' 小时前';
    if (d < 7 * 86400000) return Math.floor(d / 86400000) + ' 天前';
    return new Date(ts).toLocaleDateString();
  }

  function escapeRegExp(str) { return str.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'); }

  function clearDetailSearchHighlights() {
    const host = $('convDetail');
    if (!host) return;
    host.querySelectorAll('.search-highlight').forEach((span) => {
      const parent = span.parentNode;
      if (parent) parent.replaceChild(document.createTextNode(span.textContent), span);
    });
    detailSearchHighlights = [];
    detailSearchIndex = -1;
    const countEl = $('convDetailSearchCount');
    if (countEl) countEl.textContent = '';
  }

  function applySearchHighlightsToHost(host, query) {
    const highlights = [];
    if (!query) return highlights;
    const regex = new RegExp(escapeRegExp(query), 'gi');
    const walker = document.createTreeWalker(host, NodeFilter.SHOW_TEXT, null);
    const textNodes = [];
    let node;
    while ((node = walker.nextNode())) {
      if (node.nodeValue && node.nodeValue.trim()) textNodes.push(node);
    }
    textNodes.forEach((textNode) => {
      const text = textNode.nodeValue;
      let lastIdx = 0;
      let match;
      const frag = document.createDocumentFragment();
      let matched = false;
      regex.lastIndex = 0;
      while ((match = regex.exec(text)) !== null) {
        matched = true;
        const start = match.index;
        const len = match[0].length;
        if (start > lastIdx) frag.appendChild(document.createTextNode(text.substring(lastIdx, start)));
        const mark = document.createElement('span');
        mark.className = 'search-highlight';
        mark.textContent = match[0];
        frag.appendChild(mark);
        highlights.push(mark);
        lastIdx = start + len;
      }
      if (matched) {
        if (lastIdx < text.length) frag.appendChild(document.createTextNode(text.substring(lastIdx)));
        textNode.parentNode.replaceChild(frag, textNode);
      }
    });
    return highlights;
  }

  function gotoDetailSearchMatch(newIndex) {
    if (!detailSearchHighlights.length) return;
    if (detailSearchIndex >= 0 && detailSearchHighlights[detailSearchIndex]) detailSearchHighlights[detailSearchIndex].classList.remove('current');
    detailSearchIndex = ((newIndex % detailSearchHighlights.length) + detailSearchHighlights.length) % detailSearchHighlights.length;
    const el = detailSearchHighlights[detailSearchIndex];
    if (el && el.isConnected) {
      el.classList.add('current');
      el.scrollIntoView({ behavior: 'smooth', block: 'center' });
    }
    const countEl = $('convDetailSearchCount');
    if (countEl) countEl.textContent = `${detailSearchIndex + 1}/${detailSearchHighlights.length}`;
  }

  function performDetailSearch(query) {
    const host = $('convDetail');
    if (!host) return;
    clearDetailSearchHighlights();
    if (!query) return;
    detailSearchHighlights = applySearchHighlightsToHost(host, query);
    if (detailSearchHighlights.length > 0) gotoDetailSearchMatch(0);
  }

  /* ---------- list (projects → sessions) ---------- */
  async function refresh() {
    try { projects = (await api.historyProjects()) || []; } catch (_) { projects = []; }
    renderList();
  }

  function filteredProjects() {
    if (!search) return projects;
    const q = search.toLowerCase();
    return projects
      .map((p) => {
        const sessions = p.sessions.filter((s) =>
          (s.title || '').toLowerCase().includes(q) ||
          (s.model || '').toLowerCase().includes(q) ||
          (p.name || '').toLowerCase().includes(q));
        return sessions.length ? Object.assign({}, p, { sessions }) : null;
      })
      .filter(Boolean);
  }

  function renderList() {
    const el = $('convList');
    if (!el) return;
    const list = filteredProjects();
    const total = list.reduce((n, p) => n + p.sessions.length, 0);
    if (!total) {
      el.innerHTML = `<div class="state-inline" style="padding:24px 12px">${search ? '没有匹配的会话' : '未找到本地会话历史<br><span class="muted small">~/.claude/projects</span>'}</div>`;
      return;
    }
    el.innerHTML = list.map((p) => {
      const isCol = collapsed.has(p.cwd || p.name) && !search;
      const items = isCol ? '' : `<div class="conv-proj-sessions">${p.sessions.map(sessionItem).join('')}</div>`;
      return `<div class="conv-proj">
        <div class="conv-proj-head" data-proj="${esc(p.cwd || p.name)}" title="${esc(p.cwd || '')}">
          <span class="conv-proj-caret">${isCol ? '▸' : '▾'}</span>
          <span class="conv-proj-name">${esc(p.name || '(项目)')}</span>
          <span class="conv-proj-count">${p.sessions.length}</span>
        </div>${items}
      </div>`;
    }).join('');
  }

  function sessionItem(c) {
    const live = isLive(c.lastActivity) ? '<span class="conv-live"></span>' : '';
    const sub = c.isSubagent ? '<span class="conv-badge">子代理</span>' : '';
    const model = c.model ? `<span class="conv-model">${esc(c.model)}</span>` : '';
    return `<div class="conv-item ${c.id === openId ? 'active' : ''}" data-id="${esc(c.id)}" data-file="${esc(c.file || '')}">
      <div class="conv-item-top">${live}<span class="conv-title">${esc(c.title || '(对话)')}</span></div>
      <div class="conv-item-sub">${model}${sub}</div>
      <div class="conv-item-meta"><span>${esc(relTime(c.lastActivity))}</span>${c.sizeKB ? '<span>' + c.sizeKB + ' KB</span>' : ''}</div>
    </div>`;
  }

  /* ---------- detail ---------- */
  async function openConversation(id, file) {
    const ds = $('convDetailSearch');
    if (ds) ds.value = '';
    clearDetailSearchHighlights();
    openId = id; openFile = file || null;
    lastRender = { file: null, count: -1 };
    renderList();
    await rerenderDetail(true);
  }

  async function rerenderDetail(force) {
    if (!openFile) return;
    let detail = null;
    try { detail = await api.historyGet(openFile); } catch (_) {}
    const host = $('convDetail');
    if (!host) return;
    if (!detail) { host.innerHTML = '<div class="conv-empty">会话不存在或已被移动。</div>'; lastRender = { file: null, count: -1 }; return; }

    const messages = detail.messages || [];
    // Skip needless re-renders: on-disk turns are written whole, so a stable message count
    // means nothing changed — preserves scroll + expanded thinking/result panels.
    if (!force && lastRender.file === openFile && lastRender.count === messages.length && host.querySelector('.msg')) return;

    const searchActive = !!($('convDetailSearch') && $('convDetailSearch').value.trim());
    clearDetailSearchHighlights();
    const wasBottom = isNearBottom(host);
    host.innerHTML = renderDetail(detail);
    highlight(host);
    if (force || wasBottom) host.scrollTop = host.scrollHeight;
    renderSidePanels(detail);
    lastRender = { file: openFile, count: messages.length };

    if (searchActive) {
      const dsearchInput = $('convDetailSearch');
      setTimeout(() => { if (dsearchInput && dsearchInput.value.trim()) performDetailSearch(dsearchInput.value.trim()); }, 20);
    }
  }
  function isNearBottom(el) { return el.scrollHeight - el.scrollTop - el.clientHeight < 120; }
  function highlight(root) {
    if (!window.hljs) return;
    root.querySelectorAll('pre code').forEach((b) => { if (b.dataset.highlighted) return; try { window.hljs.highlightElement(b); } catch (_) {} });
  }

  function buildResults(messages) {
    const results = {};
    messages.forEach((m) => normContent(m.content).forEach((b) => { if (b.type === 'tool_result') results[b.tool_use_id] = b; }));
    return results;
  }
  // Returns the HTML for one message, or '' for a pure tool_result / meta user turn.
  function renderMessage(m, results) {
    const blocks = normContent(m.content);
    if (m.role === 'user') {
      const vis = blocks.filter((b) => b.type === 'text' || b.type === 'image');
      if (!vis.length) return '';
      const textVal = vis.map((b) => b.text || '').join('');
      if (textVal.includes('<system-reminder>') || textVal.includes('<command-name>') || textVal.includes('<local-command')) return '';
      return `<div class="msg user"><div class="msg-role">👤 你</div><div class="msg-body">${vis.map(renderUserBlock).join('')}</div></div>`;
    }
    let body = '';
    blocks.forEach((b) => {
      if (b.type === 'text') body += `<div class="blk-text">${md(b.text)}</div>`;
      else if (b.type === 'thinking') body += renderThinking(b);
      else if (b.type === 'tool_use') body += renderToolCard(b, results[b.id]);
      else if (b.type === 'image') body += renderUserBlock(b);
      else body += `<pre class="wrap">${esc(JSON.stringify(b))}</pre>`;
    });
    if (!body) return '';
    return `<div class="msg assistant ${m.isSidechain ? 'sidechain' : ''}"><div class="msg-role">✦ Claude${m.isSidechain ? ' <span class="conv-badge">子代理</span>' : ''}</div><div class="msg-body">${body}${turnMeta(m)}</div></div>`;
  }
  function renderDetail(detail) {
    const messages = detail.messages || [];
    const results = buildResults(messages);
    const html = messages.map((m) => renderMessage(m, results)).join('');
    return html || '<div class="conv-empty">（空对话）</div>';
  }

  function renderUserBlock(b) {
    if (b.type === 'image') {
      const s = b.source || {};
      if (s.data) return `<img class="msg-img" src="data:${esc(s.media_type || 'image/png')};base64,${s.data}" />`;
      return `<div class="img-redacted">🖼 图片</div>`;
    }
    return `<div class="blk-text">${md(b.text)}</div>`;
  }
  function renderThinking(b) {
    const t = b.thinking || '';
    const first = t.split('\n').find((x) => x.trim()) || '思考';
    return `<details class="thinking"><summary>💭 思考 · <span class="muted">${esc(first.slice(0, 60))}</span></summary><div class="thinking-body">${md(t)}</div></details>`;
  }
  function turnMeta(m) {
    const bits = [];
    if (m.modelActual) bits.push(esc(m.modelActual));
    if (m.usage) bits.push(`${fmtTok(m.usage.inputTokens)}↑ ${fmtTok(m.usage.outputTokens)}↓`);
    if (m.usage && m.usage.cacheRead) bits.push(`${fmtTok(m.usage.cacheRead)} 缓存`);
    if (m.stopReason && m.stopReason !== 'end_turn' && m.stopReason !== 'tool_use') bits.push(esc(m.stopReason));
    return bits.length ? `<div class="turn-meta">${bits.map((b) => '<span>' + b + '</span>').join('')}</div>` : '';
  }

  function toolResultText(b) {
    const c = b && b.content;
    if (typeof c === 'string') return c;
    if (Array.isArray(c)) return c.map((x) => (x && x.type === 'text' ? x.text : (x && x.text) || JSON.stringify(x))).join('\n');
    return c == null ? '' : JSON.stringify(c);
  }
  function diff(oldS, newS) {
    const o = String(oldS || '').split('\n');
    const n = String(newS || '').split('\n');
    return '<div class="diff">' + o.map((l) => `<div class="d-del">- ${esc(l)}</div>`).join('') + n.map((l) => `<div class="d-add">+ ${esc(l)}</div>`).join('') + '</div>';
  }
  function todos(list) {
    return '<div class="todos">' + (list || []).map((t) => {
      const m = t.status === 'completed' ? '☑' : t.status === 'in_progress' ? '◐' : '☐';
      return `<div class="todo ${esc(t.status || '')}"><span class="todo-box">${m}</span>${esc(t.content || t.activeForm || '')}</div>`;
    }).join('') + '</div>';
  }
  function codePre(text, lang) { return `<pre><code${lang ? ' class="language-' + esc(lang) + '"' : ''}>${esc(truncate(text, 12000))}</code></pre>`; }

  function renderToolCard(tu, resBlock) {
    const name = tu.name || 'tool';
    const input = (tu.input && typeof tu.input === 'object') ? tu.input : {};
    let icon = '🔧', head = name, bodyInput = '';
    if (name === 'Bash') { icon = '⌘'; head = 'Bash'; bodyInput = `<pre class="cmd">$ ${esc(input.command || '')}</pre>` + (input.description ? `<div class="muted small">${esc(input.description)}</div>` : ''); }
    else if (name === 'Read') { icon = '📖'; head = 'Read ' + (input.file_path || ''); }
    else if (name === 'Edit') { icon = '✏️'; head = 'Edit ' + (input.file_path || ''); bodyInput = diff(input.old_string, input.new_string); }
    else if (name === 'MultiEdit') { icon = '✏️'; head = 'MultiEdit ' + (input.file_path || ''); bodyInput = Array.isArray(input.edits) && input.edits.length ? input.edits.map((e) => diff(e.old_string, e.new_string)).join('') : '<div class="muted small">（无编辑内容）</div>'; }
    else if (name === 'Write') { icon = '📝'; head = 'Write ' + (input.file_path || ''); bodyInput = codePre(input.content || ''); }
    else if (name === 'Grep') { icon = '🔎'; head = 'Grep ' + (input.pattern || ''); if (input.path) bodyInput = `<div class="muted small">in ${esc(input.path)}</div>`; }
    else if (name === 'Glob') { icon = '🔎'; head = 'Glob ' + (input.pattern || ''); }
    else if (name === 'TodoWrite') { icon = '✅'; head = 'Todos'; bodyInput = todos(input.todos); }
    else if (name === 'Task') { icon = '🤖'; head = 'Task → ' + (input.subagent_type || 'agent'); bodyInput = (input.description ? `<div class="muted small">${esc(input.description)}</div>` : '') + (input.prompt ? `<pre class="wrap">${esc(truncate(input.prompt, 4000))}</pre>` : ''); }
    else if (name === 'WebSearch') { icon = '🌐'; head = 'WebSearch ' + (input.query || ''); }
    else if (name === 'WebFetch') { icon = '🌐'; head = 'WebFetch ' + (input.url || ''); }
    else if (/^mcp__/.test(name)) { icon = '🧩'; head = 'MCP · ' + name.replace(/^mcp__/, ''); bodyInput = Object.keys(input).length ? `<pre class="wrap">${esc(JSON.stringify(input, null, 2))}</pre>` : ''; }
    else { bodyInput = Object.keys(input).length ? `<pre class="wrap">${esc(JSON.stringify(input, null, 2))}</pre>` : ''; }

    let resHtml;
    if (resBlock) {
      const isErr = !!resBlock.is_error;
      const txt = toolResultText(resBlock);
      resHtml = `<details class="tool-result ${isErr ? 'err' : ''}"${isErr ? ' open' : ''}><summary>${isErr ? '✗ 错误结果' : '✓ 结果'}</summary><pre class="wrap">${esc(truncate(txt, 8000))}</pre></details>`;
    } else {
      resHtml = '<div class="tool-pending">— 无结果记录</div>';
    }
    return `<div class="tool-card"><div class="tool-head"><span class="tool-icon">${icon}</span><span class="tool-name">${esc(head)}</span></div>${bodyInput ? `<div class="tool-input">${bodyInput}</div>` : ''}${resHtml}</div>`;
  }

  function renderSidePanels(detail) {
    const m = detail.meta || {};
    const t = m.totals || {};
    const rows = [
      ['标题', m.title],
      ['模型', m.model],
      ...(m.isSubagent ? [['类型', '子代理会话']] : []),
      ['项目', m.cwd ? projName(m.cwd) : m.project],
      ['分支', m.gitBranch],
      ['会话', m.sessionId ? String(m.sessionId).slice(0, 8) : null],
      ['消息', m.messages],
      ['轮次', t.turns],
      ['输入', t.in != null ? fmtTok(t.in) : null],
      ['输出', t.out != null ? fmtTok(t.out) : null],
      ['缓存读', t.cacheRead ? fmtTok(t.cacheRead) : null],
      ['版本', m.version],
    ].filter((r) => r[1] != null && r[1] !== '');
    $('convStats').innerHTML = rows.map((r) => `<div class="stat-row"><span class="k">${esc(r[0])}</span><span class="v" title="${esc(r[1])}">${esc(r[1])}</span></div>`).join('');

    const host = $('convDetail');
    const msgs = host.querySelectorAll('.msg');
    const toc = [];
    msgs.forEach((el, i) => {
      el.id = 'm' + i;
      const role = el.classList.contains('user') ? '👤' : '✦';
      const tools = el.querySelectorAll('.tool-name');
      const label = tools.length ? Array.from(tools).slice(0, 2).map((t2) => t2.textContent.split(' ')[0]).join(', ') : (el.querySelector('.blk-text') ? el.querySelector('.blk-text').textContent.trim().slice(0, 30) : '');
      toc.push(`<div class="toc-item" data-go="m${i}" title="${esc(label || '')}">${role} ${esc(label || '…')}</div>`);
    });
    $('convToc').innerHTML = toc.join('');
  }

  /* ---------- events ---------- */
  function bind() {
    const list = $('convList');
    if (list) list.addEventListener('click', (e) => {
      const head = e.target.closest('.conv-proj-head');
      if (head) {
        const key = head.dataset.proj;
        if (collapsed.has(key)) collapsed.delete(key); else collapsed.add(key);
        persistCollapsed();
        renderList();
        return;
      }
      const item = e.target.closest('.conv-item');
      if (item) openConversation(item.dataset.id, item.dataset.file);
    });
    const sb = $('convSearch');
    if (sb) sb.addEventListener('input', (e) => { search = e.target.value.trim(); renderList(); });
    const clr = $('convClear');
    if (clr) clr.addEventListener('click', () => { const i = $('convSearch'); if (i) { i.value = ''; search = ''; renderList(); i.focus(); } });
    const toc = $('convToc');
    if (toc) toc.addEventListener('click', (e) => { const it = e.target.closest('.toc-item'); if (it) { const t = document.getElementById(it.dataset.go); if (t) t.scrollIntoView({ behavior: 'smooth', block: 'start' }); } });

    // Collapse the conversation list sidebar / nav panel
    const convSidebar = document.querySelector('.conv-sidebar');
    const I = window.ClawdyIcons || {};
    const setChevron = (btn, isCol) => {
      const icon = btn && btn.querySelector('[data-icon]');
      if (icon) icon.innerHTML = isCol ? (I.chevronRight || '›') : (I.chevronLeft || '‹');
    };

    const collapseListBtn = $('btnCollapseConvList');
    if (collapseListBtn && convSidebar) {
      try { if (localStorage.getItem('clawdy-convlist-collapsed') === '1') { convSidebar.classList.add('collapsed'); setChevron(collapseListBtn, true); } } catch (_) {}
      collapseListBtn.addEventListener('click', (e) => {
        e.stopPropagation();
        const isCol = convSidebar.classList.toggle('collapsed');
        setChevron(collapseListBtn, isCol);
        try { localStorage.setItem('clawdy-convlist-collapsed', isCol ? '1' : '0'); } catch (_) {}
      });
    }

    const convNav = document.querySelector('.conv-nav');
    const collapseNavBtn = $('btnCollapseConvNav');
    if (collapseNavBtn && convNav) {
      try { if (localStorage.getItem('clawdy-convnav-collapsed') === '1') { convNav.classList.add('collapsed'); setChevron(collapseNavBtn, true); } } catch (_) {}
      collapseNavBtn.addEventListener('click', (e) => {
        e.stopPropagation();
        const isCol = convNav.classList.toggle('collapsed');
        setChevron(collapseNavBtn, isCol);
        try { localStorage.setItem('clawdy-convnav-collapsed', isCol ? '1' : '0'); } catch (_) {}
      });
    }

    // Detail message search
    const dsearch = $('convDetailSearch');
    if (dsearch) {
      let t;
      dsearch.addEventListener('input', () => { clearTimeout(t); t = setTimeout(() => performDetailSearch(dsearch.value.trim()), 100); });
      dsearch.addEventListener('keydown', (e) => {
        if (e.key === 'Enter') { e.preventDefault(); if (detailSearchHighlights.length) gotoDetailSearchMatch(detailSearchIndex + 1); }
        if (e.key === 'Escape') { dsearch.value = ''; clearDetailSearchHighlights(); }
      });
    }
    const dprev = $('convDetailSearchPrev');
    if (dprev) dprev.addEventListener('click', () => { if (detailSearchHighlights.length) gotoDetailSearchMatch(detailSearchIndex - 1); });
    const dnext = $('convDetailSearchNext');
    if (dnext) dnext.addEventListener('click', () => { if (detailSearchHighlights.length) gotoDetailSearchMatch(detailSearchIndex + 1); });
    const dclear = $('convDetailSearchClear');
    if (dclear) dclear.addEventListener('click', () => { const inp = $('convDetailSearch'); if (inp) inp.value = ''; clearDetailSearchHighlights(); });

    // Live follow: ~/.claude/projects changed → refresh list, re-render open session if touched.
    if (api.onHistoryChanged) api.onHistoryChanged((p) => {
      clearTimeout(listTimer);
      listTimer = setTimeout(refresh, 200);
      if (openFile && p && p.files && p.files.indexOf(openFile) !== -1) rerenderDetail(false);
    });
  }

  window.ClawdyConversations = {
    onShow() { refresh(); if (openFile) rerenderDetail(false); },
  };

  bind();
})();
