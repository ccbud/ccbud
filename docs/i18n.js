/* CCBUD site i18n — same 5 languages as the app, auto-detected from the browser. No build step.
   Text nodes use [data-i18n]; rich text uses [data-i18n-html]. Choice persists in localStorage. */
(function () {
  var DICT = {
    en: {
      'meta.title': 'CCBUD — Coding CLI Buddy',
      'nav.features': 'Features', 'nav.philosophy': 'Philosophy', 'nav.faq': 'FAQ', 'nav.download': 'Download',
      'hero.badge': 'Open source · macOS',
      'hero.title': "Claude Code,<br><span class='grad-text'>your providers.</span>",
      'hero.sub': 'A calm, native macOS app that lets Claude Code talk to any Anthropic-compatible provider — Kimi, DeepSeek, GLM, MiMo and more. Switch in one click, map models, watch usage, and read your whole history. All local.',
      'hero.expand': "<b>cc</b> · Claude Code &nbsp;+&nbsp; <b>bud</b> · buddy — your Claude Code buddy.",
      'hero.download': 'Download for macOS', 'hero.github': 'View on GitHub',
      'hero.m1': 'Runs entirely on localhost', 'hero.m2': 'No data leaves your machine', 'hero.m3': '5 languages · light & dark',
      'flow.n1t': 'Claude Code', 'flow.n1s': 'your CLI', 'flow.n2t': 'CCBUD', 'flow.n2s': 'local gateway', 'flow.n3t': 'Any provider', 'flow.n3s': 'Kimi · DeepSeek · GLM…',
      'why.kicker': 'The story',
      'why.title': 'Claude Code is brilliant. It just shouldn’t be locked to one door.',
      'why.p1': 'Claude Code is the best coding agent there is — but the moment you want a different model, a cheaper provider, or simply one that’s reachable where you are, you’re editing env vars, juggling settings files, and hoping nothing breaks.',
      'why.quote': 'So we built the buddy we wanted: a local gateway that sits quietly between Claude Code and whatever provider you choose.',
      'why.p2': 'No accounts. No cloud. No telemetry. CCBUD writes Claude Code’s settings for you when you connect, and puts them back exactly as they were when you disconnect. Your keys, history and usage never leave the machine.',
      'why.p3': 'It’s open source because a tool that sits in the middle of your requests should be one you can read, audit, and trust.',
      'feat.kicker': 'Features',
      'feat.title': 'Everything the switch needs — and nothing it doesn’t',
      'feat.lede': 'Each piece exists for a reason. Here’s what it does, and why it works the way it does.',
      'f1.title': 'One-click connect',
      'f1.desc': 'Pick a provider and press connect. CCBUD points Claude Code at the local gateway; press again to go back to official. No terminal, no config spelunking.',
      'f1.note': '<b>Why it’s safe:</b> we snapshot your Claude Code settings before touching them and restore that exact snapshot on disconnect — so you can never get stranded in a half-configured state.',
      'f2.title': 'Every provider, one shelf',
      'f2.desc': 'Keep all your providers in one place and switch with a click. Per provider, map model aliases — so Claude Code keeps asking for “opus” while CCBUD quietly routes it to whatever you chose.',
      'f2.note': '<b>Design choice:</b> aliases, not rewrites. Your prompts, scripts and muscle memory keep using familiar model names; only the destination changes.',
      'f3.title': 'See where it all goes',
      'f3.desc': 'Tokens and requests over time, per model — in the app and in a menu-bar popover you can glance at without switching windows.',
      'f3.note': '<b>Local by design:</b> the numbers are computed on your machine from Claude Code’s own logs. Nothing is sent anywhere to draw a chart.',
      'f4.title': 'Your whole history, finally readable',
      'f4.desc': 'Browse every Claude Code session — subagents, tool calls, thinking, diffs — with instant search and one-click export to JSONL or a self-contained HTML page.',
      'f4.note': '<b>Built for scale:</b> threads with thousands of turns stay smooth because we virtualize the view and search the data, not the DOM — so even a 4,000-message session opens and searches instantly.',
      'f6.title': 'Native, quiet, in your language',
      'f6.desc': 'A real macOS app with native vibrancy, a menu-bar presence, and light & dark themes — in English, 简体中文, 繁體中文, 日本語 and 한국어.',
      'f6.note': '<b>Native where it matters:</b> built on Tauri with virtualized lists and restrained effects, so it feels fast, quiet, and platform-native.',
      'how.kicker': 'How it works', 'how.title': 'Three steps, then forget it’s there',
      'how.s1t': 'Add a provider', 'how.s1d': 'Paste a base URL and key, or pick a preset. Map models if you like.',
      'how.s2t': 'Press connect', 'how.s2d': 'CCBUD writes Claude Code’s settings to point at localhost. That’s it.',
      'how.s3t': 'Use Claude Code', 'how.s3d': 'Everything flows through CCBUD — switch providers anytime, watch usage live.',
      'prin.kicker': 'Principles', 'prin.title': 'What we won’t compromise on',
      'prin.p1t': 'Local-first', 'prin.p1d': 'Your keys, history and usage stay on your machine. No accounts, no cloud, no telemetry.',
      'prin.p2t': 'Reversible', 'prin.p2d': 'Connecting is a snapshot away from undo. We restore your exact settings, always.',
      'prin.p3t': 'Native & calm', 'prin.p3d': 'It should feel like macOS — fast, quiet, and out of your way.',
      'prin.p4t': 'Open source', 'prin.p4d': 'A tool in the middle of your requests should be readable and auditable. It is.',
      'faq.kicker': 'FAQ', 'faq.title': 'Good questions',
      'faq.q1': 'Is it free and open source?', 'faq.a1': 'Yes. CCBUD is open source and free to use — you bring your own provider keys.',
      'faq.q2': 'Does my data go anywhere?', 'faq.a2': 'No. CCBUD runs as a local gateway on your machine. Your keys, conversations and usage never leave it — there is no CCBUD account or server.',
      'faq.q3': 'Which providers work?', 'faq.a3': 'Any Anthropic-compatible endpoint — Kimi, DeepSeek, GLM (Zhipu), MiMo, Zenmux and others. Add a base URL and key, or start from a preset.',
      'faq.q4': 'Will it mess up my Claude Code setup?', 'faq.a4': 'No. CCBUD snapshots your existing Claude Code settings before connecting and restores that exact snapshot when you disconnect.',
      'faq.q5': 'Which platforms are supported?', 'faq.a5': 'macOS today. The gateway core is plain Node, so other platforms are on the roadmap.',
      'cta.title': 'Give Claude Code a buddy', 'cta.sub': 'Open source, local, and quietly powerful. Point Claude Code wherever you need.',
      'cta.download': 'Download for macOS', 'cta.github': 'Star on GitHub',
      'foot.tagline': 'Your Claude Code buddy. Local, open, native.', 'foot.product': 'Product', 'foot.resources': 'Resources',
      'foot.l_features': 'Features', 'foot.l_phil': 'Philosophy', 'foot.l_faq': 'FAQ', 'foot.l_github': 'GitHub', 'foot.l_releases': 'Releases', 'foot.l_issues': 'Issues',
      'foot.made': 'Built for the Claude Code community.', 'foot.lic': 'GPL-3.0 License',
      'showcase.kicker': 'A look inside', 'showcase.title': 'See it in action',
      'cap.switch': 'Switch providers and map models in a click', 'cap.monitor': 'Watch every request flow through, live',
      'cap.usage': 'Usage at a glance — even from the menu bar', 'cap.privacy': 'Redact sensitive data before it ever leaves',
      'cap.desktop': 'Point Claude Desktop at the same gateway, too'
    },
    zh: {
      'meta.title': 'CCBUD — Coding CLI Buddy',
      'nav.features': '功能', 'nav.philosophy': '理念', 'nav.faq': '常见问题', 'nav.download': '下载',
      'hero.badge': '开源 · macOS',
      'hero.title': "Claude Code，<br><span class='grad-text'>由你选择服务商。</span>",
      'hero.sub': '一个安静的原生 macOS 应用，让 Claude Code 接到任意 Anthropic 兼容服务商 —— Kimi、DeepSeek、GLM、MiMo 等。一键切换、映射模型、查看用量、阅读完整历史。全部在本地。',
      'hero.expand': "<b>cc</b> · Claude Code &nbsp;+&nbsp; <b>bud</b> · buddy（伙伴）—— 你的 Claude Code 伙伴。",
      'hero.download': '下载 macOS 版', 'hero.github': '在 GitHub 查看',
      'hero.m1': '完全运行在 localhost', 'hero.m2': '数据不出本机', 'hero.m3': '5 种语言 · 明亮/暗色',
      'flow.n1t': 'Claude Code', 'flow.n1s': '你的 CLI', 'flow.n2t': 'CCBUD', 'flow.n2s': '本地网关', 'flow.n3t': '任意服务商', 'flow.n3s': 'Kimi · DeepSeek · GLM…',
      'why.kicker': '背后的故事',
      'why.title': 'Claude Code 很出色，但它不该只能走一扇门。',
      'why.p1': 'Claude Code 是当下最好的编码 agent —— 可一旦你想换个模型、换个更便宜、或在你这里真正能连上的服务商，就得改环境变量、来回折腾配置文件，还要祈祷别出岔子。',
      'why.quote': '于是我们做了自己想要的那个「伙伴」：一个安静地待在 Claude Code 与你所选服务商之间的本地网关。',
      'why.p2': '没有账号，没有云端，没有遥测。接入时，CCBUD 替你写好 Claude Code 的配置；断开时，再原样还原。你的密钥、历史与用量从不离开本机。',
      'why.p3': '它开源，是因为一个夹在你请求中间的工具，理应能被你阅读、审计与信任。',
      'feat.kicker': '功能',
      'feat.title': '切换所需的一切 —— 不多不少',
      'feat.lede': '每一处设计都有理由。下面是它做什么，以及为什么这样做。',
      'f1.title': '一键接入',
      'f1.desc': '选一个服务商，点「接入」。CCBUD 会把 Claude Code 指向本地网关；再点一次即可切回官方。无需终端，无需翻配置。',
      'f1.note': '<b>为何安全：</b>动手前我们会先为你的 Claude Code 配置做快照，断开时原样还原 —— 你永远不会被卡在「配置改了一半」的状态里。',
      'f2.title': '所有服务商，一处管理',
      'f2.desc': '把所有服务商放在一起，一点即换。可为每个服务商配置模型别名映射 —— Claude Code 依旧请求「opus」，CCBUD 在背后悄悄路由到你选的模型。',
      'f2.note': '<b>设计取舍：</b>用别名，而非改写。你的提示词、脚本与肌肉记忆继续使用熟悉的模型名，只有目的地变了。',
      'f3.title': '看清每一份消耗',
      'f3.desc': '按模型统计 token 与请求随时间的变化 —— 应用内有，菜单栏弹窗也有，不切窗口就能瞥一眼。',
      'f3.note': '<b>本地计算：</b>这些数字由本机从 Claude Code 自己的日志算出。画一张图，不会把任何东西发往别处。',
      'f4.title': '完整历史，终于读得顺',
      'f4.desc': '浏览每一次 Claude Code 会话 —— 子代理、工具调用、思考、diff —— 支持即时搜索，一键导出为 JSONL 或自包含的 HTML 页面。',
      'f4.note': '<b>为规模而生：</b>上千轮的对话依然流畅，因为我们虚拟化了视图、搜索的是数据而非 DOM —— 即便 4000 条消息的会话，也能瞬间打开与搜索。',
      'f6.title': '原生、安静、说你的语言',
      'f6.desc': '一个真正的 macOS 应用：原生毛玻璃、菜单栏常驻、明暗主题 —— 提供 English、简体中文、繁體中文、日本語 与 한국어。',
      'f6.note': '<b>该原生的地方就原生：</b>基于 Tauri，配合列表虚拟化与克制的视觉效果，让它快、静，并像系统的一部分。',
      'how.kicker': '工作方式', 'how.title': '三步，然后忘了它的存在',
      'how.s1t': '添加服务商', 'how.s1d': '粘贴 base URL 与密钥，或选个预设。需要的话再配模型映射。',
      'how.s2t': '点击接入', 'how.s2d': 'CCBUD 写好 Claude Code 的配置，指向 localhost。就这样。',
      'how.s3t': '照常用 Claude Code', 'how.s3d': '一切都经由 CCBUD —— 随时切换服务商，实时查看用量。',
      'prin.kicker': '理念', 'prin.title': '我们不会妥协的事',
      'prin.p1t': '本地优先', 'prin.p1d': '密钥、历史与用量都留在本机。没有账号、没有云端、没有遥测。',
      'prin.p2t': '可还原', 'prin.p2d': '接入只是「快照之外的一步」。我们始终原样还原你的配置。',
      'prin.p3t': '原生而安静', 'prin.p3d': '它该有 macOS 的感觉 —— 快、静、不挡你的路。',
      'prin.p4t': '开源', 'prin.p4d': '一个夹在你请求中间的工具，应当可读、可审计。它就是。',
      'faq.kicker': '常见问题', 'faq.title': '一些好问题',
      'faq.q1': '免费且开源吗？', 'faq.a1': '是的。CCBUD 开源、免费使用 —— 你自带服务商密钥。',
      'faq.q2': '我的数据会被传走吗？', 'faq.a2': '不会。CCBUD 以本地网关的形式运行在你机器上。密钥、对话与用量从不离开本机 —— 没有 CCBUD 账号，也没有服务器。',
      'faq.q3': '支持哪些服务商？', 'faq.a3': '任意 Anthropic 兼容端点 —— Kimi、DeepSeek、GLM（智谱）、MiMo、Zenmux 等。填入 base URL 与密钥，或从预设开始。',
      'faq.q4': '会弄乱我的 Claude Code 配置吗？', 'faq.a4': '不会。接入前 CCBUD 会为现有 Claude Code 配置做快照，断开时原样还原。',
      'faq.q5': '支持哪些平台？', 'faq.a5': '目前是 macOS。网关核心是纯 Node，所以其它平台已在路线图上。',
      'cta.title': '给 Claude Code 配个伙伴', 'cta.sub': '开源、本地、安静而强大。让 Claude Code 去到你需要的任何地方。',
      'cta.download': '下载 macOS 版', 'cta.github': '在 GitHub 点亮 Star',
      'foot.tagline': '你的 Claude Code 伙伴。本地、开源、原生。', 'foot.product': '产品', 'foot.resources': '资源',
      'foot.l_features': '功能', 'foot.l_phil': '理念', 'foot.l_faq': '常见问题', 'foot.l_github': 'GitHub', 'foot.l_releases': '发布', 'foot.l_issues': '问题反馈',
      'foot.made': '为 Claude Code 社区而做。', 'foot.lic': 'GPL-3.0 许可证',
      'showcase.kicker': '走进来看看', 'showcase.title': '看看实际运行',
      'cap.switch': '一键切换服务商、映射模型', 'cap.monitor': '实时看着每个请求流过',
      'cap.usage': '用量一目了然 —— 菜单栏也能看', 'cap.privacy': '敏感信息出门前先抹掉',
      'cap.desktop': 'Claude 桌面版也能指向同一个网关'
    },
    'zh-TW': {
      'meta.title': 'CCBUD — Coding CLI Buddy',
      'nav.features': '功能', 'nav.philosophy': '理念', 'nav.faq': '常見問題', 'nav.download': '下載',
      'hero.badge': '開源 · macOS',
      'hero.title': "Claude Code，<br><span class='grad-text'>由你選擇服務商。</span>",
      'hero.sub': '一個安靜的原生 macOS 應用，讓 Claude Code 接到任意 Anthropic 相容服務商 —— Kimi、DeepSeek、GLM、MiMo 等。一鍵切換、對應模型、查看用量、閱讀完整紀錄。全部在本機。',
      'hero.expand': "<b>cc</b> · Claude Code &nbsp;+&nbsp; <b>bud</b> · buddy（夥伴）—— 你的 Claude Code 夥伴。",
      'hero.download': '下載 macOS 版', 'hero.github': '在 GitHub 查看',
      'hero.m1': '完全執行於 localhost', 'hero.m2': '資料不離開本機', 'hero.m3': '5 種語言 · 明亮/暗色',
      'flow.n1t': 'Claude Code', 'flow.n1s': '你的 CLI', 'flow.n2t': 'CCBUD', 'flow.n2s': '本機閘道', 'flow.n3t': '任意服務商', 'flow.n3s': 'Kimi · DeepSeek · GLM…',
      'why.kicker': '背後的故事',
      'why.title': 'Claude Code 很出色，但它不該只能走一扇門。',
      'why.p1': 'Claude Code 是當下最好的程式 agent —— 可一旦你想換個模型、換個更便宜、或在你這裡真正連得上的服務商，就得改環境變數、來回擺弄設定檔，還要祈禱別出狀況。',
      'why.quote': '於是我們做了自己想要的那個「夥伴」：一個安靜待在 Claude Code 與你所選服務商之間的本機閘道。',
      'why.p2': '沒有帳號、沒有雲端、沒有遙測。接入時 CCBUD 替你寫好 Claude Code 的設定，斷開時再原樣還原。你的金鑰、紀錄與用量從不離開本機。',
      'why.p3': '它開源，是因為一個夾在你請求中間的工具，理應能被你閱讀、稽核與信任。',
      'feat.kicker': '功能',
      'feat.title': '切換所需的一切 —— 不多不少',
      'feat.lede': '每一處設計都有理由。以下是它做什麼，以及為何這樣做。',
      'f1.title': '一鍵接入',
      'f1.desc': '選一個服務商，按「接入」。CCBUD 會把 Claude Code 指向本機閘道；再按一次即可切回官方。不需終端機，不必翻設定。',
      'f1.note': '<b>為何安全：</b>動手前我們會先為你的 Claude Code 設定做快照，斷開時原樣還原 —— 你永遠不會卡在「設定改到一半」的狀態。',
      'f2.title': '所有服務商，一處管理',
      'f2.desc': '把所有服務商放在一起，一點即換。可為每個服務商設定模型別名對應 —— Claude Code 仍請求「opus」，CCBUD 在背後悄悄路由到你選的模型。',
      'f2.note': '<b>設計取捨：</b>用別名，而非改寫。你的提示詞、指令稿與肌肉記憶照樣使用熟悉的模型名，只有目的地變了。',
      'f3.title': '看清每一分消耗',
      'f3.desc': '依模型統計 token 與請求隨時間的變化 —— 應用內有，選單列彈窗也有，不切視窗就能瞄一眼。',
      'f3.note': '<b>本機計算：</b>這些數字由本機從 Claude Code 自己的紀錄算出。畫一張圖，不會把任何東西送往別處。',
      'f4.title': '完整紀錄，終於讀得順',
      'f4.desc': '瀏覽每一次 Claude Code 工作階段 —— 子代理、工具呼叫、思考、diff —— 支援即時搜尋，一鍵匯出為 JSONL 或自包含的 HTML 頁面。',
      'f4.note': '<b>為規模而生：</b>上千輪的對話依然流暢，因為我們把視圖虛擬化、搜尋的是資料而非 DOM —— 即使 4000 則訊息的工作階段，也能瞬間開啟與搜尋。',
      'f6.title': '原生、安靜、說你的語言',
      'f6.desc': '一個真正的 macOS 應用：原生毛玻璃、選單列常駐、明暗主題 —— 提供 English、簡體中文、繁體中文、日本語 與 한국어。',
      'f6.note': '<b>該原生的地方就原生：</b>基於 Tauri，配合清單虛擬化與克制的視覺效果，讓它快、靜，並像系統的一部分。',
      'how.kicker': '運作方式', 'how.title': '三步，然後忘了它的存在',
      'how.s1t': '新增服務商', 'how.s1d': '貼上 base URL 與金鑰，或選個預設。需要的話再設定模型對應。',
      'how.s2t': '點擊接入', 'how.s2d': 'CCBUD 寫好 Claude Code 的設定，指向 localhost。就這樣。',
      'how.s3t': '照常用 Claude Code', 'how.s3d': '一切都經由 CCBUD —— 隨時切換服務商，即時查看用量。',
      'prin.kicker': '理念', 'prin.title': '我們不會妥協的事',
      'prin.p1t': '本機優先', 'prin.p1d': '金鑰、紀錄與用量都留在本機。沒有帳號、沒有雲端、沒有遙測。',
      'prin.p2t': '可還原', 'prin.p2d': '接入只是「快照之外的一步」。我們始終原樣還原你的設定。',
      'prin.p3t': '原生而安靜', 'prin.p3d': '它該有 macOS 的感覺 —— 快、靜、不擋你的路。',
      'prin.p4t': '開源', 'prin.p4d': '一個夾在你請求中間的工具，應當可讀、可稽核。它就是。',
      'faq.kicker': '常見問題', 'faq.title': '一些好問題',
      'faq.q1': '免費且開源嗎？', 'faq.a1': '是的。CCBUD 開源、免費使用 —— 你自帶服務商金鑰。',
      'faq.q2': '我的資料會被傳走嗎？', 'faq.a2': '不會。CCBUD 以本機閘道的形式執行在你機器上。金鑰、對話與用量從不離開本機 —— 沒有 CCBUD 帳號，也沒有伺服器。',
      'faq.q3': '支援哪些服務商？', 'faq.a3': '任意 Anthropic 相容端點 —— Kimi、DeepSeek、GLM（智譜）、MiMo、Zenmux 等。填入 base URL 與金鑰，或從預設開始。',
      'faq.q4': '會弄亂我的 Claude Code 設定嗎？', 'faq.a4': '不會。接入前 CCBUD 會為現有 Claude Code 設定做快照，斷開時原樣還原。',
      'faq.q5': '支援哪些平台？', 'faq.a5': '目前是 macOS。閘道核心是純 Node，因此其它平台已在路線圖上。',
      'cta.title': '給 Claude Code 配個夥伴', 'cta.sub': '開源、本機、安靜而強大。讓 Claude Code 去到你需要的任何地方。',
      'cta.download': '下載 macOS 版', 'cta.github': '在 GitHub 點亮 Star',
      'foot.tagline': '你的 Claude Code 夥伴。本機、開源、原生。', 'foot.product': '產品', 'foot.resources': '資源',
      'foot.l_features': '功能', 'foot.l_phil': '理念', 'foot.l_faq': '常見問題', 'foot.l_github': 'GitHub', 'foot.l_releases': '發布', 'foot.l_issues': '問題回報',
      'foot.made': '為 Claude Code 社群而做。', 'foot.lic': 'GPL-3.0 授權',
      'showcase.kicker': '走進來看看', 'showcase.title': '看看實際運行',
      'cap.switch': '一鍵切換服務商、對應模型', 'cap.monitor': '即時看著每個請求流過',
      'cap.usage': '用量一目了然 —— 選單列也能看', 'cap.privacy': '敏感資訊出門前先抹掉',
      'cap.desktop': 'Claude 桌面版也能指向同一個閘道'
    },
    ja: {
      'meta.title': 'CCBUD — Coding CLI Buddy',
      'nav.features': '機能', 'nav.philosophy': '理念', 'nav.faq': 'FAQ', 'nav.download': 'ダウンロード',
      'hero.badge': 'オープンソース · macOS',
      'hero.title': "Claude Code を、<br><span class='grad-text'>あなたのプロバイダーへ。</span>",
      'hero.sub': 'Claude Code を任意の Anthropic 互換プロバイダー（Kimi・DeepSeek・GLM・MiMo ほか）につなぐ、静かなネイティブ macOS アプリ。ワンクリックで切替、モデルをマッピング、使用量を確認、履歴をすべて閲覧。すべてローカルで。',
      'hero.expand': "<b>cc</b> · Claude Code &nbsp;+&nbsp; <b>bud</b> · buddy（相棒）—— あなたの Claude Code の相棒。",
      'hero.download': 'macOS 版をダウンロード', 'hero.github': 'GitHub で見る',
      'hero.m1': '完全に localhost で動作', 'hero.m2': 'データは端末から出ない', 'hero.m3': '5 言語 · ライト/ダーク',
      'flow.n1t': 'Claude Code', 'flow.n1s': 'あなたの CLI', 'flow.n2t': 'CCBUD', 'flow.n2s': 'ローカルゲートウェイ', 'flow.n3t': '任意のプロバイダー', 'flow.n3s': 'Kimi · DeepSeek · GLM…',
      'why.kicker': '背景にある話',
      'why.title': 'Claude Code は素晴らしい。ただ、一つの扉に縛られるべきではない。',
      'why.p1': 'Claude Code は今もっとも優れたコーディング agent です。けれど別のモデルや、より安いプロバイダー、あるいは単にあなたの環境でつながるプロバイダーに切り替えたい瞬間、環境変数をいじり、設定ファイルをやりくりし、壊れないことを祈ることになります。',
      'why.quote': 'そこで、私たち自身が欲しかった「相棒」を作りました。Claude Code と選んだプロバイダーの間に静かに座るローカルゲートウェイです。',
      'why.p2': 'アカウントなし、クラウドなし、テレメトリなし。接続時に CCBUD が Claude Code の設定を書き込み、切断時に元どおりに戻します。鍵・履歴・使用量が端末から出ることはありません。',
      'why.p3': 'オープンソースなのは、リクエストの中間に座るツールこそ、読めて、監査でき、信頼できるべきだからです。',
      'feat.kicker': '機能',
      'feat.title': '切替に必要なすべて —— 余計なものはなし',
      'feat.lede': 'どの要素にも理由があります。何をするか、そしてなぜそうなっているか。',
      'f1.title': 'ワンクリック接続',
      'f1.desc': 'プロバイダーを選んで「接続」を押すだけ。CCBUD が Claude Code をローカルゲートウェイに向け、もう一度押せば公式に戻ります。ターミナルも設定の探検も不要。',
      'f1.note': '<b>安全な理由：</b>触れる前に Claude Code の設定をスナップショットし、切断時にその同じ状態へ正確に戻します。中途半端な設定で立ち往生することはありません。',
      'f2.title': 'すべてのプロバイダーを一枚の棚に',
      'f2.desc': 'すべてのプロバイダーを一か所にまとめ、クリックで切替。プロバイダーごとにモデルのエイリアスをマッピング —— Claude Code は「opus」を求め続け、CCBUD が裏で選んだ先へ静かにルーティングします。',
      'f2.note': '<b>設計上の選択：</b>書き換えではなくエイリアス。プロンプト・スクリプト・体に染みた呼び名はそのまま、変わるのは行き先だけ。',
      'f3.title': 'どこへ消えるかを可視化',
      'f3.desc': 'モデルごとのトークンとリクエストの推移を —— アプリ内でも、メニューバーのポップオーバーでも、ウィンドウを切り替えずに一目で。',
      'f3.note': '<b>ローカル計算：</b>数値は Claude Code 自身のログから端末上で算出します。グラフのために何かを送ることはありません。',
      'f4.title': '全履歴が、ついに読みやすく',
      'f4.desc': 'すべての Claude Code セッション —— サブエージェント、ツール呼び出し、思考、diff —— を閲覧。即時検索、JSONL や単体 HTML への書き出しもワンクリック。',
      'f4.note': '<b>スケール対応：</b>数千ターンのスレッドも滑らか。ビューを仮想化し、DOM ではなくデータを検索するため、4,000 メッセージのセッションでも瞬時に開いて検索できます。',
      'f6.title': 'ネイティブで、静かで、あなたの言語で',
      'f6.desc': 'ネイティブの曇りガラス、メニューバー常駐、ライト/ダークを備えた本物の macOS アプリ —— English・简体中文・繁體中文・日本語・한국어 に対応。',
      'f6.note': '<b>ネイティブであるべきところはネイティブに：</b>Tauri ベースで、リスト仮想化と控えめな視覚効果により、速く静かでプラットフォームになじむ体験にしています。',
      'how.kicker': '使い方', 'how.title': '3 ステップ、あとは存在を忘れる',
      'how.s1t': 'プロバイダーを追加', 'how.s1d': 'base URL と鍵を貼り付けるか、プリセットを選択。必要ならモデルもマッピング。',
      'how.s2t': '接続を押す', 'how.s2d': 'CCBUD が Claude Code の設定を localhost に向けて書き込みます。それだけ。',
      'how.s3t': 'いつも通り Claude Code', 'how.s3d': 'すべてが CCBUD を通ります —— いつでもプロバイダーを切替、使用量をリアルタイムで確認。',
      'prin.kicker': '理念', 'prin.title': '妥協しないこと',
      'prin.p1t': 'ローカルファースト', 'prin.p1d': '鍵・履歴・使用量は端末に留まります。アカウントなし、クラウドなし、テレメトリなし。',
      'prin.p2t': '元に戻せる', 'prin.p2d': '接続はスナップショット一つで取り消せます。あなたの設定を常に正確に復元します。',
      'prin.p3t': 'ネイティブで静か', 'prin.p3d': 'macOS のように感じられるべき —— 速く、静かで、邪魔をしない。',
      'prin.p4t': 'オープンソース', 'prin.p4d': 'リクエストの中間に座るツールは、読めて監査できるべき。実際にそうです。',
      'faq.kicker': 'FAQ', 'faq.title': '良い質問',
      'faq.q1': '無料でオープンソースですか？', 'faq.a1': 'はい。CCBUD はオープンソースで無料です —— プロバイダーの鍵はご自身で用意します。',
      'faq.q2': 'データはどこかへ送られますか？', 'faq.a2': 'いいえ。CCBUD は端末上でローカルゲートウェイとして動作します。鍵・会話・使用量が外へ出ることはなく、CCBUD のアカウントもサーバーもありません。',
      'faq.q3': '対応プロバイダーは？', 'faq.a3': 'Anthropic 互換のエンドポイントなら何でも —— Kimi・DeepSeek・GLM（Zhipu）・MiMo・Zenmux など。base URL と鍵を追加するか、プリセットから始められます。',
      'faq.q4': 'Claude Code の設定が壊れませんか？', 'faq.a4': 'いいえ。接続前に既存の Claude Code 設定をスナップショットし、切断時にその同じ状態へ戻します。',
      'faq.q5': '対応プラットフォームは？', 'faq.a5': '現在は macOS。ゲートウェイの中核は素の Node なので、他プラットフォームもロードマップにあります。',
      'cta.title': 'Claude Code に相棒を', 'cta.sub': 'オープンソース、ローカル、静かにパワフル。Claude Code を必要な先へ。',
      'cta.download': 'macOS 版をダウンロード', 'cta.github': 'GitHub でスター',
      'foot.tagline': 'あなたの Claude Code の相棒。ローカル・オープン・ネイティブ。', 'foot.product': 'プロダクト', 'foot.resources': 'リソース',
      'foot.l_features': '機能', 'foot.l_phil': '理念', 'foot.l_faq': 'FAQ', 'foot.l_github': 'GitHub', 'foot.l_releases': 'リリース', 'foot.l_issues': 'Issue',
      'foot.made': 'Claude Code コミュニティのために。', 'foot.lic': 'GPL-3.0 ライセンス',
      'showcase.kicker': '中をのぞく', 'showcase.title': '実際の動きを見る',
      'cap.switch': 'ワンクリックで切替・モデルをマッピング', 'cap.monitor': 'すべてのリクエストをライブで',
      'cap.usage': '使用量を一目で —— メニューバーからも', 'cap.privacy': '出ていく前に機微情報を伏せる',
      'cap.desktop': 'Claude Desktop も同じゲートウェイへ'
    },
    ko: {
      'meta.title': 'CCBUD — Coding CLI Buddy',
      'nav.features': '기능', 'nav.philosophy': '철학', 'nav.faq': 'FAQ', 'nav.download': '다운로드',
      'hero.badge': '오픈소스 · macOS',
      'hero.title': "Claude Code를,<br><span class='grad-text'>당신의 프로바이더로.</span>",
      'hero.sub': 'Claude Code를 임의의 Anthropic 호환 프로바이더(Kimi·DeepSeek·GLM·MiMo 등)에 연결하는 조용한 네이티브 macOS 앱. 한 번에 전환하고, 모델을 매핑하고, 사용량을 확인하고, 전체 기록을 읽으세요. 모두 로컬에서.',
      'hero.expand': "<b>cc</b> · Claude Code &nbsp;+&nbsp; <b>bud</b> · buddy(친구) —— 당신의 Claude Code 친구.",
      'hero.download': 'macOS용 다운로드', 'hero.github': 'GitHub에서 보기',
      'hero.m1': '전적으로 localhost에서 실행', 'hero.m2': '데이터가 기기를 벗어나지 않음', 'hero.m3': '5개 언어 · 라이트/다크',
      'flow.n1t': 'Claude Code', 'flow.n1s': '당신의 CLI', 'flow.n2t': 'CCBUD', 'flow.n2s': '로컬 게이트웨이', 'flow.n3t': '임의의 프로바이더', 'flow.n3s': 'Kimi · DeepSeek · GLM…',
      'why.kicker': '뒷이야기',
      'why.title': 'Claude Code는 훌륭합니다. 다만 하나의 문에만 묶일 이유는 없습니다.',
      'why.p1': 'Claude Code는 현존 최고의 코딩 agent입니다 —— 그러나 다른 모델, 더 저렴한, 혹은 당신의 환경에서 실제로 닿는 프로바이더로 바꾸려는 순간, 환경변수를 손보고 설정 파일을 이리저리 옮기며 아무것도 깨지지 않기를 바라게 됩니다.',
      'why.quote': '그래서 우리가 원하던 그 “친구”를 만들었습니다. Claude Code와 당신이 고른 프로바이더 사이에 조용히 앉는 로컬 게이트웨이입니다.',
      'why.p2': '계정 없음, 클라우드 없음, 텔레메트리 없음. 연결할 때 CCBUD가 Claude Code 설정을 대신 써주고, 끊을 때 원래대로 정확히 되돌립니다. 키·기록·사용량은 기기를 벗어나지 않습니다.',
      'why.p3': '오픈소스인 이유는, 당신의 요청 한가운데 앉는 도구라면 읽고, 감사하고, 신뢰할 수 있어야 하기 때문입니다.',
      'feat.kicker': '기능',
      'feat.title': '전환에 필요한 모든 것 —— 그뿐',
      'feat.lede': '모든 요소에는 이유가 있습니다. 무엇을 하는지, 그리고 왜 그렇게 동작하는지.',
      'f1.title': '원클릭 연결',
      'f1.desc': '프로바이더를 고르고 “연결”을 누르세요. CCBUD가 Claude Code를 로컬 게이트웨이로 향하게 하고, 다시 누르면 공식으로 돌아갑니다. 터미널도, 설정 탐험도 없습니다.',
      'f1.note': '<b>안전한 이유:</b> 손대기 전에 Claude Code 설정을 스냅샷하고, 끊을 때 그 스냅샷 그대로 복원합니다 —— 절반만 설정된 상태에 갇히는 일은 없습니다.',
      'f2.title': '모든 프로바이더를 한 선반에',
      'f2.desc': '모든 프로바이더를 한곳에 두고 클릭으로 전환. 프로바이더마다 모델 별칭을 매핑 —— Claude Code는 계속 “opus”를 요청하고, CCBUD가 뒤에서 당신이 고른 곳으로 조용히 라우팅합니다.',
      'f2.note': '<b>설계 선택:</b> 재작성이 아니라 별칭. 프롬프트·스크립트·익숙한 호출명은 그대로 두고, 바뀌는 건 목적지뿐.',
      'f3.title': '어디로 쓰이는지 보기',
      'f3.desc': '모델별 토큰과 요청 추이를 —— 앱에서도, 메뉴 막대 팝오버에서도 창을 바꾸지 않고 한눈에.',
      'f3.note': '<b>로컬 계산:</b> 수치는 Claude Code 자체 로그에서 기기 안에서 계산됩니다. 차트를 그리려고 무언가를 보내지 않습니다.',
      'f4.title': '전체 기록을, 드디어 읽기 좋게',
      'f4.desc': '모든 Claude Code 세션 —— 서브에이전트, 도구 호출, 사고, diff —— 을 탐색. 즉시 검색과 JSONL/단일 HTML 내보내기를 한 번에.',
      'f4.note': '<b>규모를 위한 설계:</b> 수천 턴의 스레드도 부드럽습니다. 뷰를 가상화하고 DOM이 아닌 데이터를 검색하기에, 4,000개 메시지 세션도 즉시 열리고 검색됩니다.',
      'f6.title': '네이티브하게, 조용하게, 당신의 언어로',
      'f6.desc': '네이티브 비브런시, 메뉴 막대 상주, 라이트/다크를 갖춘 진짜 macOS 앱 —— English·简体中文·繁體中文·日本語·한국어 지원.',
      'f6.note': '<b>네이티브가 중요한 곳은 네이티브하게:</b> Tauri 기반에 리스트 가상화와 절제된 시각 효과를 더해 빠르고 조용하며 플랫폼에 자연스럽게 맞습니다.',
      'how.kicker': '작동 방식', 'how.title': '세 단계, 그다음엔 존재를 잊으세요',
      'how.s1t': '프로바이더 추가', 'how.s1d': 'base URL과 키를 붙여넣거나 프리셋을 고르세요. 원하면 모델도 매핑.',
      'how.s2t': '연결 누르기', 'how.s2d': 'CCBUD가 Claude Code 설정을 localhost로 향하게 씁니다. 그게 전부.',
      'how.s3t': '평소처럼 Claude Code', 'how.s3d': '모든 것이 CCBUD를 거칩니다 —— 언제든 프로바이더 전환, 사용량 실시간 확인.',
      'prin.kicker': '철학', 'prin.title': '타협하지 않는 것',
      'prin.p1t': '로컬 우선', 'prin.p1d': '키·기록·사용량은 기기에 남습니다. 계정 없음, 클라우드 없음, 텔레메트리 없음.',
      'prin.p2t': '되돌릴 수 있음', 'prin.p2d': '연결은 스냅샷 한 번이면 취소됩니다. 당신의 설정을 늘 정확히 복원합니다.',
      'prin.p3t': '네이티브하고 조용함', 'prin.p3d': 'macOS처럼 느껴져야 합니다 —— 빠르고, 조용하고, 길을 막지 않게.',
      'prin.p4t': '오픈소스', 'prin.p4d': '요청 한가운데 앉는 도구는 읽고 감사할 수 있어야 합니다. 실제로 그렇습니다.',
      'faq.kicker': 'FAQ', 'faq.title': '좋은 질문들',
      'faq.q1': '무료이고 오픈소스인가요?', 'faq.a1': '네. CCBUD는 오픈소스이며 무료입니다 —— 프로바이더 키는 직접 준비합니다.',
      'faq.q2': '제 데이터가 어딘가로 가나요?', 'faq.a2': '아니요. CCBUD는 기기에서 로컬 게이트웨이로 동작합니다. 키·대화·사용량은 절대 벗어나지 않으며, CCBUD 계정도 서버도 없습니다.',
      'faq.q3': '어떤 프로바이더가 되나요?', 'faq.a3': 'Anthropic 호환 엔드포인트라면 무엇이든 —— Kimi·DeepSeek·GLM(Zhipu)·MiMo·Zenmux 등. base URL과 키를 추가하거나 프리셋에서 시작하세요.',
      'faq.q4': 'Claude Code 설정이 망가지나요?', 'faq.a4': '아니요. 연결 전 기존 Claude Code 설정을 스냅샷하고, 끊을 때 그 스냅샷 그대로 복원합니다.',
      'faq.q5': '지원 플랫폼은?', 'faq.a5': '현재는 macOS. 게이트웨이 코어는 순수 Node라 다른 플랫폼도 로드맵에 있습니다.',
      'cta.title': 'Claude Code에 친구를', 'cta.sub': '오픈소스, 로컬, 조용히 강력. Claude Code를 필요한 곳으로.',
      'cta.download': 'macOS용 다운로드', 'cta.github': 'GitHub에서 스타',
      'foot.tagline': '당신의 Claude Code 친구. 로컬·오픈·네이티브.', 'foot.product': '제품', 'foot.resources': '리소스',
      'foot.l_features': '기능', 'foot.l_phil': '철학', 'foot.l_faq': 'FAQ', 'foot.l_github': 'GitHub', 'foot.l_releases': '릴리스', 'foot.l_issues': '이슈',
      'foot.made': 'Claude Code 커뮤니티를 위해.', 'foot.lic': 'GPL-3.0 라이선스',
      'showcase.kicker': '안을 들여다보기', 'showcase.title': '실제 동작 보기',
      'cap.switch': '원클릭 전환·모델 매핑', 'cap.monitor': '모든 요청을 실시간으로',
      'cap.usage': '사용량을 한눈에 —— 메뉴 막대에서도', 'cap.privacy': '나가기 전에 민감 정보 가리기',
      'cap.desktop': 'Claude Desktop도 같은 게이트웨이로'
    }
  };

  var LANGS = ['en', 'zh', 'zh-TW', 'ja', 'ko'];
  var NAMES = { en: 'English', zh: '简体中文', 'zh-TW': '繁體中文', ja: '日本語', ko: '한국어' };
  var HTMLLANG = { en: 'en', zh: 'zh-Hans', 'zh-TW': 'zh-Hant', ja: 'ja', ko: 'ko' };

  function mapLocale(loc) {
    loc = (loc || '').toLowerCase();
    if (loc.indexOf('zh') === 0) return (/-(tw|hk|mo)\b/.test(loc) || loc.indexOf('hant') >= 0) ? 'zh-TW' : 'zh';
    if (loc.indexOf('ja') === 0) return 'ja';
    if (loc.indexOf('ko') === 0) return 'ko';
    if (loc.indexOf('en') === 0) return 'en';
    return null;
  }
  function detect() {
    try { var s = localStorage.getItem('ccbud-site-lang'); if (s && DICT[s]) return s; } catch (e) {}
    var navs = navigator.languages || [navigator.language || 'en'];
    for (var i = 0; i < navs.length; i++) { var m = mapLocale(navs[i]); if (m) return m; }
    return 'en';
  }
  function apply(lang) {
    var d = DICT[lang] || DICT.en;
    document.documentElement.setAttribute('lang', HTMLLANG[lang] || lang);
    document.querySelectorAll('[data-i18n]').forEach(function (el) { var k = el.getAttribute('data-i18n'); if (d[k] != null) el.textContent = d[k]; });
    document.querySelectorAll('[data-i18n-html]').forEach(function (el) { var k = el.getAttribute('data-i18n-html'); if (d[k] != null) el.innerHTML = d[k]; });
    if (d['meta.title']) document.title = d['meta.title'];
    var cur = document.getElementById('langCur'); if (cur) cur.textContent = NAMES[lang];
    document.querySelectorAll('[data-lang-opt]').forEach(function (b) { b.classList.toggle('on', b.getAttribute('data-lang-opt') === lang); });
    try { localStorage.setItem('ccbud-site-lang', lang); } catch (e) {}
  }

  function buildLangMenu() {
    var menu = document.getElementById('langMenu'); if (!menu) return;
    menu.innerHTML = LANGS.map(function (l) {
      return '<button data-lang-opt="' + l + '">' + NAMES[l] + '<span class="tick">✓</span></button>';
    }).join('');
    menu.querySelectorAll('[data-lang-opt]').forEach(function (b) {
      b.addEventListener('click', function () { apply(b.getAttribute('data-lang-opt')); document.getElementById('lang').classList.remove('open'); });
    });
    var wrap = document.getElementById('lang'), btn = document.getElementById('langBtn');
    if (btn) btn.addEventListener('click', function (e) { e.stopPropagation(); wrap.classList.toggle('open'); });
    document.addEventListener('click', function () { wrap && wrap.classList.remove('open'); });
  }

  function wireChrome() {
    var nav = document.querySelector('.nav');
    if (nav) { var onScroll = function () { nav.classList.toggle('scrolled', window.scrollY > 8); }; onScroll(); window.addEventListener('scroll', onScroll, { passive: true }); }
    if ('IntersectionObserver' in window) {
      var io = new IntersectionObserver(function (ents) { ents.forEach(function (e) { if (e.isIntersecting) { e.target.classList.add('in'); io.unobserve(e.target); } }); }, { threshold: 0.12 });
      document.querySelectorAll('.reveal').forEach(function (el) { io.observe(el); });
    } else { document.querySelectorAll('.reveal').forEach(function (el) { el.classList.add('in'); }); }
  }

  function init() { buildLangMenu(); apply(detect()); wireChrome(); }
  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', init); else init();
})();
