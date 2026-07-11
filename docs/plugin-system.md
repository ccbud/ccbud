# ccbud 插件系统设计（Sidecar Provider Plugin）

> 目标：让 ccbud 不再只能"绑定 `baseUrl + apiKey` 转发"，而是能通过一个**第三方
> sidecar 程序**，复用某个 coding agent（首个是 Grok/xAI）在本机的**订阅登录态**，
> 直接调用它的推理接口。第一个插件见
> [`cc-bud-grok-build-plugin`](https://github.com/ccbud/cc-bud-grok-build-plugin)，
> 契约见其 `SPEC.md`（ccbud Plugin Contract v1）。

本文是**宿主侧（ccg）**的架构与落地方案。契约（manifest + 控制面 + 数据面的线协议）
是语言中立的，定义在插件仓库的 `SPEC.md`；本文只讲 ccg 如何**消费**这套契约。

---

## 0. 一句话结论

**运行中的插件 == 一个 `baseUrl` 指向 `127.0.0.1:<port>` 的普通 provider。**
ccg 的网关本来就会把请求转发到任意 `baseUrl` 并按 `protocol` 字段做协议翻译
（`src-tauri/src/gateway.rs` 的 `handle()` 单一 fallback + `src-tauri/src/protocol/`
三协议互转）。所以插件系统**不需要在网关里新增任何翻译逻辑**，只需要补三样目前
代码库里不存在的东西：

1. **子进程生命周期**（发现 / 启动 / 崩溃重启 / 关闭）；
2. **动态端口发现**（插件监听哪个端口）；
3. **就绪健康门控**（插件起来了没、登录了没）。

凭证、推理、协议脏活全部封装在插件里，ccg 从不接触厂商登录态。

---

## 1. 为什么是"独立进程 + 本地 HTTP"，而不是动态库

参考项目 CLIProxyAPI 用的是 `dlopen` 原生动态库 + C-ABI（`internal/pluginhost`），
插件与宿主同进程。我们**不采用**这条路，改用 sidecar 进程，理由：

| 维度 | 动态库(dlopen) | **Sidecar 进程(本方案)** |
|---|---|---|
| 语言 | 需与宿主 ABI 兼容 | **任意语言**：Grok 插件用 Go，直接复用 xAI 生态 |
| 崩溃隔离 | 插件崩溃拖垮宿主 | **进程隔离**，插件挂了只影响该 provider |
| 复用现有能力 | 需新增插件宿主机制 | **零新增**：ccg 网关本就是 HTTP 反代 + 协议翻译 |
| 分发 | 平台 ABI 敏感的 .so/.dylib/.dll | **单个自包含二进制** + manifest |
| ccg 现状契合 | ccg 是 Rust/Tauri，dlopen 一个 Go 库很痛 | 天然契合 |

代价是一次本地回环 HTTP 往返，对 LLM 推理（本身几十毫秒到数秒）可忽略。

---

## 2. 三平面架构

```
                    ┌───────────────────────── ccbud host (ccg) ─────────────────────────┐
 Claude Code ──────▶│  gateway.rs (127.0.0.1:8788)                                        │
   (Anthropic)      │    ├─ resolve_routing()  选 provider + 模型映射                     │
                    │    ├─ protocol/*  Anthropic ⇄ Responses 翻译                        │
                    │    └─ 转发到 provider.baseUrl ───────────────┐                      │
                    │  PluginManager (新增)                        │                      │
                    │    ├─ 发现 ~/.ccbud/plugins/<id>/plugin.json │                      │
                    │    ├─ spawn 子进程 + 动态端口 + /healthz 门控 │                      │
                    │    └─ 把插件登记为 backend:"plugin" 的 provider                     │
                    └──────────────────────────────────────────────┼──────────────────────┘
                                                                    ▼
                    ┌──────────────── sidecar plugin (独立进程) ────────────────┐
                    │  data plane :  POST /v1/responses   GET /v1/models        │
                    │  control plane: /healthz  /v1/plugin/status  /auth/*      │
                    │  复用 ~/.grok 登录态 + OAuth 刷新 + 注入 Bearer + SSE     │
                    └──────────────────────────┬───────────────────────────────┘
                                               ▼
                                    https://api.x.ai/v1/responses
```

三平面（详见插件仓库 `SPEC.md`）：

- **Manifest**：`plugin.json`，声明 id / 可执行文件 / 启动参数 / 协议 / 控制面路径 / 模型。
- **控制面**：`/healthz`、`/v1/plugin/status`、`/v1/plugin/auth`、`/v1/plugin/auth/login[/{session}]`、`/v1/plugin/auth/logout`——ccg 用来门控就绪与编排登录。
- **数据面**：`POST /v1/responses`、`GET /v1/models`——ccg 网关直接转发。

### 职责边界（关键）

- **ccbud 只做协议互转**：messages / chat / responses 三者之间的转换全部在本 app
  （`src-tauri/src/protocol/`）。ccbud 向插件转发的，永远是这三种**标准协议之一**。
- **插件是厂商抹平层**：标准协议进、标准协议出。插件内部吸收厂商推理接口的一切非标准性
  ——请求侧净化（如删 `previous_response_id`、空 tools 的 `tool_choice`）、响应侧归一化
  （如把 Grok 的 `response.reasoning_text.*` 改写成标准 `response.reasoning_summary_text.*`）、
  鉴权注入、token 刷新。**厂商升级、变更推理接口时只改插件，ccbud 不受影响**。
- 因此 ccbud 与"某个 coding agent 具体怎么调推理"解耦：它只认标准协议，不认 Grok。

| 关注点 | 归属 |
|---|---|
| messages ⇄ chat ⇄ responses 协议互转 | **ccbud（本 app，`protocol/`）** |
| 把厂商私有/易变推理接口抹平成某一种标准协议（双向） | **插件** |
| 复用厂商登录态、刷新 token、发起推理 | **插件** |

> 这也意味着插件选哪种 `endpoint.protocol` 是**性能/贴合度**取舍，不影响正确性：Grok 插件
> 选 `openai-responses` 因为 xAI 原生接近 Responses，抹平最薄；某个原生是 chat 形状的
> 厂商则可声明 `openai-chat`。无论哪种，ccbud 侧的三协议互转都能对接。

目录约定：`~/.ccbud/plugins/<plugin-id>/`（与现有配置同在 `~/.ccbud`，见
`store.rs::ccbud_home()`）。

---

## 3. 宿主侧改动点（精确到文件/函数）

下面每一处都标了当前代码位置。**Phase 0 完全不用改**（见 §4）；Phase 1 才动这些。

### 3.1 `store.rs::normalize()` — 登记插件 provider 字段

现状：provider 是自由 `serde_json::Value`，但 `normalize()`（`src-tauri/src/store.rs`
约 L80–129）只保留白名单字段，**未知字段会被丢弃**（有测试断言）。因此必须在白名单里
新增插件字段，并对插件 provider **放宽 `baseUrl` 非空校验**（它的 baseUrl 由运行时填）：

```rust
// store.rs normalize() 内，provider 对象规整处新增：
let backend = p.get("backend").and_then(|v| v.as_str()).unwrap_or("http");
out.insert("backend".into(), json!(backend));              // "http" | "plugin"
if backend == "plugin" {
    // 插件 provider：baseUrl 运行时由 PluginManager 覆盖，允许为空
    out.insert("pluginId".into(), json!(p.get("pluginId").and_then(|v| v.as_str()).unwrap_or("")));
    // 不强制 baseUrl 非空
} else {
    // 现有 http provider 逻辑不变
}
```

> 设计选择：用**独立的 `backend` 字段**表达"后端类型"，而不是复用 `protocol`。
> `protocol` 的语义是 **wire 格式**（anthropic / openai-chat / openai-responses），
> 插件同样要用它告诉网关怎么翻译。两者正交。

运行态端口**不落盘**到 config.json（避免污染用户配置），而是放在 PluginManager 内存
态里；如需持久化就写 `~/.ccbud/plugins/<id>/runtime.json`。

### 3.2 新建 `src-tauri/src/plugin.rs` — `PluginManager`

仿照现有 `GatewayState`（`gateway.rs` 约 L159–200，通过 `app.manage()` 注入的单例）。
`start_mock_upstream`（`gateway.rs` 约 L1294–1296）用 `bind 127.0.0.1:0` 让 OS 选端口
的模式可直接借鉴给"动态端口"。

```rust
// src-tauri/src/plugin.rs（蓝图）
pub struct PluginManager {
    plugins: Mutex<HashMap<String, RunningPlugin>>,   // pluginId -> 运行态
    client: reqwest::Client,                          // 探活/控制面用
}

struct RunningPlugin {
    id: String,
    manifest: Manifest,
    child: Option<std::process::Child>,               // 子进程句柄（现库里没有，需新增）
    port: u16,                                         // 从 stdout ready 行解析
    base_url: String,                                  // http://127.0.0.1:<port>/v1
    ready: bool,
    auth_state: String,                                // 缓存的最近 auth 快照
}

impl PluginManager {
    // 发现：扫 ~/.ccbud/plugins/*/plugin.json
    pub fn discover(home: &Path) -> Vec<Manifest> { /* … */ }

    // 启动：substitute args -> spawn -> 读 stdout ready 行拿 port -> 轮询 /healthz
    pub async fn start(&self, id: &str) -> Result<u16, PluginError> {
        // 1. 选 exec[os-arch]，args 里 {port}->0、{home}->plugin_home
        // 2. Command::new(exec).args(...).stdout(piped).stderr(piped).spawn()
        // 3. 读第一行 stdout JSON: {"event":"ready","port":<p>}
        // 4. 循环 GET http://127.0.0.1:<p>/healthz 直到 200 或 readyTimeoutMs 超时
        // 5. 记录 RunningPlugin{ ready:true, base_url:... }
    }

    pub fn stop(&self, id: &str) -> Result<(), PluginError> { /* SIGTERM child */ }
    pub fn base_url(&self, id: &str) -> Option<String> { /* 就绪则返回，否则 None */ }
    pub async fn auth_status(&self, id: &str) -> serde_json::Value { /* GET /v1/plugin/auth */ }
}
```

在 `lib.rs` 的 setup 钩子里（现有 `GatewayState::new` + `gw.start(port)` 约 L1507–1520
旁边）`app.manage(PluginManager::new())`，并对已启用插件调用 `start()`。

### 3.3 `gateway.rs::handle()` — 插件 provider 的转发分支

现状：`handle()` 读取 `provider["baseUrl"] / authToken / name`（约 L734–744）。
对 `backend == "plugin"` 的 provider，改成从 PluginManager 取运行态 base_url，未就绪则
直接 502/503（复用现有 `error_response`，`gateway.rs` 约 L540–542）：

```rust
// gateway.rs handle() 读取 provider 字段处：
let base_url = if provider["backend"] == "plugin" {
    let pid = provider["pluginId"].as_str().unwrap_or_default();
    match state.plugins.base_url(pid) {                 // PluginManager
        Some(u) => u,                                    // http://127.0.0.1:<port>/v1
        None => return error_response(503, "plugin not ready"),
    }
} else {
    provider["baseUrl"].as_str().unwrap_or_default().to_string()
};
// 之后 resolve_routing / 协议翻译 / header 拼装 / SSE 全部走现有逻辑，authToken 留空
```

`resolve_routing()`（`gateway.rs` L53–155，模型映射/别名）**无需改动**——后端类型与模型
路由正交。协议翻译（`protocol/mod.rs`）也**完全复用**。

### 3.4 新增 Tauri 命令 + 前端桥

仿现有 `provider_*`（`lib.rs` L106–166）与 `gateway_set_enabled`（L358–384），在
`invoke_handler`（L1866–1876）注册：

| 命令 | 作用 |
|---|---|
| `plugin_list` | 列出已发现插件 + manifest + 运行/登录状态 |
| `plugin_install` | 从本地目录/zip 安装到 `~/.ccbud/plugins/`（Phase 2 可接 GitHub release） |
| `plugin_set_enabled` | 启用/停用（启用即 `start`，停用即 `stop` + 注销 provider） |
| `plugin_status` | 单个插件综合状态（转发 `/v1/plugin/status`） |
| `plugin_auth_login` | 转发 `/v1/plugin/auth/login`；拿到 `auth_url` 用 Tauri opener 开浏览器 |
| `plugin_auth_poll` | 转发 `/v1/plugin/auth/login/{session}` |
| `plugin_auth_logout` | 转发 `/v1/plugin/auth/logout` |
| `plugin_action` | 运行声明式动作：把表单 `values` 转发到动作的控制面端点（见 §9） |
| `plugin_action_load` | 预填声明式表单：GET 动作的 `loadPath` 拿当前值 |

前端 `src/renderer/tauri-bridge.js`（L33–102 的 `window.ccbud` 桥）加对应方法，
`renderer.js` 加一个"插件"面板（复用 provider 卡片 UI），并按 §9 通用渲染插件声明的按钮/表单。

---

## 4. 分阶段落地

### Phase 0 — 零宿主改动，**今天就能用**

因为插件 == localhost provider，无需改任何 ccg 代码即可验证整条链路：

1. `cc-bud-grok-build-plugin serve --port 8899`
2. ccg 里手动加一个 provider：`baseUrl=http://127.0.0.1:8899/v1`、`protocol=openai-responses`、
   `authToken` 留空、`defaultModel=grok-4.5`、`smallFastModel=grok-3-mini`、开启 `mapDefaultModels`。
3. Claude Code / Codex 照常指向 ccg → 已经在用 Grok 订阅推理。

这一步就是**验证契约**，也让用户先享受到价值。

### Phase 1 — PluginManager 生命周期

实现 §3.2 / §3.3 / §3.1：发现、spawn、动态端口、`/healthz` 门控、崩溃重启、优雅关闭；
`backend:"plugin"` provider 自动注册与 base_url 覆盖。到这一步，用户"启用插件"即自动起
进程、自动路由，不用手动配 provider。

### Phase 2 — UI + 登录编排 + 插件商店

- 插件面板：列表、启用开关、状态灯、登录按钮。
- 登录编排（§5）。
- 插件商店：从 GitHub release 拉取插件（校验校验和/签名后落 `~/.ccbud/plugins/`），
  可借鉴 CLIProxyAPI 的 `internal/pluginstore`（manifest/registry/checksum/github）。

---

## 5. 登录态编排（ccg 从不碰凭证）

```
UI「登录 Grok」按钮
  └─▶ plugin_auth_login  ──▶ POST /v1/plugin/auth/login
        ├─ 返回 {mode:"reused"}       → 直接显示"已用本机 Grok CLI 登录"
        └─ 返回 {mode:"browser", auth_url, session_id}
              ├─ ccg 用 tauri opener 打开 auth_url（浏览器 OAuth）
              └─ 轮询 plugin_auth_poll(session_id) 直到 complete/error
```

ccg 全程只看到 `state`（logged_in / logged_out / expired）和账户邮箱，**永远不接触
access_token / refresh_token**。这恰好绕开了当前代码库里没有的"读厂商凭证"能力——不是
缺陷，而是有意的信任边界。

---

## 6. 安全模型

- **信任边界**：sidecar 是本机进程，权限 = 当前用户。ccg 只会启动 manifest 里 `exec`
  声明的二进制，且只连 `127.0.0.1`。Phase 2 安装时校验来源与校验和/签名。
- **凭证隔离**：ccg 不读写任何厂商登录态；插件自持、自刷、自存（在其 `--home` 下）。
- **端口**：插件只监听 `127.0.0.1`（与网关一致，`gateway.rs:258`）。
- **刷新轮换权衡**：复用本机 CLI 的 refresh token 刷新时，厂商可能轮换旧 token，导致该
  CLI 被登出。缓解见插件 `SPEC.md §7` / README（可让插件走独立 `login` 持有独立 token）。

---

## 7. 接入更多 coding agent（可扩展性）

新增一个 coding agent 只是**再写一个 sidecar 插件**，实现同一份契约即可，**ccg 宿主零改动**
（装进 `~/.ccbud/plugins/` 就被发现）。每个插件各自封装：

- 该 agent 的**登录态复用**（读它的本机凭证文件，或自走 OAuth）；
- 它的**推理调用**（endpoint / 鉴权 / 流式）；
- 它声明的 **wire 协议**（anthropic / openai-chat / openai-responses）。

素材可参考 CLIProxyAPI 已支持的家族：`gemini` / `codex` / `claude` / `qwen` / `kimi` /
`xai`（`internal/auth/*`、`internal/runtime/executor/*`）。可能的后续插件：
`cc-bud-gemini-cli-plugin`、`cc-bud-qwen-plugin`、`cc-bud-copilot-plugin` ……

---

## 8. 与 CLIProxyAPI 的关系

CLIProxyAPI 是**机制参考**，不是运行时依赖。我们从它学到：xAI 复用的是 Grok CLI 的
**公共 OAuth client**（`b1a00492-…`，scope 含 `offline_access grok-cli:access
api:access`），推理走官方 `https://api.x.ai/v1/responses`（OpenAI Responses 形状），
**没有 cookie、没有反爬签名**，唯一门槛是合法的 OAuth Bearer。第一个插件据此用纯标准库
Go 独立实现（零第三方依赖），并进一步支持**直接读取本机 `~/.grok/auth.json`**——比
CLIProxyAPI"自己走一遍 OAuth"更贴合桌面端"复用本机登录态"的诉求。

---

## 9. 声明式 UI 与动作（`ui.actions`）

原则：**插件面板里的一切显示都来自"读协议"，一切按钮交互都走协议**。名称、版本、协议
徽章、模型、运行/登录状态已经全部由 manifest + 控制面提供；`ui.actions` 把**按钮**也纳入
同一套契约——插件声明自己的按钮，宿主**通用渲染**，点击时把交互**转发回插件控制面**。宿主
不认识任何具体动作，也不为某个插件写死 UI。

> 典型用途：插件自带一个「设置」按钮，点开是一个表单（如配置端口号 / 超时 / 默认推理档位），
> 保存后写回插件自己的配置。宿主只负责画表单、收集值、转发，不理解字段含义。

### 9.1 manifest 声明

```jsonc
{
  "ui": {
    "actions": [
      {
        "id": "settings",             // 必填，插件内唯一
        "label": "设置",              // 按钮文案（插件自带多语言由插件负责）
        "kind": "form",               // "form" | "call" | "link"
        "requiresRunning": true,      // 默认 true：插件未运行则按钮禁用
        "submitLabel": "保存",        // form 提交按钮文案（默认走宿主 plugins.save）
        "submitPath": "/v1/plugin/action/settings",  // 可选，默认 /v1/plugin/action/<id>
        "loadPath": "/v1/plugin/action/settings",    // 可选，默认同 submitPath
        "loadOnOpen": true,           // 默认 true：打开表单时 GET loadPath 预填
        "fields": [
          { "key": "port", "label": "端口号", "type": "number",
            "placeholder": "8899", "min": 1, "max": 65535, "required": true,
            "help": "留空则由宿主自动分配" }
        ]
      },
      { "id": "reset", "label": "重置", "kind": "call",
        "confirm": "确定重置该插件的本地配置？", "submitPath": "/v1/plugin/action/reset" },
      { "id": "help", "label": "文档", "kind": "link",
        "url": "https://github.com/ccbud/cc-bud-grok-build-plugin#readme" }
    ]
  }
}
```

**动作类型（`kind`）**

| kind | 宿主行为 |
|---|---|
| `link` | 直接用系统浏览器打开 `url`，不经过插件（`requiresRunning` 对它无意义）。 |
| `call` | 直接 `POST submitPath`（body `{}`）；有 `confirm` 则先弹确认。用于"重置/刷新"等无输入动作。 |
| `form` | 打开由 `fields` 生成的表单；`loadOnOpen` 时先 GET 预填；提交时 `POST submitPath` 表单值。 |

**字段（`fields[]`）** —— `type` 支持 `text`(默认)/`number`/`password`/`textarea`/`select`/
`checkbox`；通用属性 `key`(必填) `label` `placeholder` `required` `default` `help`；
`number` 额外支持 `min`/`max`；`select` 用 `options: [{ value, label }]` 或纯字符串数组。

### 9.2 控制面端点（插件实现）

- **`GET loadPath`**（form 预填，可选）→ `200 { "values": { "port": 8899 } }`。
- **`POST submitPath`**（form/call 提交）body 是表单值对象（call 为 `{}`）→
  成功 `200 { "ok": true, "message": "已保存，重启插件后生效" }`（`message` 可选，宿主
  弹 toast；无则用宿主 `plugins.actionDone`）；失败返回非 2xx + `{ "message": "..." }`，
  宿主把该 `message` 作为错误提示。

宿主侧只认这套线协议：`submitPath`/`loadPath` 属于**宿主内部路由**，`plugin_status` 下发给
前端的 `actions` 会**剥掉**这两个字段（只保留 `id/label/kind/url/fields/...` 等显示信息），
前端永远不直接触碰插件端口——所有转发都经 `plugin_action` / `plugin_action_load`。

### 9.3 数据流

```
插件面板按钮（宿主按 actions 渲染）
  ├─ kind:link  ──▶ 系统浏览器打开 url
  ├─ kind:call  ──▶ plugin_action(id, actionId, {})            ──▶ POST submitPath
  └─ kind:form  ──▶ [loadOnOpen] plugin_action_load ──▶ GET loadPath  预填
                    提交 ──▶ plugin_action(id, actionId, values) ──▶ POST submitPath
                    ◀── { ok, message } ── toast
```

厂商差异、字段校验语义、写盘时机全部封装在插件里；宿主永远只是"画表单 + 转发"，与
§0 的"运行中的插件 == 一个 localhost provider"一脉相承：**UI 也零厂商耦合**。

---

## 附：关键现有代码位置速查

| 关注点 | 位置 |
|---|---|
| 网关启动 / 监听 127.0.0.1 / 单 fallback | `gateway.rs` `start()` L254–277（bind L258，fallback L263） |
| 请求主处理 / 选 provider / 协议决策 | `gateway.rs` `handle()`（provider 选 L714–725，翻译决策 L770–813，读 provider 字段 L734–744） |
| 模型路由 / 别名映射 | `gateway.rs` `resolve_routing()` L53–155 |
| 协议翻译入口 | `protocol/mod.rs`（`Wire`、`decode/encode_*`） |
| provider schema 权威 | `store.rs` `normalize()` L80–129（白名单 L110–127） |
| 配置读写 / `~/.ccbud` | `store.rs` `read_config`/`write_config`/`ccbud_home` |
| provider 命令 | `lib.rs` `provider_upsert` L106–138、`provider_delete` L139–160、`provider_set_active` L161–166、`provider_test` L171–271 |
| 网关开关命令 | `lib.rs` `gateway_set_enabled` L358–384 |
| setup / app.manage 单例 | `lib.rs` L1507–1520；命令注册 L1866–1876 |
| 动态端口(port 0)可借鉴 | `gateway.rs` `start_mock_upstream` L1294–1296 |
| "发现本机工具"可借鉴 | `codexconnect.rs` `config_path` L29–39、`is_available` L44–47；备份/还原 `connect` L84–122 |
| 前端桥 | `src/renderer/tauri-bridge.js` L33–102；provider UI `renderer.js` |
