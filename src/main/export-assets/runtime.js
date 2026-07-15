/* ccbud export viewer runtime. Renders window.__CONV__ into a Claude-styled, themeable,
   searchable single-file app with a sidebar outline and expandable tools/subagents. */

/* ---- usage analytics (Microsoft Clarity) ----
   Runs first (inside the generator's nonce'd script block) so viewer-runtime errors are
   captured too; the injected tag is allowed by the export CSP's clarity.ms origins (see
   exporthtml.rs / exportHtml.js). Offline viewers just queue into the stub and send
   nothing. Only element identifiers ever become event names — message text, paths and
   titles are never sent. */
(function () {
  try {
    var PROJECT_ID = 'xij8wflxsj';
    window.clarity = window.clarity || function () { (window.clarity.q = window.clarity.q || []).push(arguments); };
    if (!document.getElementById('clarity-script')) {
      var s = document.createElement('script');
      s.async = true; s.id = 'clarity-script';
      s.src = 'https://www.clarity.ms/tag/' + PROJECT_ID;
      (document.head || document.documentElement).appendChild(s);
    }
    var track = function (n) { try { window.clarity('event', String(n).slice(0, 250)); } catch (e) {} };
    var tag = function (k, v) { try { if (v != null && v !== '') window.clarity('set', k, String(v).slice(0, 250)); } catch (e) {} };
    var meta = (window.__CONV__ && window.__CONV__.meta) || {};
    tag('surface', 'export');
    tag('assistant', meta.assistant || 'Claude');
    tag('appVersion', window.__CCBUD_VERSION__); // version of the app that generated this export
    track('export:open');
    var name = function (el) {
      for (var n = el, d = 0; n && n.nodeType === 1 && d < 15; n = n.parentElement, d++) {
        if (n.id) return /^m\d+$/.test(n.id) ? '#msg' : '#' + n.id;
        var cls = typeof n.className === 'string' ? n.className.trim().split(/\s+/)[0] : '';
        if (cls) return n.tagName.toLowerCase() + '.' + cls;
      }
      return el && el.nodeType === 1 ? el.tagName.toLowerCase() : 'unknown';
    };
    document.addEventListener('click', function (e) {
      if (e.target && e.target.nodeType === 1) track('click:' + name(e.target));
    }, true);
    var searched = false;
    document.addEventListener('input', function (e) {
      if (!searched && e.target && e.target.id === 'q') { searched = true; track('export:search'); }
    }, true);
    // Error messages can embed local paths or URLs — redact those before tagging.
    var scrubError = function (s) {
      return String(s == null ? 'unknown' : s)
        .replace(/(?:file|https?):\/\/[^\s'")]+/gi, '<url>')
        .replace(/(^|[\s'"(=:,])(?:~\/|\/)[^\s'")]+/g, '$1<path>')
        .replace(/[A-Za-z]:\\[^\s'")]+/g, '<path>')
        .slice(0, 120);
    };
    window.addEventListener('error', function (e) {
      if (e && e.target && e.target !== window && e.target.nodeType === 1) return;
      track('error:js'); tag('lastError', scrubError(e && e.message));
      try { window.clarity('upgrade', 'js-error'); } catch (err) {}
    }, true);
    window.addEventListener('unhandledrejection', function (e) {
      var r = e && e.reason;
      track('error:unhandled-rejection'); tag('lastError', scrubError(r && r.message ? r.message : r));
    });
  } catch (e) {}
})();

(function () {
  var D = window.__CONV__ || { meta: {}, messages: [], subagents: {} };
  var marked = window.marked, hljs = window.hljs;
  if (marked && marked.setOptions) {
    marked.setOptions({ gfm: true, breaks: true });
    try { marked.use({ renderer: { html: function (t) { return esc(typeof t === 'string' ? t : (t && t.text) || ''); } } }); } catch (e) {}
  }

  function esc(s) { return String(s == null ? '' : s).replace(/[&<>"']/g, function (c) { return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]; }); }
  function md(t) { try { return marked ? marked.parse(String(t || '')) : '<p>' + esc(t) + '</p>'; } catch (e) { return '<p>' + esc(t) + '</p>'; } }
  function fmtTok(n) { n = n || 0; if (n < 1000) return '' + n; if (n < 1e6) return (n / 1e3).toFixed(n < 1e4 ? 1 : 0).replace(/\.0$/, '') + 'K'; return (n / 1e6).toFixed(1).replace(/\.0$/, '') + 'M'; }
  function size(s) { var b = s ? s.length : 0; if (!b) return ''; return b < 1024 ? b + ' B' : (b / 1024).toFixed(1) + ' KB'; }
  function trunc(s, n) { s = String(s == null ? '' : s); return s.length > n ? s.slice(0, n) + '…' : s; }
  function shortPath(p) { if (!p) return ''; var a = String(p).split('/'); return a.length > 3 ? '…/' + a.slice(-2).join('/') : p; }

  // results map: tool_use_id -> tool_result, across main + every subagent
  function collectResults(msgs, map) { (msgs || []).forEach(function (m) { (Array.isArray(m.content) ? m.content : []).forEach(function (b) { if (b && b.type === 'tool_result') map[b.tool_use_id] = b; }); }); }
  var RESULTS = {};
  collectResults(D.messages, RESULTS);
  Object.keys(D.subagents || {}).forEach(function (k) { collectResults(D.subagents[k].messages, RESULTS); });
  var USED = {}; // subagent keys embedded inline under their spawn tool

  var TOOL = {
    Bash: ['⌘', 'exec'], Read: ['📖', 'read'], Edit: ['✏️', 'write'], MultiEdit: ['✏️', 'write'], Write: ['📝', 'write'], ApplyPatch: ['✏️', 'write'],
    Grep: ['🔎', 'search'], Glob: ['🔎', 'search'], TodoWrite: ['✅', 'todo'], TaskCreate: ['✅', 'todo'], TaskUpdate: ['✅', 'todo'], TaskList: ['✅', 'todo'],
    Agent: ['🤖', 'agent'], Task: ['🤖', 'agent'], Workflow: ['🛠️', 'agent'], WebSearch: ['🌐', 'net'], WebFetch: ['🌐', 'net'], AskUserQuestion: ['❓', 'ask']
  };
  function toolMeta(n) { if (TOOL[n]) return TOOL[n]; if (/^mcp__/.test(n)) return ['🧩', 'mcp']; return ['🔧', 'default']; }
  // Codex apply_patch envelope: "*** Update File: x" headers → the card's target (file, or "N files").
  function patchTarget(patch) {
    var files = [];
    String(patch || '').split('\n').forEach(function (l) { var m = /^\*\*\*\s+(?:Add|Update|Delete)\s+File:\s+(.+)$/.exec(l.trim()); if (m) files.push(m[1].trim()); });
    if (!files.length) return '';
    return files.length === 1 ? shortPath(files[0]) : files.length + ' files';
  }
  function toolTarget(n, i) {
    if (n === 'Bash') return i.description || '';
    if (n === 'Read' || n === 'Edit' || n === 'MultiEdit' || n === 'Write') return shortPath(i.file_path);
    if (n === 'ApplyPatch') return patchTarget(i.patch);
    if (n === 'Grep' || n === 'Glob') return i.pattern || '';
    if (n === 'Agent' || n === 'Task') return i.subagent_type || 'agent';
    if (n === 'WebSearch') return i.query || ''; if (n === 'WebFetch') return i.url || ''; if (n === 'Workflow') return i.name || '';
    return '';
  }
  function resultText(b) { if (!b) return ''; var c = b.content; if (typeof c === 'string') return c; if (Array.isArray(c)) return c.map(function (x) { return x && x.type === 'text' ? x.text : (x && x.text) || ''; }).join('\n'); return c == null ? '' : JSON.stringify(c); }
  function codePre(t, lang) { return '<pre class="code"><code' + (lang ? ' class="language-' + esc(lang) + '"' : '') + '>' + esc(trunc(t, 16000)) + '</code></pre>'; }
  function diff(o, n) { o = String(o || '').split('\n'); n = String(n || '').split('\n'); return '<div class="diff">' + o.map(function (l) { return '<div class="d-del">- ' + esc(l) + '</div>'; }).join('') + n.map(function (l) { return '<div class="d-add">+ ' + esc(l) + '</div>'; }).join('') + '</div>'; }
  function todos(list) { return '<div class="todos">' + (list || []).map(function (t) { var m = t.status === 'completed' ? '☑' : t.status === 'in_progress' ? '◐' : '☐'; return '<div class="todo ' + esc(t.status || '') + '"><span class="box">' + m + '</span>' + esc(t.content || t.activeForm || '') + '</div>'; }).join('') + '</div>'; }

  function renderTool(b) {
    var name = b.name || 'tool', inp = (b.input && typeof b.input === 'object') ? b.input : {};
    var tm = toolMeta(name), ico = tm[0], cls = tm[1];
    var label = /^mcp__/.test(name) ? ('MCP · ' + name.replace(/^mcp__/, '')) : name;
    var target = toolTarget(name, inp), body = '';
    if (name === 'Bash') body += '<pre class="code"><code>$ ' + esc(inp.command || '') + '</code></pre>';
    else if (name === 'ApplyPatch') body += codePre(inp.patch || '', 'diff');
    else if (name === 'Edit') body += diff(inp.old_string, inp.new_string);
    else if (name === 'MultiEdit') body += (Array.isArray(inp.edits) ? inp.edits : []).map(function (e) { return diff(e.old_string, e.new_string); }).join('');
    else if (name === 'Write') body += codePre(inp.content || '');
    else if (name === 'Grep') { if (inp.path) body += '<div class="tool-desc">in ' + esc(inp.path) + '</div>'; }
    else if (name === 'TodoWrite') body += todos(inp.todos);
    else if (name === 'Agent' || name === 'Task') { if (inp.description) body += '<div class="tool-desc">' + esc(inp.description) + '</div>'; if (inp.prompt) body += '<div class="lbl">prompt</div>' + codePre(inp.prompt); }
    else if (Object.keys(inp).length) body += codePre(JSON.stringify(inp, null, 2));

    var res = RESULTS[b.id], badge, resHtml = '';
    if (res) { var err = !!res.is_error, txt = resultText(res); badge = '<span class="tool-badge ' + (err ? 'err' : 'ok') + '">' + (err ? '✗' : '✓') + (size(txt) ? ' ' + size(txt) : '') + '</span>'; if (txt) resHtml = '<div class="lbl">' + (err ? 'error' : 'result') + '</div><pre class="code tool-result-pre"><code>' + esc(trunc(txt, 16000)) + '</code></pre>'; }
    else badge = '<span class="tool-badge">—</span>';

    var sub = (D.subagents || {})[b.id];
    if (sub) USED[b.id] = true;
    var inner = (body || resHtml) ? ('<div class="tool-body">' + body + resHtml + '</div>') : '';
    var open = (name === 'Agent' || name === 'Task' || (res && res.is_error)) ? ' open' : '';
    return '<details class="tool tool-' + cls + '"' + open + '><summary class="tool-head"><span class="tool-ico">' + ico + '</span><span class="tool-name">' + esc(label) + '</span><span class="tool-target">' + esc(target) + '</span>' + badge + '</summary>' + inner + '</details>' + (sub ? renderSubagent(sub) : '');
  }
  function renderSubagent(sub) {
    return '<div class="subagent"><details class="subagent-d"><summary><span class="subagent-ico">🤖</span><span class="subagent-title">子代理 · ' + esc(sub.type || 'agent') + '</span><span class="subagent-desc">' + esc(sub.description || '') + '</span><span class="subagent-count">' + (sub.count || 0) + ' 条 · ' + fmtTok((sub.totals && sub.totals.out) || 0) + '↓</span></summary><div class="subagent-body"><div class="thread">' + renderThread(sub.messages || []) + '</div></div></details></div>';
  }

  function isReminder(t) { return /<(system-reminder|command-name|local-command)/.test(t); }
  function renderBlocks(content) {
    var blocks = Array.isArray(content) ? content : (typeof content === 'string' ? [{ type: 'text', text: content }] : []);
    var out = '';
    blocks.forEach(function (b) {
      if (!b) return;
      if (b.type === 'text') { if (b.text && b.text.trim()) out += '<div class="prose">' + md(b.text) + '</div>'; }
      else if (b.type === 'thinking') { if (b.thinking && b.thinking.trim()) { var first = b.thinking.split('\n').filter(function (x) { return x.trim(); })[0] || ''; out += '<details class="thinking"><summary>💭 思考 · ' + esc(trunc(first, 64)) + '</summary><div class="prose">' + md(b.thinking) + '</div></details>'; } }
      else if (b.type === 'tool_use') out += renderTool(b);
      else if (b.type === 'image') { var s = b.source || {}; out += s.data ? '<img class="msg-img" style="max-width:340px;border-radius:10px;border:1px solid var(--border);margin:6px 0" src="data:' + esc(s.media_type || 'image/png') + ';base64,' + esc(s.data) + '">' : '<div class="tool-desc">🖼 image' + (s.oversized ? ' (large, omitted)' : '') + '</div>'; }
    });
    return out;
  }
  function turnMeta(m) {
    var bits = []; if (m.model) bits.push(esc(m.model)); if (m.usage) bits.push(fmtTok(m.usage.in) + '↑ ' + fmtTok(m.usage.out) + '↓'); if (m.usage && m.usage.cacheRead) bits.push(fmtTok(m.usage.cacheRead) + ' cache');
    return bits.length ? '<div style="display:flex;gap:6px;flex-wrap:wrap;margin-top:6px">' + bits.map(function (b) { return '<span style="font-size:9.5px;font-family:ui-monospace,monospace;color:var(--text-faint);background:var(--surface-2);border-radius:4px;padding:1px 6px">' + b + '</span>'; }).join('') + '</div>' : '';
  }
  function msgVisible(m) {
    var bl = Array.isArray(m.content) ? m.content : (typeof m.content === 'string' ? [{ type: 'text', text: m.content }] : []);
    if (m.role === 'user') { var vis = bl.filter(function (b) { return b && (b.type === 'text' || b.type === 'image'); }); if (!vis.length) return false; var txt = vis.map(function (b) { return b.text || ''; }).join(''); return !isReminder(txt); }
    return bl.some(function (b) { return b && (b.type === 'text' || b.type === 'thinking' || b.type === 'tool_use' || b.type === 'image'); });
  }
  // tags for sidebar/filtering
  function msgTags(m) {
    var t = { tool: 0, sub: 0 };
    (Array.isArray(m.content) ? m.content : []).forEach(function (b) { if (b && b.type === 'tool_use') { t.tool++; if ((D.subagents || {})[b.id]) t.sub++; } });
    return t;
  }
  function renderThread(msgs) {
    var out = '';
    (msgs || []).forEach(function (m) {
      if (!msgVisible(m)) return;
      var body = renderBlocks(m.content); if (!body) return;
      if (m.role === 'user') out += '<div class="msg user" data-role="user"><div class="msg-name">👤 你</div><div class="bubble">' + body + '</div></div>';
      else { var tg = msgTags(m); out += '<div class="msg assistant" data-role="assistant" data-tool="' + (tg.tool ? 1 : 0) + '" data-sub="' + (tg.sub ? 1 : 0) + '"><div class="msg-name"><span class="dot">✦</span>' + esc(AST) + '</div><div class="body">' + body + turnMeta(m) + '</div></div>'; }
    });
    return out;
  }

  // ===== build shell =====
  var meta = D.meta || {};
  var AST = meta.assistant || 'Claude'; // assistant display name (Codex rollouts export with "Codex")
  function metaLine() {
    var p = [];
    if (meta.model) p.push('<b>' + esc(meta.model) + '</b>');
    if (meta.project) p.push(esc(meta.project));
    if (meta.turns) p.push(meta.turns + ' 轮');
    if (meta.inTok != null) p.push(fmtTok(meta.inTok) + '↑ ' + fmtTok(meta.outTok) + '↓');
    if (meta.cacheTok) p.push(fmtTok(meta.cacheTok) + ' 缓存');
    if (meta.subagentCount) p.push(meta.subagentCount + ' 子代理');
    return p.join(' · ');
  }
  var threadHtml = renderThread(D.messages) || '<div class="empty">空对话</div>';
  var orphanKeys = Object.keys(D.subagents || {}).filter(function (k) { return !USED[k]; });
  var orphanHtml = orphanKeys.length
    ? '<div class="msg assistant" data-role="assistant" data-sub="1"><div class="msg-name"><span class="dot">🤖</span>其他子代理 (' + orphanKeys.length + ')</div><div class="body"><div class="tool-desc">下列子代理未在主时间线中找到明确的调用点（可能由工作流派生或调用记录已省略），单独列出以便查看：</div>' + orphanKeys.map(function (k) { return renderSubagent(D.subagents[k]); }).join('') + '</div></div>'
    : '';
  var app = document.getElementById('app');
  app.innerHTML =
    '<header class="topbar">' +
      '<button class="icon-btn" id="tgSidebar" title="侧边栏">☰</button>' +
      '<div class="topbar-title"><h1>' + esc(meta.title || '对话') + '</h1><div class="topbar-meta">' + metaLine() + '</div></div>' +
      '<div class="topbar-actions">' +
        '<div class="search"><span style="color:var(--text-faint);font-size:12px">🔎</span><input id="q" placeholder="搜索对话…" spellcheck="false"><span class="search-count" id="qc"></span><button class="icon-btn" id="qprev" title="上一个">↑</button><button class="icon-btn" id="qnext" title="下一个">↓</button></div>' +
        '<button class="icon-btn" id="tgTheme" title="切换主题">🌙</button>' +
      '</div>' +
    '</header>' +
    '<div class="workspace">' +
      '<aside class="sidebar">' +
        '<div class="sidebar-filters">' +
          '<button data-filter="all" class="active">全部</button>' +
          '<button data-filter="user">提问</button>' +
          '<button data-filter="tool">工具</button>' +
          '<button data-filter="sub">子代理</button>' +
        '</div><nav class="toc" id="toc"></nav>' +
      '</aside>' +
      '<main class="content"><div class="thread" id="thread">' + threadHtml + orphanHtml + '</div>' +
      '<div class="footer">由 <a class="footer-link" href="https://ccbud.github.io/" target="_blank" rel="noopener">CC Buddy</a> 导出 · ' + (meta.count || 0) + ' 条消息' + (meta.subagentCount ? ' · ' + meta.subagentCount + ' 个子代理' : '') + '<div class="footer-site"><a class="footer-link" href="https://ccbud.github.io/" target="_blank" rel="noopener">https://ccbud.github.io/</a></div></div></main>' +
    '</div>';

  var content = app.querySelector('.content');
  var thread = document.getElementById('thread');

  // ---- sidebar outline (top-level messages only) ----
  var topMsgs = [].slice.call(thread.children).filter(function (el) { return el.classList && el.classList.contains('msg'); });
  var tocHtml = '';
  topMsgs.forEach(function (el, i) {
    el.id = 'm' + i;
    var role = el.getAttribute('data-role');
    var preview = '';
    var p = el.querySelector('.prose'); if (p) preview = p.textContent.trim().slice(0, 60);
    if (!preview) { var tn = el.querySelector('.tool-name'); preview = tn ? tn.textContent : (role === 'user' ? '提问' : '回复'); }
    var tags = '';
    if (role === 'assistant') {
      var ntool = el.querySelectorAll('.tool').length, nsub = el.querySelectorAll(':scope > .body > .subagent, :scope .subagent').length;
      if (el.getAttribute('data-sub') === '1') tags += '<span class="tt sub">🤖 子代理</span>';
      if (ntool) tags += '<span class="tt">' + ntool + ' 工具</span>';
    }
    tocHtml += '<a class="toc-item ' + role + '" data-target="m' + i + '"><span class="toc-role">' + (role === 'user' ? '你' : '✦ ' + esc(AST.toUpperCase())) + '</span><span class="toc-text">' + esc(preview || '…') + '</span>' + (tags ? '<span class="toc-tags">' + tags + '</span>' : '') + '</a>';
  });
  document.getElementById('toc').innerHTML = tocHtml;

  document.getElementById('toc').addEventListener('click', function (e) {
    var it = e.target.closest('.toc-item'); if (!it) return;
    var t = document.getElementById(it.getAttribute('data-target')); if (t) t.scrollIntoView({ behavior: 'smooth', block: 'start' });
  });

  // ---- theme ----
  var THEME_KEY = 'ccbud-export-theme';
  function setTheme(t) { document.documentElement.setAttribute('data-theme', t); document.getElementById('tgTheme').textContent = t === 'dark' ? '☀️' : '🌙'; try { localStorage.setItem(THEME_KEY, t); } catch (e) {} }
  try { var saved = localStorage.getItem(THEME_KEY); if (saved) setTheme(saved); else setTheme(window.matchMedia && window.matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light'); } catch (e) { setTheme('light'); }
  document.getElementById('tgTheme').addEventListener('click', function () { setTheme(document.documentElement.getAttribute('data-theme') === 'dark' ? 'light' : 'dark'); });

  // ---- sidebar toggle ----
  document.getElementById('tgSidebar').addEventListener('click', function () { app.classList.toggle('nosidebar'); });

  // ---- filters ----
  var filters = app.querySelectorAll('.sidebar-filters button');
  filters.forEach(function (btn) {
    btn.addEventListener('click', function () {
      filters.forEach(function (b) { b.classList.remove('active'); }); btn.classList.add('active');
      var f = btn.getAttribute('data-filter');
      topMsgs.forEach(function (el) {
        var show = f === 'all' || (f === 'user' && el.getAttribute('data-role') === 'user') || (f === 'tool' && el.getAttribute('data-tool') === '1') || (f === 'sub' && el.getAttribute('data-sub') === '1');
        el.classList.toggle('hide', !show);
      });
      document.querySelectorAll('.toc-item').forEach(function (it) {
        var el = document.getElementById(it.getAttribute('data-target'));
        it.style.display = el && el.classList.contains('hide') ? 'none' : '';
      });
    });
  });

  // ---- search (highlight + nav, opens matching <details>) ----
  var hits = [], cur = -1, qTimer;
  function clearHits() {
    content.querySelectorAll('mark.s-hit').forEach(function (m) { var p = m.parentNode; if (p) { p.replaceChild(document.createTextNode(m.textContent), m); p.normalize(); } });
    hits = []; cur = -1; document.getElementById('qc').textContent = '';
  }
  function doSearch(q) {
    clearHits(); if (!q) return;
    var rx = new RegExp(q.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'), 'gi');
    var walker = document.createTreeWalker(content, NodeFilter.SHOW_TEXT, null), nodes = [], n;
    while ((n = walker.nextNode())) { if (n.nodeValue && n.nodeValue.trim() && n.parentNode && n.parentNode.nodeName !== 'SCRIPT') nodes.push(n); }
    nodes.forEach(function (tn) {
      var txt = tn.nodeValue; rx.lastIndex = 0; if (!rx.test(txt)) return; rx.lastIndex = 0;
      var frag = document.createDocumentFragment(), last = 0, mm;
      while ((mm = rx.exec(txt))) { if (mm.index > last) frag.appendChild(document.createTextNode(txt.slice(last, mm.index))); var mk = document.createElement('mark'); mk.className = 's-hit'; mk.textContent = mm[0]; frag.appendChild(mk); hits.push(mk); last = mm.index + mm[0].length; if (mm.index === rx.lastIndex) rx.lastIndex++; }
      if (last < txt.length) frag.appendChild(document.createTextNode(txt.slice(last)));
      tn.parentNode.replaceChild(frag, tn);
    });
    if (hits.length) go(0);
    document.getElementById('qc').textContent = hits.length ? '1/' + hits.length : '0';
  }
  function go(i) {
    if (!hits.length) return;
    if (cur >= 0 && hits[cur]) hits[cur].classList.remove('cur');
    cur = (i % hits.length + hits.length) % hits.length;
    var el = hits[cur]; el.classList.add('cur');
    var d = el.closest('details'); while (d) { d.open = true; d = d.parentNode && d.parentNode.closest ? d.parentNode.closest('details') : null; }
    el.scrollIntoView({ behavior: 'smooth', block: 'center' });
    document.getElementById('qc').textContent = (cur + 1) + '/' + hits.length;
  }
  var qInput = document.getElementById('q');
  qInput.addEventListener('input', function () { clearTimeout(qTimer); qTimer = setTimeout(function () { doSearch(qInput.value.trim()); }, 160); });
  qInput.addEventListener('keydown', function (e) { if (e.key === 'Enter') { e.preventDefault(); go(cur + (e.shiftKey ? -1 : 1)); } });
  document.getElementById('qnext').addEventListener('click', function () { go(cur + 1); });
  document.getElementById('qprev').addEventListener('click', function () { go(cur - 1); });

  // ---- code highlighting (chunked, non-blocking) ----
  if (hljs) {
    var codes = [].slice.call(content.querySelectorAll('pre code')), ci = 0;
    (function step() { var end = Math.min(ci + 30, codes.length); for (; ci < end; ci++) { try { hljs.highlightElement(codes[ci]); } catch (e) {} } if (ci < codes.length) (window.requestAnimationFrame || setTimeout)(step); })();
  }

  // ---- scroll-spy (highlight current section in outline) ----
  var spy;
  content.addEventListener('scroll', function () {
    clearTimeout(spy); spy = setTimeout(function () {
      var top = content.scrollTop + 80, active = null;
      for (var i = 0; i < topMsgs.length; i++) { if (topMsgs[i].offsetTop <= top) active = i; else break; }
      document.querySelectorAll('.toc-item').forEach(function (it) { it.classList.toggle('active', it.getAttribute('data-target') === ('m' + active)); });
    }, 90);
  });
})();
