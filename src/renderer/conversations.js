'use strict';

/* "对话" view — reads Claude Code's on-disk session history (~/.claude/projects) directly
   and renders it claude-code-history-viewer style: projects → sessions tree, a rich message
   timeline (text / thinking / per-tool cards + results / diffs / code / images), live-follow
   for active sessions, per-session stats, and in-conversation search. */
(function () {
  const api = window.clawdy;
  if (!api) return;
  const $ = (id) => document.getElementById(id);
  const L = (k, p) => (window.I18n ? window.I18n.t(k, p) : k); // translate (t/$ already taken)
  const localeTag = () => (window.I18n ? window.I18n.localeTag : 'en-US');

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
  function truncate(s, n) { s = String(s == null ? '' : s); return s.length > n ? s.slice(0, n) + L('conv.charsMore', { n: s.length - n }) : s; }
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
    if (d < 60000) return L('time.justNow');
    if (d < 3600000) return L('time.minutesAgo', { n: Math.floor(d / 60000) });
    if (d < 86400000) return L('time.hoursAgo', { n: Math.floor(d / 3600000) });
    if (d < 7 * 86400000) return L('time.daysAgo', { n: Math.floor(d / 86400000) });
    return new Date(ts).toLocaleDateString(localeTag());
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
    renderDirSwitch();
    renderList();
  }

  async function renderDirSwitch() {
    const host = $('convDirSwitch');
    if (!host || !api.historyDirs) return;
    let data; try { data = await api.historyDirs(); } catch (_) { data = { dirs: [], active: 'all' }; }
    const dirs = data.dirs || [];
    if (dirs.length <= 1) { host.classList.add('hidden'); host.innerHTML = ''; return; }
    const active = data.active || 'all';
    const opts = [{ id: 'all', label: L('conv.all') }].concat(dirs.map((d) => ({ id: d.id, label: d.label, sessions: d.sessions })));
    host.classList.remove('hidden');
    host.innerHTML = opts.map((o) => `<button class="dir-chip inline-flex items-center gap-1.25 border border-border-custom bg-transparent text-muted font-medium text-[11.5px] px-2.5 py-1 rounded-full cursor-pointer transition-all duration-150 hover:text-fg hover:bg-chip-bg ${o.id === active ? 'active' : ''}" data-dir="${esc(o.id)}" title="${esc(o.label)}">${esc(o.label)}${o.sessions != null ? ' <span class="dir-chip-n text-[10px] px-1.25 py-0 rounded-full bg-black/12">' + o.sessions + '</span>' : ''}</button>`).join('');
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
      el.innerHTML = `<div class="state-inline py-6 px-3 text-center text-[11.5px] text-caption" style="padding:24px 12px">${search ? esc(L('conv.noMatch')) : esc(L('conv.noLocal')) + '<br><span class="text-muted text-[11px]">~/.claude/projects</span>'}</div>`;
      return;
    }
    el.innerHTML = list.map((p) => {
      const isCol = collapsed.has(p.cwd || p.name) && !search;
      const items = isCol ? '' : `<div class="conv-proj-sessions">${p.sessions.map(sessionItem).join('')}</div>`;
      return `<div class="conv-proj border-b border-border-custom">
        <div class="conv-proj-head flex items-center gap-1.5 px-3 py-2 cursor-pointer sticky top-0 z-10 bg-bg-sidebar/90 backdrop-blur-md select-none hover:bg-chip-bg transition-colors duration-150" data-proj="${esc(p.cwd || p.name)}" title="${esc(p.cwd || '')}">
          <span class="conv-proj-caret text-[10px] text-caption w-2.5 shrink-0">${isCol ? '▸' : '▾'}</span>
          <span class="conv-proj-name text-[12.5px] font-bold text-fg tracking-tight truncate flex-1">${esc(p.name || L('conv.unknownProject'))}</span>
          <span class="conv-proj-count text-[10.5px] font-semibold text-muted bg-chip-bg px-1.75 py-0.25 rounded-full shrink-0">${p.sessions.length}</span>
        </div>${items}
      </div>`;
    }).join('');
  }

  function sessionItem(c) {
    const live = isLive(c.lastActivity) ? '<span class="conv-live w-1.25 h-1.25 rounded-full bg-green animate-[pulse_1.6s_infinite] shrink-0"></span>' : '';
    const sub = c.isSubagent ? `<span class="conv-badge text-[10.5px] px-1.5 py-0.25 rounded-full bg-chip-bg text-fg font-sans">${esc(L('conv.subagent'))}</span>` : '';
    const model = c.model ? `<span class="conv-model text-brand">${esc(c.model)}</span>` : '';
    return `<div class="conv-item cursor-pointer flex flex-col gap-0.75 py-2.5 pr-3 pl-[22px] transition-colors duration-150 hover:bg-chip-bg border-0 ${c.id === openId ? 'active' : ''}" data-id="${esc(c.id)}" data-file="${esc(c.file || '')}">
      <div class="conv-item-top flex items-center gap-1.25">${live}<span class="conv-title text-[13.5px] font-semibold truncate">${esc(c.title || L('conv.untitled'))}</span></div>
      <div class="conv-item-sub flex items-center gap-1.5 text-[11.5px] text-caption font-mono truncate">${model}${sub}</div>
      <div class="conv-item-meta flex items-center gap-1.5 text-[11px] text-caption"><span>${esc(relTime(c.lastActivity))}</span>${c.sizeKB ? '<span>' + c.sizeKB + ' KB</span>' : ''}</div>
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
    if (!detail) { host.innerHTML = `<div class="conv-empty">${esc(L('conv.notFound'))}</div>`; lastRender = { file: null, count: -1 }; return; }

    const messages = detail.messages || [];
    const contentLen = messages.reduce((acc, m) => {
      if (!m.content) return acc;
      if (typeof m.content === 'string') return acc + m.content.length;
      if (Array.isArray(m.content)) {
        return acc + m.content.reduce((sum, b) => sum + (b.text ? b.text.length : 0) + (b.thinking ? b.thinking.length : 0), 0);
      }
      return acc;
    }, 0);

    // Skip needless re-renders: on-disk turns are written whole, so a stable message count
    // and content length means nothing changed — preserves scroll + expanded thinking/result panels.
    if (!force && lastRender.file === openFile && lastRender.count === messages.length && lastRender.contentLen === contentLen && host.querySelector('.msg')) return;

    const searchActive = !!($('convDetailSearch') && $('convDetailSearch').value.trim());
    clearDetailSearchHighlights();
    const wasBottom = isNearBottom(host);
    host.innerHTML = renderDetail(detail);
    highlight(host);
    if (force || wasBottom) host.scrollTop = host.scrollHeight;
    renderSidePanels(detail);
    lastRender = { file: openFile, count: messages.length, contentLen };

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
      return `<div class="msg user flex flex-col gap-1.25 animate-[panelIn_0.18s_cubic-bezier(0.23,1,0.32,1)] max-w-[780px] w-full"><div class="msg-role text-[10px] font-bold uppercase tracking-wider text-caption flex items-center gap-1.25">👤 ${esc(L('conv.you'))}</div><div class="msg-body bg-bg-elev border border-border-custom rounded-[11px] p-3 px-4 shadow-card text-[13px] leading-[1.58]">${vis.map(renderUserBlock).join('')}</div></div>`;
    }
    let body = '';
    blocks.forEach((b) => {
      if (b.type === 'text') body += `<div class="blk-text">${md(b.text)}</div>`;
      else if (b.type === 'thinking') body += renderThinking(b);
      else if (b.type === 'tool_use') body += renderToolCard(b, results[b.id]);
      else if (b.type === 'image') body += renderUserBlock(b);
      else body += `<pre class="pre bg-[#0c0e12] border border-white/7 rounded-[7px] p-2.5 overflow-x-auto font-mono text-[11px] leading-[1.48] text-[#e8edf4] whitespace-pre-wrap break-all">${esc(JSON.stringify(b))}</pre>`;
    });
    if (!body) return '';
    return `<div class="msg assistant group flex flex-col gap-1.25 animate-[panelIn_0.18s_cubic-bezier(0.23,1,0.32,1)] max-w-[780px] w-full ${m.isSidechain ? 'sidechain' : ''}"><div class="msg-role text-[10px] font-bold uppercase tracking-wider text-caption flex items-center gap-1.25">✦ Claude${m.isSidechain ? ` <span class="conv-badge text-[10.5px] px-1.5 py-0.25 rounded-full bg-chip-bg text-fg font-sans">${esc(L('conv.subagent'))}</span>` : ''}</div><div class="msg-body text-[13px] leading-[1.58] py-0.5 pr-0 pl-3 border-l-2 border-border-strong group-[.streaming]:border-green">${body}${turnMeta(m)}</div></div>`;
  }
  function renderDetail(detail) {
    const messages = detail.messages || [];
    const results = buildResults(messages);
    const html = messages.map((m) => renderMessage(m, results)).join('');
    return html || `<div class="conv-empty">${esc(L('conv.emptyConv'))}</div>`;
  }

  function renderUserBlock(b) {
    if (b.type === 'image') {
      const s = b.source || {};
      if (s.data) return `<img class="msg-img max-w-[300px] rounded-lg border border-border-custom my-1" src="data:${esc(s.media_type || 'image/png')};base64,${s.data}" />`;
      return `<div class="img-redacted text-[11px] text-muted p-[7px] px-2.25 bg-chip-bg rounded-[6px] inline-block">🖼 ${esc(L('conv.image'))}</div>`;
    }
    return `<div class="blk-text">${md(b.text)}</div>`;
  }
  function renderThinking(b) {
    const t = b.thinking || '';
    const first = t.split('\n').find((x) => x.trim()) || L('conv.thinking');
    return `<details class="thinking bg-[#ff9f0a]/4 border border-[#ff9f0a]/12 rounded-[7px] my-1.5"><summary class="cursor-pointer p-1.75 px-2.5 text-[11px] font-medium text-amber outline-none list-none [&::-webkit-details-marker]:hidden">💭 ${esc(L('conv.thinking'))} · <span class="text-muted/70">${esc(first.slice(0, 60))}</span></summary><div class="thinking-body p-2.5 pt-1.75 pb-2 text-[11.5px] text-muted leading-[1.48] border-t border-[#ff9f0a]/8 mt-0.75">${md(t)}</div></details>`;
  }
  function turnMeta(m) {
    const bits = [];
    if (m.modelActual) bits.push(esc(m.modelActual));
    if (m.usage) bits.push(`${fmtTok(m.usage.inputTokens)}↑ ${fmtTok(m.usage.outputTokens)}↓`);
    if (m.usage && m.usage.cacheRead) bits.push(`${fmtTok(m.usage.cacheRead)} ${esc(L('conv.cache'))}`);
    if (m.stopReason && m.stopReason !== 'end_turn' && m.stopReason !== 'tool_use') bits.push(esc(m.stopReason));
    return bits.length ? `<div class="turn-meta flex gap-1 flex-wrap mt-1.5">${bits.map((b) => `<span class="text-[9.5px] font-mono text-caption bg-chip-bg rounded-[4px] px-1.25 py-0.25">${b}</span>`).join('')}</div>` : '';
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
    return '<div class="diff font-mono text-[10.5px] rounded-[5px] overflow-hidden border border-border-custom mt-0.75">' + o.map((l) => `<div class="d-del bg-red-soft text-red py-0.5 px-1.75 whitespace-pre-wrap">- ${esc(l)}</div>`).join('') + n.map((l) => `<div class="d-add bg-green-soft text-green py-0.5 px-1.75 whitespace-pre-wrap">+ ${esc(l)}</div>`).join('') + '</div>';
  }
  function todos(list) {
    return '<div class="todos flex flex-col gap-0.5 mt-0.75">' + (list || []).map((t) => {
      const m = t.status === 'completed' ? '☑' : t.status === 'in_progress' ? '◐' : '☐';
      return `<div class="todo text-[11.5px] flex gap-1.75 [&.completed]:text-muted [&.completed]:line-through [&.in_progress]:text-primary [&.in_progress]:font-semibold ${esc(t.status || '')}"><span class="todo-box w-3.25">${m}</span>${esc(t.content || t.activeForm || '')}</div>`;
    }).join('') + '</div>';
  }
  function codePre(text, lang) { return `<pre class="pre bg-[#0c0e12] border border-white/7 rounded-[7px] p-2.5 overflow-x-auto font-mono text-[11px] leading-[1.48] text-[#e8edf4]"><code${lang ? ' class="language-' + esc(lang) + '"' : ''}>${esc(truncate(text, 12000))}</code></pre>`; }

  function renderToolCard(tu, resBlock) {
    const name = tu.name || 'tool';
    const input = (tu.input && typeof tu.input === 'object') ? tu.input : {};
    let icon = '🔧', head = name, bodyInput = '';
    if (name === 'Bash') { icon = '⌘'; head = 'Bash'; bodyInput = `<pre class="pre bg-[#0c0e12] border border-white/7 rounded-[7px] p-2.5 overflow-x-auto font-mono text-[11px] leading-[1.48] text-green">$ ${esc(input.command || '')}</pre>` + (input.description ? `<div class="text-muted text-[11px]">${esc(input.description)}</div>` : ''); }
    else if (name === 'Read') { icon = '📖'; head = 'Read ' + (input.file_path || ''); }
    else if (name === 'Edit') { icon = '✏️'; head = 'Edit ' + (input.file_path || ''); bodyInput = diff(input.old_string, input.new_string); }
    else if (name === 'MultiEdit') { icon = '✏️'; head = 'MultiEdit ' + (input.file_path || ''); bodyInput = Array.isArray(input.edits) && input.edits.length ? input.edits.map((e) => diff(e.old_string, e.new_string)).join('') : `<div class="text-muted text-[11px]">${esc(L('conv.noEdits'))}</div>`; }
    else if (name === 'Write') { icon = '📝'; head = 'Write ' + (input.file_path || ''); bodyInput = codePre(input.content || ''); }
    else if (name === 'Grep') { icon = '🔎'; head = 'Grep ' + (input.pattern || ''); if (input.path) bodyInput = `<div class="text-muted text-[11px]">in ${esc(input.path)}</div>`; }
    else if (name === 'Glob') { icon = '🔎'; head = 'Glob ' + (input.pattern || ''); }
    else if (name === 'TodoWrite') { icon = '✅'; head = 'Todos'; bodyInput = todos(input.todos); }
    else if (name === 'Task') { icon = '🤖'; head = 'Task → ' + (input.subagent_type || 'agent'); bodyInput = (input.description ? `<div class="text-muted text-[11px]">${esc(input.description)}</div>` : '') + (input.prompt ? `<pre class="pre bg-[#0c0e12] border border-white/7 rounded-[7px] p-2.5 overflow-x-auto font-mono text-[11px] leading-[1.48] text-[#e8edf4] whitespace-pre-wrap break-all">${esc(truncate(input.prompt, 4000))}</pre>` : ''); }
    else if (name === 'WebSearch') { icon = '🌐'; head = 'WebSearch ' + (input.query || ''); }
    else if (name === 'WebFetch') { icon = '🌐'; head = 'WebFetch ' + (input.url || ''); }
    else if (/^mcp__/.test(name)) { icon = '🧩'; head = 'MCP · ' + name.replace(/^mcp__/, ''); bodyInput = Object.keys(input).length ? `<pre class="pre bg-[#0c0e12] border border-white/7 rounded-[7px] p-2.5 overflow-x-auto font-mono text-[11px] leading-[1.48] text-[#e8edf4] whitespace-pre-wrap break-all">${esc(JSON.stringify(input, null, 2))}</pre>` : ''; }
    else { bodyInput = Object.keys(input).length ? `<pre class="pre bg-[#0c0e12] border border-white/7 rounded-[7px] p-2.5 overflow-x-auto font-mono text-[11px] leading-[1.48] text-[#e8edf4] whitespace-pre-wrap break-all">${esc(JSON.stringify(input, null, 2))}</pre>` : ''; }

    let resHtml;
    if (resBlock) {
      const isErr = !!resBlock.is_error;
      const txt = toolResultText(resBlock);
      resHtml = `<details class="tool-result border-t border-border-custom ${isErr ? 'err' : ''}"${isErr ? ' open' : ''}><summary class="cursor-pointer py-1.25 px-2.5 text-[10.5px] font-semibold ${isErr ? 'text-red' : 'text-green'} outline-none list-none [&::-webkit-details-marker]:hidden">${isErr ? '✗ ' + esc(L('conv.errResult')) : '✓ ' + esc(L('conv.result'))}</summary><pre class="pre bg-[#0c0e12] border border-white/7 rounded-[7px] p-2.5 overflow-x-auto font-mono text-[11px] leading-[1.48] text-[#e8edf4] whitespace-pre-wrap break-all mx-2.5 mb-2">${esc(truncate(txt, 8000))}</pre></details>`;
    } else {
      resHtml = `<div class="tool-pending py-1.25 px-2.5 text-[10.5px] text-muted border-t border-border-custom">— ${esc(L('conv.noResult'))}</div>`;
    }
    return `<div class="tool-card border border-border-strong rounded-[8px] my-2 overflow-hidden bg-bg-elev shadow-card"><div class="tool-head flex items-center gap-1.75 py-1.75 px-2.5 bg-chip-bg border-b border-border-custom text-[11px] font-semibold text-fg"><span class="tool-icon text-[11px]">${icon}</span><span class="tool-name font-mono font-semibold">${esc(head)}</span></div>${bodyInput ? `<div class="tool-input p-2 px-2.5">${bodyInput}</div>` : ''}${resHtml}</div>`;
  }

  function renderSidePanels(detail) {
    const m = detail.meta || {};
    const t = m.totals || {};
    const rows = [
      [L('conv.stat.title'), m.title],
      [L('conv.stat.model'), m.model],
      ...(m.isSubagent ? [[L('conv.stat.type'), L('conv.subagentSession')]] : []),
      [L('conv.stat.project'), m.cwd ? projName(m.cwd) : m.project],
      [L('conv.stat.branch'), m.gitBranch],
      [L('conv.stat.session'), m.sessionId ? String(m.sessionId).slice(0, 8) : null],
      [L('conv.stat.messages'), m.messages],
      [L('conv.stat.turns'), t.turns],
      [L('conv.stat.input'), t.in != null ? fmtTok(t.in) : null],
      [L('conv.stat.output'), t.out != null ? fmtTok(t.out) : null],
      [L('conv.stat.cacheRead'), t.cacheRead ? fmtTok(t.cacheRead) : null],
      [L('conv.stat.version'), m.version],
    ].filter((r) => r[1] != null && r[1] !== '');
    $('convStats').innerHTML = rows.map((r) => `<div class="stat-row flex justify-between gap-2 text-xs py-1.25 border-b border-border-custom last:border-b-0"><span class="k text-caption">${esc(r[0])}</span><span class="v font-mono text-[11.5px] text-fg truncate max-w-[120px]" title="${esc(r[1])}">${esc(r[1])}</span></div>`).join('');

    const host = $('convDetail');
    const msgs = host.querySelectorAll('.msg');
    const toc = [];
    msgs.forEach((el, i) => {
      el.id = 'm' + i;
      const role = el.classList.contains('user') ? '👤' : '✦';
      const tools = el.querySelectorAll('.tool-name');
      const label = tools.length ? Array.from(tools).slice(0, 2).map((t2) => t2.textContent.split(' ')[0]).join(', ') : (el.querySelector('.blk-text') ? el.querySelector('.blk-text').textContent.trim().slice(0, 30) : '');
      toc.push(`<div class="toc-item text-xs text-caption py-1 px-1.75 rounded-[5px] cursor-pointer truncate transition-all duration-100 hover:bg-chip-bg hover:text-fg" data-go="m${i}" title="${esc(label || '')}">${role} ${esc(label || '…')}</div>`);
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
    const dirSwitch = $('convDirSwitch');
    if (dirSwitch) dirSwitch.addEventListener('click', async (e) => {
      const btn = e.target.closest('[data-dir]');
      if (!btn) return;
      try { if (api.historySetActive) await api.historySetActive(btn.dataset.dir); } catch (_) {}
      await refresh();
    });
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
    // Re-render everything this view owns when the UI language changes.
    setLang() { renderDirSwitch(); renderList(); if (openFile) rerenderDetail(true); },
  };

  bind();
})();
