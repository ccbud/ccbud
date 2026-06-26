'use strict';

/* "对话" view — reads Claude Code's on-disk session history (~/.claude/projects) directly
   and renders it claude-code-history-viewer style: projects → sessions tree, a rich message
   timeline (text / thinking / per-tool cards + results / diffs / code / images), live-follow
   for active sessions, per-session stats, and in-conversation search. */
(function () {
  const api = window.ccbud;
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
  let currentDetail = null; // last-loaded session detail (for export)
  // Which session occupies the main panel: 'main' (the root thread) or a subagent key (its tool_use
  // id in detail.subagents). Each subagent is an independent session, so it gets the WHOLE panel —
  // switched via the agent list in the right nav, not nested inline. Reset to 'main' on open.
  let activeAgent = 'main';
  // The message list currently shown in the main panel (main thread or the active subagent's).
  function activeMessages() {
    if (!currentDetail) return [];
    if (activeAgent !== 'main' && currentDetail.subagents && currentDetail.subagents[activeAgent]) {
      return currentDetail.subagents[activeAgent].messages || [];
    }
    return currentDetail.messages || [];
  }
  // Render only the most recent N messages of a thread; a "load earlier" control reveals more.
  // Huge threads (1000s of turns) otherwise put 1000s of nodes in the DOM, so every window
  // resize / live re-render walks the whole tree (~1s) — the measured root cause of the jank.
  // Windowed (virtualized) rendering: only a window [vStart, vEnd) of the thread is ever in the DOM.
  // Browsing extends it via load-earlier/later; search/TOC jump renders a fresh window around the
  // target. Keeps the DOM bounded (fast resize/scroll) and never renders thousands of messages at once.
  const DETAIL_WIN = 160;  // window size when opening / jumping (~115 rendered after skips — a lot of thread)
  const LOAD_MORE = 120;   // messages revealed per load-earlier / load-later click
  const MAX_WIN = 240;     // hard cap on rendered messages — load-more trims the far end past this. Keeps the
                           //  DOM small so collapse/resize/scroll stay cheap no matter how far you browse.
  let vStart = 0, vEnd = 0;      // rendered window into currentDetail.messages
  let detailTexts = null;        // per-message plain text, for data-driven search (built on open)

  try { collapsed = new Set(JSON.parse(localStorage.getItem('ccbud-collapsed-projects') || '[]')); } catch (_) {}
  function persistCollapsed() { try { localStorage.setItem('ccbud-collapsed-projects', JSON.stringify([...collapsed])); } catch (_) {} }

  // Detail search state (data-driven). searchOcc = every match occurrence across the whole thread,
  // found in the parsed message text (not the DOM): [{ mi: messageIndex, k: kth-match-in-that-message }].
  let searchOcc = [];        // indices of messages that contain ≥1 match (navigation steps through these)
  let searchIndex = -1;
  let searchQuery = '';
  let searchTotalOcc = 0;    // total match occurrences across the thread (shown after the message count)

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
  // Is the currently-open session still active (recent on-disk activity)? Used to drive the
  // safety-net auto-refresh so in-progress conversations live-update even if a watch event is missed.
  function openSessionLive() {
    if (!openId) return false;
    for (const p of projects) for (const s of p.sessions) if (s.id === openId) return isLive(s.lastActivity);
    return false;
  }

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

  const hasHighlightAPI = () => !!(window.CSS && CSS.highlights && typeof Highlight !== 'undefined');

  function clearDetailSearchHighlights() {
    if (hasHighlightAPI()) { CSS.highlights.delete('cd-search'); CSS.highlights.delete('cd-current'); }
    searchOcc = []; searchIndex = -1; searchQuery = '';
    const countEl = $('convDetailSearchCount');
    if (countEl) countEl.textContent = '';
  }
  function invalidateSearchNodes() { /* no-op: search is data-driven now (kept so callers stay valid) */ }

  // Per-message plain text, for DATA-driven search — we find matches in the parsed messages (fast, no
  // DOM), so search never has to render the whole thread. Built once when a conversation opens.
  function contentToText(c) {
    if (typeof c === 'string') return c;
    if (Array.isArray(c)) return c.map((x) => (x && (x.text != null ? x.text : (typeof x.content === 'string' ? x.content : ''))) || '').join(' ');
    return '';
  }
  // Mirror renderMessage's logic so detailTexts[i] is non-empty iff message i actually renders, and
  // holds the SAME searchable text (incl. tool results, which render inside the assistant's tool card —
  // not the user turn that carries them). This keeps search matches aligned with rendered messages.
  function messagePlainText(m, results) {
    const blocks = normContent(m.content);
    if (m.role === 'user') {
      const vis = blocks.filter((b) => b.type === 'text' || b.type === 'image');
      if (!vis.length) return '';
      const tv = vis.map((b) => b.text || '').join('');
      if (tv.includes('<system-reminder>') || tv.includes('<command-name>') || tv.includes('<local-command')) return '';
      return vis.map((b) => b.text || '').join('\n');
    }
    let s = '';
    for (const b of blocks) {
      if (b.type === 'text') s += (b.text || '') + '\n';
      else if (b.type === 'thinking') s += (b.thinking || '') + '\n';
      else if (b.type === 'tool_use') { s += (b.name || '') + ' ' + (b.input ? JSON.stringify(b.input) : '') + '\n'; const r = results && results[b.id]; if (r) s += contentToText(r.content) + '\n'; }
    }
    return s;
  }
  function buildDetailTexts() {
    const messages = activeMessages();
    const results = buildResults(messages);
    detailTexts = messages.map((m) => messagePlainText(m, results));
  }

  // Highlight every match inside the CURRENT window via the CSS Custom Highlight API (Range-based, zero
  // DOM mutation). The window is bounded, so this is tiny + fast. Re-run after each window paint.
  function refreshWindowHighlights() {
    if (!hasHighlightAPI()) return;
    const host = $('convDetail'); if (!host || !searchQuery) { if (hasHighlightAPI()) CSS.highlights.delete('cd-search'); return; }
    let re; try { re = new RegExp(escapeRegExp(searchQuery), 'gi'); } catch (_) { return; }
    const h = new Highlight();
    const w = document.createTreeWalker(host, NodeFilter.SHOW_TEXT, null); let node;
    while ((node = w.nextNode())) {
      const text = node.nodeValue; if (!text || !text.trim()) continue; re.lastIndex = 0; let m;
      while ((m = re.exec(text)) !== null) {
        try { const r = document.createRange(); r.setStart(node, m.index); r.setEnd(node, m.index + m[0].length); h.add(r); } catch (_) {}
        if (m[0].length === 0) re.lastIndex++;
      }
    }
    CSS.highlights.set('cd-search', h);
  }

  function performDetailSearch(query) {
    searchQuery = query || '';
    if (hasHighlightAPI()) { CSS.highlights.delete('cd-search'); CSS.highlights.delete('cd-current'); }
    searchOcc = []; searchIndex = -1; searchTotalOcc = 0;
    const c = $('convDetailSearchCount');
    if (!query) { if (c) c.textContent = ''; return; }
    if (!detailTexts) buildDetailTexts();
    let re; try { re = new RegExp(escapeRegExp(query), 'gi'); } catch (_) { return; }
    // Scan the parsed message texts (NOT the DOM) for matches across the whole thread. Navigate by
    // matching MESSAGE (robust — no need to align data-text offsets with rendered-DOM offsets); each
    // matching message lists once in searchOcc, and every match inside it is highlighted on arrival.
    for (let i = 0; i < detailTexts.length; i++) {
      const t = detailTexts[i]; if (!t) continue; re.lastIndex = 0; let m, has = false;
      while ((m = re.exec(t)) !== null) { searchTotalOcc++; has = true; if (m[0].length === 0) re.lastIndex++; }
      if (has) searchOcc.push(i);
    }
    if (!searchOcc.length) { if (c) c.textContent = '0/0'; return; }
    gotoDetailSearchMatch(0); // jump to the first matching message (renders its window on demand)
  }

  // Navigate to matching message #newIndex: bring it into the window, highlight every match in the
  // window, mark + centre the first match in the target message. Bounded — never renders the whole thread.
  function gotoDetailSearchMatch(newIndex) {
    const len = searchOcc.length; if (!len) return;
    searchIndex = ((newIndex % len) + len) % len;
    const mi = searchOcc[searchIndex];
    jumpToMessage(mi, 'center');
    refreshWindowHighlights();
    const host = $('convDetail');
    const el = host && host.querySelector(`[data-mi="${mi}"]`);
    if (el && hasHighlightAPI() && searchQuery) {
      let re; try { re = new RegExp(escapeRegExp(searchQuery), 'gi'); } catch (_) { re = null; }
      let curRange = null;
      if (re) {
        const w = document.createTreeWalker(el, NodeFilter.SHOW_TEXT, null); let node;
        while ((node = w.nextNode()) && !curRange) {
          const text = node.nodeValue; if (!text) continue; re.lastIndex = 0; const m = re.exec(text);
          if (m) { curRange = document.createRange(); curRange.setStart(node, m.index); curRange.setEnd(node, m.index + m[0].length); }
        }
      }
      if (curRange) {
        const cur = new Highlight(); cur.add(curRange); CSS.highlights.set('cd-current', cur);
        const rect = curRange.getBoundingClientRect(); const hr = host.getBoundingClientRect();
        if (rect && hr && rect.height) host.scrollTop += (rect.top - hr.top) - host.clientHeight / 2;
      }
    }
    const c = $('convDetailSearchCount');
    if (c) c.textContent = `${searchIndex + 1}/${len}` + (searchTotalOcc > len ? ` · ${searchTotalOcc}` : '');
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
    // Hide the synthetic 导入 chip until something is actually imported (keeps the bar clean / unchanged
    // for single-dir users). The + button is the import entry point regardless.
    const dirs = (data.dirs || []).filter((d) => !(d.imported && !d.sessions));
    if (dirs.length <= 1) { host.classList.add('hidden'); host.innerHTML = ''; return; }
    const active = data.active || 'all';
    const opts = [{ id: 'all', label: L('conv.all') }].concat(dirs.map((d) => ({ id: d.id, label: d.imported ? '📥 ' + d.label : d.label, sessions: d.sessions })));
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
    const imp = c.imported ? `<span class="conv-badge text-[10.5px] px-1.5 py-0.25 rounded-full bg-brand-soft text-brand font-sans">📥 ${esc(L('conv.imported'))}</span>` : '';
    // Imported copies live only in the app store, so they get a remove affordance (deletes the copy).
    const rm = c.imported ? `<button class="conv-remove-import ml-auto shrink-0 opacity-55 group-hover:opacity-100 text-caption hover:text-red hover:bg-chip-bg rounded text-[12px] leading-none w-[18px] h-[18px] flex items-center justify-center transition-all" data-remove-import="${esc(c.file || '')}" title="${esc(L('conv.removeImport'))}">✕</button>` : '';
    const model = c.model ? `<span class="conv-model text-brand">${esc(c.model)}</span>` : '';
    return `<div class="conv-item group cursor-pointer flex flex-col gap-0.75 py-2.5 pr-3 pl-[22px] transition-colors duration-150 hover:bg-chip-bg border-0 ${c.id === openId ? 'active' : ''}" data-id="${esc(c.id)}" data-file="${esc(c.file || '')}">
      <div class="conv-item-top flex items-center gap-1.25">${live}<span class="conv-title text-[13.5px] font-semibold truncate min-w-0">${esc(c.title || L('conv.untitled'))}</span>${rm}</div>
      <div class="conv-item-sub flex items-center gap-1.5 text-[11.5px] text-caption font-mono truncate">${model}${sub}${imp}</div>
      <div class="conv-item-meta flex items-center gap-1.5 text-[11px] text-caption"><span>${esc(relTime(c.lastActivity))}</span>${c.sizeKB ? '<span>' + c.sizeKB + ' KB</span>' : ''}</div>
    </div>`;
  }

  /* ---------- detail ---------- */
  async function openConversation(id, file) {
    const ds = $('convDetailSearch');
    if (ds) ds.value = '';
    clearDetailSearchHighlights();
    openId = id; openFile = file || null;
    activeAgent = 'main'; // new conversation always opens on its main thread
    detailTexts = null; vStart = 0; vEnd = 0; // reset the render window for the new conversation
    lastRender = { file: null, count: -1 };
    const eb = $('convExportBtn'); if (eb) eb.disabled = !openFile;
    const cp = $('convCopyPathBtn'); if (cp) cp.disabled = !openFile;
    const rp = $('convReplayBtn'); if (rp) rp.disabled = !openFile;
    const mb = $('convMoreBtn'); if (mb) mb.disabled = !openFile;
    renderList();
    // Big sessions take a beat to read+parse off disk — show a loading hint during the async fetch
    // (this wait is genuinely async/IPC, so the hint paints; the later render is what's kept bounded).
    const host = $('convDetail');
    if (host && openFile) host.innerHTML = `<div class="conv-empty">${esc(L('conv.loading'))}</div>`;
    await rerenderDetail(true);
  }

  async function rerenderDetail(force) {
    if (!openFile) return;
    let detail = null;
    try { detail = await api.historyGet(openFile); } catch (_) {}
    const host = $('convDetail');
    if (!host) return;
    if (!detail) { host.innerHTML = `<div class="conv-empty">${esc(L('conv.notFound'))}</div>`; lastRender = { file: null, count: -1 }; return; }
    currentDetail = detail;
    subIndex = null; // call-site map is rebuilt lazily against the freshly-loaded subagents

    const messages = detail.messages || [];
    const msgLen = (m) => {
      if (!m || !m.content) return 0;
      if (typeof m.content === 'string') return m.content.length;
      if (Array.isArray(m.content)) return m.content.reduce((sum, b) => sum + (b.text ? b.text.length : 0) + (b.thinking ? b.thinking.length : 0), 0);
      return 0;
    };
    let contentLen = messages.reduce((acc, m) => acc + msgLen(m), 0);
    // Fold subagent growth into the change key too: while a subagent streams, the main thread can
    // sit idle, and the skip-guard below would otherwise freeze the nested subagent view mid-run.
    const subs = detail.subagents || {};
    let subCount = 0;
    for (const k of Object.keys(subs)) {
      const sm = (subs[k] && subs[k].messages) || [];
      subCount += sm.length;
      contentLen += sm.reduce((acc, m) => acc + msgLen(m), 0);
    }

    // Skip needless re-renders: on-disk turns are written whole, so a stable message count
    // and content length means nothing changed — preserves scroll + expanded thinking/result panels.
    if (!force && lastRender.file === openFile && lastRender.count === messages.length && lastRender.contentLen === contentLen && lastRender.subCount === subCount && host.querySelector('.msg')) return;

    const total = activeMessages().length; // window/paint follow the ACTIVE session (main or subagent)
    const wasBottom = isNearBottom(host);
    buildDetailTexts();
    if (force) {
      // open at the newest turns (trailing window), scrolled to the bottom
      clearDetailSearchHighlights();
      vEnd = total; vStart = Math.max(0, total - DETAIL_WIN);
      paintWindow();
      host.scrollTop = host.scrollHeight;
    } else if (wasBottom) {
      // live-follow at the bottom: extend the window to the newest and stay pinned to the bottom
      vEnd = total; vStart = Math.max(0, total - DETAIL_WIN);
      paintWindow();
      host.scrollTop = host.scrollHeight;
    } else {
      // scrolled up reading history: don't repaint (preserves scroll + expanded panels); new turns are
      // appended past the window and surface via the "load later" affordance / next jump.
      vEnd = Math.min(vEnd, total);
    }
    renderSidePanels(detail);
    renderAgentTabs(detail);
    lastRender = { file: openFile, count: messages.length, contentLen, subCount };
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
  // inSub: rendered inside a nested subagent block — suppress the per-turn "subagent" badge
  // (the surrounding block already labels it) so the nested thread stays clean.
  function renderMessage(m, results, idx, inSub) {
    const mid = idx == null ? '' : ` id="m${idx}" data-mi="${idx}"`;
    const blocks = normContent(m.content);
    if (m.role === 'user') {
      const vis = blocks.filter((b) => b.type === 'text' || b.type === 'image');
      if (!vis.length) return '';
      const textVal = vis.map((b) => b.text || '').join('');
      if (textVal.includes('<system-reminder>') || textVal.includes('<command-name>') || textVal.includes('<local-command')) return '';
      return `<div class="msg user flex flex-col gap-1.25 animate-[panelIn_0.18s_cubic-bezier(0.23,1,0.32,1)] w-full"${mid}><div class="msg-role text-[10px] font-bold uppercase tracking-wider text-caption flex items-center gap-1.25">👤 ${esc(L('conv.you'))}</div><div class="msg-body bg-bg-elev border border-border-custom rounded-[11px] p-3 px-4 shadow-card text-[13px] leading-[1.58]">${vis.map(renderUserBlock).join('')}</div></div>`;
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
    return `<div class="msg assistant group flex flex-col gap-1.25 animate-[panelIn_0.18s_cubic-bezier(0.23,1,0.32,1)] w-full ${m.isSidechain ? 'sidechain' : ''}"${mid}><div class="msg-role text-[10px] font-bold uppercase tracking-wider text-caption flex items-center gap-1.25">✦ Claude${m.isSidechain && !inSub ? ` <span class="conv-badge text-[10.5px] px-1.5 py-0.25 rounded-full bg-chip-bg text-fg font-sans">${esc(L('conv.subagent'))}</span>` : ''}</div><div class="msg-body text-[13px] leading-[1.58] py-0.5 pr-0 pl-3 border-l-2 border-border-strong group-[.streaming]:border-green">${body}${turnMeta(m)}</div></div>`;
  }
  function winBtn(dir, n) {
    const lbl = esc(L('conv.loadEarlier', { n }));
    return `<button type="button" data-load-${dir} class="conv-load-earlier block mx-auto my-2 px-3.5 py-1.5 rounded-full bg-chip-bg text-muted text-[12px] font-medium cursor-pointer border border-border-custom hover:text-fg hover:bg-bg-elev transition-colors" title="${lbl}">${dir === 'earlier' ? '↑ ' : '↓ '}${lbl}</button>`;
  }
  // HTML for the current window [vStart,vEnd) into currentDetail.messages, plus load-earlier/later buttons.
  function renderWindow() {
    const messages = activeMessages();
    const total = messages.length;
    if (!total) return `<div class="conv-empty">${esc(L('conv.emptyConv'))}</div>`;
    const results = buildResults(messages); // scan ALL so tool_use cards resolve their result even if out of window
    const inSub = activeAgent !== 'main'; // in a subagent view the whole panel is that agent — drop per-turn badge
    let html = vStart > 0 ? winBtn('earlier', vStart) : '';
    for (let i = vStart; i < vEnd; i++) html += renderMessage(messages[i], results, i, inSub);
    if (vEnd < total) html += winBtn('later', total - vEnd);
    return html || `<div class="conv-empty">${esc(L('conv.emptyConv'))}</div>`;
  }
  function paintWindow() {
    const host = $('convDetail'); if (!host) return;
    host.innerHTML = renderWindow();
    highlight(host);
    refreshWindowHighlights(); // re-paint search highlights for the new window (no-op if not searching)
  }
  // The first message whose bottom is below the viewport top, with its offset within the viewport —
  // used to keep the view fixed across a repaint even when content is both added AND trimmed.
  function visibleAnchor() {
    const host = $('convDetail'); if (!host) return null;
    const hr = host.getBoundingClientRect();
    const els = host.querySelectorAll('[data-mi]');
    for (const el of els) { const r = el.getBoundingClientRect(); if (r.bottom > hr.top + 2) return { mi: +el.dataset.mi, off: r.top - hr.top }; }
    return null;
  }
  function anchoredPaint(a) {
    paintWindow();
    const host = $('convDetail');
    const el = a && host && host.querySelector(`[data-mi="${a.mi}"]`);
    if (el) host.scrollTop += (el.getBoundingClientRect().top - host.getBoundingClientRect().top) - a.off;
  }
  // Extend the window upward / downward; trim the far end past MAX_WIN so the DOM stays bounded.
  // Anchored on a currently-visible message so the viewport doesn't jump despite add+trim.
  function loadEarlier() {
    const host = $('convDetail'); if (!host || vStart <= 0) return;
    const a = visibleAnchor();
    vStart = Math.max(0, vStart - LOAD_MORE);
    if (vEnd - vStart > MAX_WIN) vEnd = vStart + MAX_WIN; // trim the (off-screen) bottom
    anchoredPaint(a);
  }
  function loadLater() {
    const host = $('convDetail'); if (!host) return;
    const total = activeMessages().length;
    if (vEnd >= total) return;
    const a = visibleAnchor();
    vEnd = Math.min(total, vEnd + LOAD_MORE);
    if (vEnd - vStart > MAX_WIN) vStart = vEnd - MAX_WIN; // trim the (off-screen) top
    anchoredPaint(a);
  }
  // Render a fresh window centred on message `mi` and bring it into view.
  function jumpToMessage(mi, block) {
    const total = activeMessages().length;
    if (!total) return null;
    mi = Math.max(0, Math.min(total - 1, mi));
    if (mi < vStart || mi >= vEnd || vEnd - vStart > DETAIL_WIN * 2) {
      vStart = Math.max(0, mi - Math.floor(DETAIL_WIN / 2));
      vEnd = Math.min(total, vStart + DETAIL_WIN);
      paintWindow();
    }
    const host = $('convDetail');
    const el = host && host.querySelector(`[data-mi="${mi}"]`);
    if (el) el.scrollIntoView({ block: block || 'center' });
    return el;
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
    // Some turns carry a thinking block with only a signature and no visible text (the model/upstream
    // returned encrypted/empty reasoning). Skip it rather than draw an empty collapsible.
    if (!t.trim()) return '';
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

  function shortPath(p) { if (!p) return ''; const s = String(p).split('/'); return s.length > 3 ? '…/' + s.slice(-2).join('/') : p; }
  function resultSummary(txt) { const b = txt ? txt.length : 0; if (!b) return ''; return b < 1024 ? b + ' B' : (b / 1024).toFixed(1) + ' KB'; }
  const PRE = 'pre bg-[#0c0e12] border border-white/7 rounded-[7px] p-2.5 overflow-x-auto font-mono text-[11px] leading-[1.48] text-[#e8edf4] whitespace-pre-wrap break-all';
  const TOOL_CLS = { Bash: 'exec', Read: 'read', Edit: 'write', MultiEdit: 'write', Write: 'write', Grep: 'search', Glob: 'search', TodoWrite: 'todo', Task: 'task', WebSearch: 'net', WebFetch: 'net' };
  function renderToolCard(tu, resBlock) {
    const name = tu.name || 'tool';
    const input = (tu.input && typeof tu.input === 'object') ? tu.input : {};
    const cls = /^mcp__/.test(name) ? 'mcp' : (TOOL_CLS[name] || 'default');
    let icon = '🔧', label = name, target = '', bodyInput = '';
    if (name === 'Bash') { icon = '⌘'; label = 'Bash'; target = input.description || ''; bodyInput = `<pre class="pre bg-[#0c0e12] border border-white/7 rounded-[7px] p-2.5 overflow-x-auto font-mono text-[11px] leading-[1.48] text-green">$ ${esc(input.command || '')}</pre>`; }
    else if (name === 'Read') { icon = '📖'; label = 'Read'; target = shortPath(input.file_path); }
    else if (name === 'Edit') { icon = '✏️'; label = 'Edit'; target = shortPath(input.file_path); bodyInput = diff(input.old_string, input.new_string); }
    else if (name === 'MultiEdit') { icon = '✏️'; label = 'MultiEdit'; target = shortPath(input.file_path); bodyInput = Array.isArray(input.edits) && input.edits.length ? input.edits.map((e) => diff(e.old_string, e.new_string)).join('') : `<div class="text-muted text-[11px]">${esc(L('conv.noEdits'))}</div>`; }
    else if (name === 'Write') { icon = '📝'; label = 'Write'; target = shortPath(input.file_path); bodyInput = codePre(input.content || ''); }
    else if (name === 'Grep') { icon = '🔎'; label = 'Grep'; target = input.pattern || ''; if (input.path) bodyInput = `<div class="text-muted text-[11px]">in ${esc(input.path)}</div>`; }
    else if (name === 'Glob') { icon = '🔎'; label = 'Glob'; target = input.pattern || ''; }
    else if (name === 'TodoWrite') { icon = '✅'; label = 'Todos'; bodyInput = todos(input.todos); }
    else if (name === 'Task') { icon = '🤖'; label = 'Task'; target = '→ ' + (input.subagent_type || 'agent'); bodyInput = (input.description ? `<div class="text-muted text-[11px] mb-1">${esc(input.description)}</div>` : '') + (input.prompt ? `<pre class="${PRE}">${esc(truncate(input.prompt, 4000))}</pre>` : ''); }
    else if (name === 'WebSearch') { icon = '🌐'; label = 'WebSearch'; target = input.query || ''; }
    else if (name === 'WebFetch') { icon = '🌐'; label = 'WebFetch'; target = input.url || ''; }
    else if (/^mcp__/.test(name)) { icon = '🧩'; label = 'MCP · ' + name.replace(/^mcp__/, ''); bodyInput = Object.keys(input).length ? `<pre class="${PRE}">${esc(JSON.stringify(input, null, 2))}</pre>` : ''; }
    else { bodyInput = Object.keys(input).length ? `<pre class="${PRE}">${esc(JSON.stringify(input, null, 2))}</pre>` : ''; }

    let resHtml;
    if (resBlock) {
      const isErr = !!resBlock.is_error;
      const txt = toolResultText(resBlock);
      const size = resultSummary(txt);
      resHtml = `<details class="tool-result border-t border-border-custom ${isErr ? 'err' : ''}"${isErr ? ' open' : ''}><summary class="cursor-pointer py-1.25 px-2.5 text-[10.5px] font-semibold ${isErr ? 'text-red' : 'text-green'} outline-none list-none [&::-webkit-details-marker]:hidden flex items-center gap-1.5"><span>${isErr ? '✗ ' + esc(L('conv.errResult')) : '✓ ' + esc(L('conv.result'))}</span>${size ? `<span class="tool-res-size">${esc(size)}</span>` : ''}</summary><pre class="pre bg-[#0c0e12] border border-white/7 rounded-[7px] p-2.5 overflow-x-auto font-mono text-[11px] leading-[1.48] text-[#e8edf4] whitespace-pre-wrap break-all mx-2.5 mb-2">${esc(truncate(txt, 8000))}</pre></details>`;
    } else {
      resHtml = `<div class="tool-pending py-1.25 px-2.5 text-[10.5px] text-muted border-t border-border-custom">— ${esc(L('conv.noResult'))}</div>`;
    }
    // If this call spawned a subagent (Task / Agent / Workflow / …, matched by tool_use id), nest its
    // transcript right under the call so it's read in the context that produced it. See inlineSubagentBlock.
    const subHtml = inlineSubagentBlock(tu.id);
    return `<div class="tool-card tool-${cls} border border-border-strong rounded-[8px] my-2 overflow-hidden bg-bg-elev shadow-card"><div class="tool-head flex items-center gap-1.75 py-1.75 px-2.5 bg-chip-bg border-b border-border-custom text-[11px] font-semibold text-fg"><span class="tool-icon text-[11px]">${icon}</span><span class="tool-name font-mono font-semibold shrink-0">${esc(label)}</span>${target ? `<span class="tool-target font-mono text-[10.5px] text-muted font-normal truncate min-w-0">${esc(target)}</span>` : ''}</div>${bodyInput ? `<div class="tool-input p-2 px-2.5">${bodyInput}</div>` : ''}${resHtml}${subHtml}</div>`;
  }

  // ---------- inline subagents (expand-at-call-site) ----------
  // A subagent dialogue is keyed by the tool_use id that spawned it (history.readSubagents). We render it
  // as a lazily-filled disclosure directly under that call — at any nesting depth, since a subagent's own
  // tool cards run through this same path. Body stays empty until opened (see fillSubBody) to bound the DOM.
  function inlineSubagentBlock(id) {
    const subs = (currentDetail && currentDetail.subagents) || {};
    const s = id && subs[id];
    if (!s) return '';
    const cnt = s.count != null ? s.count : ((s.messages || []).length);
    const out = (s.totals && s.totals.out) || 0;
    const meta = `${esc(L('conv.subagentMsgs', { n: cnt }))} · ${fmtTok(out)}↓`;
    const desc = s.description ? ` · <span class="font-normal text-muted">${esc(s.description)}</span>` : '';
    return `<details class="subagent-inline" data-sub="${esc(id)}"><summary class="cursor-pointer py-2 px-2.5 text-[11px] font-semibold text-brand outline-none list-none [&::-webkit-details-marker]:hidden flex items-center gap-1.5 bg-brand-soft hover:brightness-105"><span class="sub-caret shrink-0 transition-transform">▸</span><span class="shrink-0">🤖 ${esc(L('conv.subagent'))} · ${esc(s.type || 'agent')}</span><span class="truncate min-w-0 flex-1">${desc}</span><span class="text-caption font-mono font-normal shrink-0">${meta}</span></summary><div class="subagent-inline-body pl-3 pr-1 py-1.5 bg-brand-soft/10" data-sub-body="${esc(id)}"></div></details>`;
  }
  // Render one subagent's whole thread (recursively wiring its own inline subagents via renderMessage →
  // renderToolCard). idx=null so nested turns carry no data-mi (they're outside main-window navigation).
  function renderSubThread(key) {
    const s = currentDetail && currentDetail.subagents && currentDetail.subagents[key];
    if (!s) return '';
    const msgs = s.messages || [];
    if (!msgs.length) return `<div class="conv-empty text-[11px] text-muted py-1">${esc(L('conv.emptyConv'))}</div>`;
    const results = buildResults(msgs);
    return msgs.map((m) => renderMessage(m, results, null, true)).join('') || `<div class="conv-empty text-[11px] text-muted py-1">${esc(L('conv.emptyConv'))}</div>`;
  }
  // Fill a subagent disclosure's body on first open (no-op afterwards). Returns the body element.
  function fillSubBody(det) {
    const body = det && det.querySelector(':scope > [data-sub-body]');
    if (!body) return null;
    if (!body.dataset.filled) { body.innerHTML = renderSubThread(body.getAttribute('data-sub-body')); body.dataset.filled = '1'; highlight(body); }
    return body;
  }

  /* ---------- session tabs (top of the main panel) ---------- */
  // When a conversation spawned subagents, the panel header shows peer tabs: [主会话] [子代理 (N) ▾].
  // 主会话 and each subagent are equals — picking one moves the WHOLE panel to that session.
  let agentMenuOpen = false;
  function renderAgentTabs(detail) {
    const host = $('convAgentTabs');
    if (!host) return;
    const subs = (detail && detail.subagents) || {};
    const keys = Object.keys(subs);
    if (!keys.length) { host.innerHTML = ''; host.classList.add('hidden'); host.classList.remove('flex'); agentMenuOpen = false; return; }
    host.classList.remove('hidden'); host.classList.add('flex');
    const mainActive = activeAgent === 'main';
    const activeSub = !mainActive && subs[activeAgent] ? subs[activeAgent] : null;
    const seg = (active) => `inline-flex items-center gap-1.5 h-[28px] px-3 rounded-[8px] text-[12px] font-semibold cursor-pointer border transition-colors whitespace-nowrap ${active ? 'bg-brand-soft text-brand border-brand/25' : 'bg-bg-elev text-muted border-border-custom hover:text-fg hover:bg-chip-bg'}`;
    const mainTab = `<button type="button" data-agent="main" class="${seg(mainActive)}">👤 ${esc(L('conv.mainSession'))}</button>`;
    const ddLabel = activeSub ? `🤖 ${esc(activeSub.type || 'agent')}` : `🤖 ${esc(L('conv.stat.subagents'))} (${keys.length})`;
    const items = keys.map((k) => {
      const s = subs[k] || {};
      const out = (s.totals && s.totals.out) || 0;
      const cnt = s.count != null ? s.count : ((s.messages || []).length);
      const active = activeAgent === k;
      const desc = s.description ? `<div class="text-[10.5px] text-muted truncate pl-[18px]">${esc(s.description)}</div>` : '';
      return `<button type="button" data-agent="${esc(k)}" class="conv-agent-menu-item w-full flex flex-col gap-0.25 text-left px-2 py-1.5 rounded-[6px] cursor-pointer border border-transparent transition-colors ${active ? 'bg-brand-soft text-brand' : 'hover:bg-chip-bg text-fg'}">
        <div class="flex items-center gap-1.25 min-w-0"><span class="shrink-0 text-[11px]">🤖</span><span class="font-mono text-[11.5px] font-semibold truncate">${esc(s.type || 'agent')}</span></div>
        ${desc}
        <div class="text-[10px] text-caption font-mono pl-[18px]">${esc(L('conv.subagentMsgs', { n: cnt }))} · ${fmtTok(out)}↓</div>
      </button>`;
    }).join('');
    const menu = `<div class="conv-agent-menu ${agentMenuOpen ? '' : 'hidden'} absolute left-0 top-[34px] z-30 min-w-[240px] max-w-[320px] max-h-[60vh] overflow-y-auto bg-bg-elev border border-border-custom rounded-[9px] shadow-[0_10px_30px_rgba(0,0,0,0.24)] p-1 flex flex-col gap-0.5">${items}</div>`;
    const dd = `<div class="conv-agent-dd relative"><button type="button" data-agent-dd class="${seg(!!activeSub)}">${ddLabel}<span class="text-[8px] opacity-70 ml-0.5">▾</span></button>${menu}</div>`;
    host.innerHTML = mainTab + dd;
  }
  // Move the main panel to a different session (main thread or a subagent). Resets the render window
  // + search and repaints from the bottom, exactly like opening a fresh conversation.
  function switchAgent(key) {
    agentMenuOpen = false;
    if (key === activeAgent) { renderAgentTabs(currentDetail); return; }
    activeAgent = key;
    detailTexts = null; vStart = 0; vEnd = 0;
    clearDetailSearchHighlights();
    const ds = $('convDetailSearch'); if (ds) ds.value = '';
    buildDetailTexts();
    const total = activeMessages().length;
    vEnd = total; vStart = Math.max(0, total - DETAIL_WIN);
    paintWindow();
    const host = $('convDetail'); if (host) host.scrollTop = host.scrollHeight;
    renderAgentTabs(currentDetail);
    renderSidePanels(currentDetail);
  }

  // Map every subagent to where it was spawned: callSite.get(subKey) = { thread, mi } where thread is
  // 'main' or another subagent's key (nested spawns), and mi is the message index in that thread. Built
  // lazily per open session and reset when the session changes.
  let subIndex = null;
  function buildSubIndex() {
    const subs = (currentDetail && currentDetail.subagents) || {};
    const keys = new Set(Object.keys(subs));
    const callSite = new Map();
    const scan = (msgs, threadKey) => (msgs || []).forEach((m, i) => normContent(m.content).forEach((b) => {
      if (b.type === 'tool_use' && keys.has(b.id) && !callSite.has(b.id)) callSite.set(b.id, { thread: threadKey, mi: i });
    }));
    scan((currentDetail && currentDetail.messages) || [], 'main');
    for (const k of keys) scan(subs[k].messages, k);
    subIndex = { callSite };
  }
  // Ancestor chain from the outermost (spawned in main) down to `key`, e.g. [topSub, …, key]. Empty if
  // the call site can't be resolved (e.g. a subagent whose meta recorded no toolUseId).
  function subChain(key) {
    if (!subIndex) buildSubIndex();
    const chain = []; const seen = new Set(); let cur = key;
    while (cur && cur !== 'main' && !seen.has(cur)) {
      seen.add(cur); chain.unshift(cur);
      const cs = subIndex.callSite.get(cur);
      if (!cs) return []; // broken link — can't place it in context
      cur = cs.thread;
    }
    return chain;
  }
  // Bring a subagent into view AT ITS CALL SITE: jump the main thread to the outermost spawning turn,
  // then expand each disclosure down the chain (filling lazily) and scroll/flash the target. Falls back
  // to the standalone full-panel view when the call site is unknown, so orphan subagents stay reachable.
  function focusSubagent(key) {
    agentMenuOpen = false;
    const menu = document.querySelector('#convAgentTabs .conv-agent-menu');
    if (menu) menu.classList.add('hidden'); // close the picker immediately as click feedback
    if (!currentDetail || !(currentDetail.subagents || {})[key]) return;
    const chain = subChain(key);
    if (!chain.length) { switchAgent(key); return; } // call site unknown → standalone full-panel view
    if (activeAgent !== 'main') { activeAgent = 'main'; detailTexts = null; vStart = 0; vEnd = 0; buildDetailTexts(); }
    const top = subIndex.callSite.get(chain[0]); // { thread:'main', mi }
    jumpToMessage(top.mi, 'center');
    const host = $('convDetail');
    let det = null;
    if (host) for (const k of chain) {
      det = host.querySelector(`.subagent-inline[data-sub="${cssAttr(k)}"]`);
      if (!det) break;
      fillSubBody(det); det.open = true; // child level now exists in the DOM for the next iteration
    }
    renderAgentTabs(currentDetail);
    if (!det) { switchAgent(key); return; } // couldn't place it inline → don't leave the click doing nothing
    // Land on the spawning CALL (the tool card), not the middle of the now-tall subagent body, so the
    // "why did this subagent appear" context reads top-down. Flash the whole block so it's unmistakable.
    const anchor = det.closest('.tool-card') || det;
    anchor.scrollIntoView({ block: 'start' });
    if (host) host.scrollTop = Math.max(0, host.scrollTop - 48);
    det.classList.remove('sub-flash'); void det.offsetWidth; // restart the animation if re-focused
    det.classList.add('sub-flash');
    setTimeout(() => det.classList.remove('sub-flash'), 2200);
  }
  // Escape a tool_use id for use inside a [data-sub="…"] attribute selector (ids may contain ':').
  function cssAttr(s) { return String(s).replace(/(["\\])/g, '\\$1'); }

  function renderSidePanels(detail) {
    const m = detail.meta || {};
    const t = m.totals || {};
    const rows = [
      [L('conv.stat.title'), m.title],
      [L('conv.stat.model'), m.model],
      ...(m.isSubagent ? [[L('conv.stat.type'), L('conv.subagentSession')]] : []),
      ...(m.imported ? [[L('conv.imported'), m.importedFrom || '✓']] : []),
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

    // TOC is built from the message DATA (global indices) so it spans the WHOLE thread even though only
    // a window is rendered; clicking jumps the window to that message. Keyed on user turns — the natural
    // navigation points — which also keeps the sidebar light on huge threads.
    const messages = activeMessages(); // TOC follows the session shown in the main panel
    const toc = [];
    messages.forEach((m, i) => {
      if (m.role !== 'user') return;
      const vis = normContent(m.content).filter((b) => b.type === 'text');
      const tv = vis.map((b) => b.text || '').join(' ').trim();
      if (!tv || tv.includes('<system-reminder>') || tv.includes('<command-name>') || tv.includes('<local-command')) return;
      toc.push(`<div class="toc-item text-xs text-caption py-1 px-1.75 rounded-[5px] cursor-pointer truncate transition-all duration-100 hover:bg-chip-bg hover:text-fg" data-go="${i}" title="${esc(tv.slice(0, 90))}">👤 ${esc(tv.slice(0, 32) || '…')}</div>`);
    });
    $('convToc').innerHTML = toc.join('');
  }

  /* ---------- export ---------- */
  function toast(msg, ok) {
    let t = document.querySelector('.conv-toast');
    if (!t) { t = document.createElement('div'); t.className = 'conv-toast'; document.body.appendChild(t); }
    t.textContent = msg;
    t.classList.toggle('err', ok === false);
    t.classList.add('show');
    clearTimeout(t._t); t._t = setTimeout(() => t.classList.remove('show'), 2200);
  }
  function hideExportMenu() { const m = $('convExportMenu'); if (m) m.classList.add('hidden'); }

  // Absolute .jsonl path for the session currently in the panel — the active subagent's file when
  // one is selected, else the main session file. Used by the "copy path" button so a transcript can
  // be handed to another Claude Code session for replay / agent debugging.
  function currentJsonlPath() {
    if (activeAgent !== 'main' && currentDetail && currentDetail.subagents) {
      const s = currentDetail.subagents[activeAgent];
      if (s && s.file) return s.file;
    }
    return openFile;
  }

  function doCopyPath() {
    const p = currentJsonlPath();
    if (!p) return;
    try { api.copy(p); } catch (_) {}
    toast(L('conv.pathCopied'));
  }
  async function doReplay(btn) {
    const p = currentJsonlPath();
    if (!p || !api.desktopReplay) return;
    if (btn) btn.disabled = true;
    toast(L('conv.replayOpening'));
    let res;
    try { res = await api.desktopReplay(p); } catch (e) { res = { ok: false, reason: 'failed' }; }
    if (btn) btn.disabled = false;
    if (res && res.ok) return; // Claude Desktop now opening with the file + prompt
    const reason = res && res.reason;
    toast(
      reason === 'notInstalled' ? L('conv.replayNoApp')
        : reason === 'unsupported' ? L('conv.replayUnsupported')
        : reason === 'permission' ? L('conv.replayPermission')
        : reason === 'cancelled' ? L('conv.replayOpening')
        : L('conv.replayFail'),
      false
    );
  }
  // Collapse the action buttons into a "⋯" menu when the toolbar is too narrow to fit them
  // alongside a 200px-min search box.
  function updateToolbarLayout() {
    const tb = document.querySelector('.conv-detail-toolbar');
    const actions = $('convActions');
    const moreWrap = $('convMoreWrap');
    if (!tb || !actions || !moreWrap) return;
    actions.classList.remove('hidden');
    moreWrap.classList.add('hidden');
    const exp = $('convExportMenu'); if (exp) exp.classList.add('hidden');
    if (tb.scrollWidth > tb.clientWidth + 1) {
      actions.classList.add('hidden');
      moreWrap.classList.remove('hidden');
    }
  }

  // HTML export is built MAIN-process side (src/main/exportHtml.js): it needs fs access to
  // the on-disk subagent dialogues and emits a self-contained, Claude-styled viewer app.

  async function doExport(kind) {
    hideExportMenu();
    if (!openFile) return;
    try {
      if (kind === 'jsonl') {
        const r = await api.historyExportRaw(openFile);
        if (r && r.canceled) return;
        toast(r && r.path ? L('conv.exportOk') : L('conv.exportFail'), !!(r && r.path));
      } else if (kind === 'html') {
        const r = await api.historyExportHtml(openFile);
        if (r && r.canceled) return;
        toast(r && r.path ? L('conv.exportOk') : L('conv.exportFail'), !!(r && r.path));
      }
    } catch (_) { toast(L('conv.exportFail'), false); }
  }

  /* ---------- events ---------- */
  function bind() {
    const list = $('convList');
    if (list) list.addEventListener('click', async (e) => {
      const rm = e.target.closest('[data-remove-import]');
      if (rm) {
        e.stopPropagation();
        const file = rm.dataset.removeImport;
        if (!file || !api.historyRemoveImport) return;
        let res; try { res = await api.historyRemoveImport(file); } catch (_) { res = null; } // confirms in main
        if (!res || !res.ok) return; // cancelled or failed → leave the list as-is
        if (file === openFile) { openId = null; openFile = null; }
        await refresh();
        return;
      }
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
    const imp = $('convImportBtn');
    if (imp && api.historyImport) imp.addEventListener('click', async () => {
      imp.disabled = true;
      let r; try { r = await api.historyImport(); } catch (_) { r = null; }
      imp.disabled = false;
      if (!r || r.canceled) return;
      if (!r.imported) { toast((r.skipped ? L('conv.importSkip', { n: r.skipped }) : L('conv.importNone')), r.failed ? false : undefined); }
      else {
        const parts = [L('conv.importDone', { n: r.imported })];
        if (r.skipped) parts.push(L('conv.importSkip', { n: r.skipped }));
        if (r.failed) parts.push(L('conv.importFail', { n: r.failed }));
        toast(parts.join(' · '));
        try { if (api.historySetActive) await api.historySetActive('__imported__'); } catch (_) {}
      }
      await refresh();
    });
    const dirSwitch = $('convDirSwitch');
    if (dirSwitch) dirSwitch.addEventListener('click', async (e) => {
      const btn = e.target.closest('[data-dir]');
      if (!btn) return;
      try { if (api.historySetActive) await api.historySetActive(btn.dataset.dir); } catch (_) {}
      await refresh();
    });
    const toc = $('convToc');
    if (toc) toc.addEventListener('click', (e) => { const it = e.target.closest('.toc-item'); if (it) jumpToMessage(+it.dataset.go, 'start'); });

    // Session tabs: [主会话] [子代理 (N) ▾]. The dropdown lists subagents; picking one jumps the main
    // thread to where it was spawned and expands it inline there (focusSubagent), so it reads in context.
    // The 主会话 tab switches the whole panel back to the root thread.
    const tabs = $('convAgentTabs');
    if (tabs) tabs.addEventListener('click', (e) => {
      if (e.target.closest('[data-agent-dd]')) {
        agentMenuOpen = !agentMenuOpen;
        const menu = tabs.querySelector('.conv-agent-menu');
        if (menu) menu.classList.toggle('hidden', !agentMenuOpen);
        return;
      }
      const it = e.target.closest('[data-agent]');
      if (it) { if (it.dataset.agent === 'main') switchAgent('main'); else focusSubagent(it.dataset.agent); }
    });
    // Close the subagent menu when clicking outside the tab bar.
    document.addEventListener('click', (e) => {
      if (!agentMenuOpen) return;
      if (e.target.closest('#convAgentTabs')) return;
      agentMenuOpen = false;
      const menu = document.querySelector('#convAgentTabs .conv-agent-menu');
      if (menu) menu.classList.add('hidden');
    });

    // Drag-to-resize the left/right panels (middle absorbs the rest). Widths persist; collapse wins via CSS.
    (function initConvResizers() {
      const layout = document.querySelector('.conv-layout');
      const sidebar = document.querySelector('.conv-sidebar');
      const nav = document.querySelector('.conv-nav');
      if (!layout || !sidebar || !nav) return;
      const MIN_LEFT = 200, MIN_RIGHT = 180, MIN_MAIN = 320; // MIN_MAIN keeps the middle usable, not a fixed width
      const num = (v, d) => { const n = parseInt(v, 10); return isFinite(n) ? n : d; };
      let leftW = num(localStorage.getItem('ccbud-conv-leftw'), 248);
      let rightW = num(localStorage.getItem('ccbud-conv-rightw'), 220);
      const apply = () => { sidebar.style.setProperty('--conv-left-w', leftW + 'px'); nav.style.setProperty('--conv-right-w', rightW + 'px'); };
      apply();
      const startDrag = (side, handle, e) => {
        e.preventDefault();
        const total = layout.getBoundingClientRect().width;
        const startX = e.clientX, sL = leftW, sR = rightW;
        layout.classList.add('resizing'); handle.classList.add('dragging');
        const onMove = (ev) => {
          const dx = ev.clientX - startX;
          if (side === 'left') leftW = Math.max(MIN_LEFT, Math.min(total - rightW - MIN_MAIN, sL + dx));
          else rightW = Math.max(MIN_RIGHT, Math.min(total - leftW - MIN_MAIN, sR - dx));
          apply();
        };
        const onUp = () => {
          document.removeEventListener('mousemove', onMove); document.removeEventListener('mouseup', onUp);
          layout.classList.remove('resizing'); handle.classList.remove('dragging');
          try { localStorage.setItem('ccbud-conv-leftw', String(leftW)); localStorage.setItem('ccbud-conv-rightw', String(rightW)); } catch (_) {}
        };
        document.addEventListener('mousemove', onMove); document.addEventListener('mouseup', onUp);
      };
      layout.querySelectorAll('.conv-resizer').forEach((r) => r.addEventListener('mousedown', (e) => startDrag(r.dataset.resize, r, e)));
    })();

    // Collapse the conversation list sidebar / nav panel
    const convSidebar = document.querySelector('.conv-sidebar');
    const I = window.ccbudIcons || {};
    // Left sidebar: ‹ when expanded (collapse leftward), › when collapsed (expand rightward).
    const setChevron = (btn, isCol) => {
      const icon = btn && btn.querySelector('[data-icon]');
      if (icon) icon.innerHTML = isCol ? (I.chevronRight || '›') : (I.chevronLeft || '‹');
    };
    // Right nav is the mirror image: › when expanded (collapse rightward), ‹ when collapsed.
    const setChevronNav = (btn, isCol) => {
      const icon = btn && btn.querySelector('[data-icon]');
      if (icon) icon.innerHTML = isCol ? (I.chevronLeft || '‹') : (I.chevronRight || '›');
    };

    const collapseListBtn = $('btnCollapseConvList');
    if (collapseListBtn && convSidebar) {
      try { if (localStorage.getItem('ccbud-convlist-collapsed') === '1') { convSidebar.classList.add('collapsed'); setChevron(collapseListBtn, true); } } catch (_) {}
      collapseListBtn.addEventListener('click', (e) => {
        e.stopPropagation();
        const isCol = convSidebar.classList.toggle('collapsed');
        setChevron(collapseListBtn, isCol);
        try { localStorage.setItem('ccbud-convlist-collapsed', isCol ? '1' : '0'); } catch (_) {}
      });
    }

    const convNav = document.querySelector('.conv-nav');
    const collapseNavBtn = $('btnCollapseConvNav');
    if (collapseNavBtn && convNav) {
      setChevronNav(collapseNavBtn, false); // default expanded → › (collapse rightward)
      try { if (localStorage.getItem('ccbud-convnav-collapsed') === '1') { convNav.classList.add('collapsed'); setChevronNav(collapseNavBtn, true); } } catch (_) {}
      collapseNavBtn.addEventListener('click', (e) => {
        e.stopPropagation();
        const isCol = convNav.classList.toggle('collapsed');
        setChevronNav(collapseNavBtn, isCol);
        try { localStorage.setItem('ccbud-convnav-collapsed', isCol ? '1' : '0'); } catch (_) {}
      });
    }

    // Detail message search
    const dsearch = $('convDetailSearch');
    if (dsearch) {
      let t;
      dsearch.addEventListener('input', () => { clearTimeout(t); t = setTimeout(() => performDetailSearch(dsearch.value.trim()), 200); });
      dsearch.addEventListener('keydown', (e) => {
        if (e.key === 'Enter') { e.preventDefault(); if (searchOcc.length) gotoDetailSearchMatch(searchIndex + (e.shiftKey ? -1 : 1)); }
        if (e.key === 'Escape') { dsearch.value = ''; clearDetailSearchHighlights(); }
      });
    }
    const dprev = $('convDetailSearchPrev');
    if (dprev) dprev.addEventListener('click', () => { if (searchOcc.length) gotoDetailSearchMatch(searchIndex - 1); });
    const dnext = $('convDetailSearchNext');
    if (dnext) dnext.addEventListener('click', () => { if (searchOcc.length) gotoDetailSearchMatch(searchIndex + 1); });
    const dclear = $('convDetailSearchClear');
    if (dclear) dclear.addEventListener('click', () => { const inp = $('convDetailSearch'); if (inp) inp.value = ''; clearDetailSearchHighlights(); });

    // Load-earlier / load-later (delegated; #convDetail is stable, its innerHTML isn't).
    const detailHost = $('convDetail');
    if (detailHost) detailHost.addEventListener('click', (e) => {
      if (e.target.closest('[data-load-earlier]')) { loadEarlier(); return; }
      if (e.target.closest('[data-load-later]')) { loadLater(); return; }
      // Lazily render an inline subagent transcript the first time its disclosure is opened (its
      // children render the same way, so the tree fills one level per click — never all at once).
      const sum = e.target.closest('.subagent-inline > summary');
      if (sum) fillSubBody(sum.parentElement);
    });

    // Action buttons (inline) + their collapsed "⋯" menu equivalents.
    const copyPathBtn = $('convCopyPathBtn');
    if (copyPathBtn) copyPathBtn.addEventListener('click', doCopyPath);
    const replayBtn = $('convReplayBtn');
    if (replayBtn) replayBtn.addEventListener('click', () => doReplay(replayBtn));

    const moreBtn = $('convMoreBtn');
    const moreMenu = $('convMoreMenu');
    if (moreBtn) moreBtn.addEventListener('click', (e) => { e.stopPropagation(); if (moreBtn.disabled) return; if (moreMenu) moreMenu.classList.toggle('hidden'); });
    if (moreMenu) moreMenu.addEventListener('click', (e) => {
      const it = e.target.closest('[data-more]'); if (!it) return;
      moreMenu.classList.add('hidden');
      const a = it.dataset.more;
      if (a === 'replay') doReplay();
      else if (a === 'copyPath') doCopyPath();
      else if (a === 'jsonl') doExport('jsonl');
      else if (a === 'html') doExport('html');
    });
    document.addEventListener('click', (e) => { if (moreMenu && !e.target.closest('.conv-more-wrap')) moreMenu.classList.add('hidden'); });

    // Responsive toolbar: collapse the action buttons into the "⋯" menu when space is tight.
    const toolbar = document.querySelector('.conv-detail-toolbar');
    if (toolbar && window.ResizeObserver) {
      const ro = new ResizeObserver(() => updateToolbarLayout());
      ro.observe(toolbar);
    }
    updateToolbarLayout();

    // Export menu (JSONL / HTML)
    const exportBtn = $('convExportBtn');
    if (exportBtn) exportBtn.addEventListener('click', (e) => { e.stopPropagation(); if (exportBtn.disabled) return; const m = $('convExportMenu'); if (m) m.classList.toggle('hidden'); });
    const exportMenu = $('convExportMenu');
    if (exportMenu) exportMenu.addEventListener('click', (e) => { const it = e.target.closest('[data-export]'); if (it) doExport(it.dataset.export); });
    document.addEventListener('click', (e) => { if (!e.target.closest('.conv-export-wrap')) hideExportMenu(); });

    // Live follow: ~/.claude/projects changed → refresh list, re-render open session if touched.
    // rerenderDetail rebuilds the WHOLE thread, so during an active Claude Code session (the file
    // is rewritten on every streamed turn) we debounce it — bursts of writes coalesce into one
    // rebuild instead of one-per-write, which was the main "under load" jank (traced).
    let detailTimer;
    // True when a changed file belongs to the OPEN session — its own .jsonl, or one of its
    // subagent files (<session>/subagents/agent-*.jsonl) — so nested subagents live-follow too.
    const touchesOpenSession = (files) => {
      if (!openFile || !files) return false;
      const base = openFile.replace(/\.jsonl$/i, '');
      return files.some((f) => f === openFile || f.indexOf(base + '/subagents/') === 0 || f.indexOf(base + '\\subagents\\') === 0);
    };
    if (api.onHistoryChanged) api.onHistoryChanged((p) => {
      clearTimeout(listTimer);
      listTimer = setTimeout(refresh, 200);
      if (p && p.files && touchesOpenSession(p.files)) {
        clearTimeout(detailTimer);
        detailTimer = setTimeout(() => rerenderDetail(false), 300);
      }
    });

    // Safety-net auto-refresh: while an in-progress (live) session is open, re-read it on a timer in
    // case a file-watch event is missed. rerenderDetail's skip-guard makes this a no-op when nothing
    // changed and only repaints when the view is pinned to the bottom, so it never disrupts reading.
    setInterval(() => { if (openFile && openSessionLive()) rerenderDetail(false); }, 4000);
  }

  window.ccbudConversations = {
    onShow() { refresh(); if (openFile) rerenderDetail(false); },
    // Re-render everything this view owns when the UI language changes.
    setLang() { renderDirSwitch(); renderList(); if (openFile) rerenderDetail(true); },
  };

  bind();
})();
