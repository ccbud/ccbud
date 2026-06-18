# Clawdy — Claude Code Gateway

一个跨平台桌面应用（macOS / Windows / Linux），本质是 **Claude Code 的本地网关**。

在 App 里添加多家 Anthropic 兼容服务（智谱 GLM、DeepSeek、小米 MiMo、月之暗面 Kimi……），
**点一下「一键接入」就帮你写好 Claude Code 配置，点一下卡片就切换服务，再点一下「断开接入」原样还原** ——
全程不用懂、不用碰任何环境变量。

## 它解决什么

- 普通人根本不知道 `export ANTHROPIC_BASE_URL` 是干嘛 → **一键接入**，自动写入 Claude Code 设置
- 买了多家服务，每次切换要手动改一堆配置 → 多家**并存**，点一下卡片**即时切换**
- 还要记每家的模型名 → 网关自动把 Claude 默认模型名映射到当前服务的模型
- 用完想还原 → **一键断开**，接入前的配置自动备份并完整恢复

## 工作原理

```
Claude Code ──(ANTHROPIC_BASE_URL=127.0.0.1:port)──▶ Clawdy 网关 ──▶ 激活服务上游
                                                       │
                                          · 替换为上游真实 token
                                          · 模型名路由 / 映射
                                          · 篡改响应里的 model 名回客户端期望值
```

- **不修改任何第三方服务的配置**，只在本机起一个转发 server。
- 客户端可用「上游原名」直连透传；也可用「别名」，网关转发后会把响应里的
  `model` 字段（含流式 `message_start`）改回别名，保证客户端看到的就是它请求的名字。
- 自动模型映射：未配置 `ANTHROPIC_MODEL` 时，Claude Code 发来的 `claude-*`
  默认模型名会被映射到激活服务的主模型（含 `haiku` → 小模型）。

## 接入方式（一键，无需懂环境变量）

1. 打开 App，点 `+` 添加一个或多个服务（选预设 → 粘贴 API Key 即可）。
2. 点首页大按钮 **「一键接入」**。Clawdy 会自动把网关地址写入 Claude Code 的
   `~/.claude/settings.json`（`env.ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN`），
   并清掉会干扰的模型名覆盖项 —— **你不用碰任何环境变量**。
3. 重启 Claude Code（新开会话）即生效。之后想换服务，点一下下面的服务卡片即可，
   **即时切换、无需任何额外操作**；不想用了点 **「断开接入」**，Clawdy 会把你原来的
   配置原样还原。

> Clawdy 会在断开/退出时完整恢复你接入前的 Claude 配置（接入前会自动备份）。
> 接入期间请保持 Clawdy 运行（关闭窗口会缩到托盘继续工作；可开启「开机自启动」）。

也支持手动：首页「高级 / 手动配置」里有网关地址和 `export` 写法。

## 界面

- **服务商**：卡片式管理多家服务，单选激活、拖动排序、一键测试连接；添加/编辑弹窗带预设供应商(GLM/DeepSeek/MiMo/Kimi)与模型映射配置。风格参考 cc-switch。
- **运行监控**：实时统计卡(状态/活跃服务/总请求/成功率/平均耗时)+ 实时请求流(逐条展示 `请求模型 → 实际模型`、是否改写 ✎、状态码、耗时),以及网关日志。风格参考 claude-code-history-viewer。
- **菜单栏用量面板**：网关会从每次响应里抓真实 token 用量。点系统菜单栏图标弹出用量面板
  (总用量 / 请求数 / 活跃天数 / 连续天数 / 峰值时段 / 常用模型 + 活跃热力图 + 按模型占比),
  可切 今日 / 7 天 / 30 天 / 全部;可在「高级」里开启**在菜单栏直接显示 token 数**(xK/xM/xB)。
- 顶部支持浅色 / 深色主题切换。

## 开发 / 运行

```bash
npm install
npm start        # 启动桌面应用
npm test         # 跑网关核心自测（会打真实上游验证转发 + 模型改写）
```

## 安全 / 设置

- 网关只绑定 `127.0.0.1`，不对外暴露。
- **网关访问令牌**：在 App 顶部「① 接入」区可勾选「要求本地访问令牌」。开启后，
  本机任意进程要用网关都必须带上该令牌（否则返回 401），防止别的程序偷用你的上游额度；
  开启后接入区展示的 `ANTHROPIC_AUTH_TOKEN` 会自动变成该令牌值。不开启时，本机任何
  进程都能使用网关。
- **开机自启动**：可在设置里勾选（macOS / Windows / 打包后的 Linux 原生支持）。
- 配置保存在 Electron `userData/config.json`，文件权限 `0600`，上游密钥以明文保存
  （本地桌面应用的常规做法）。

## 打包

```bash
npm run dist:mac     # macOS dmg/zip (x64 + arm64)
npm run dist:win     # Windows nsis/portable
npm run dist:linux   # Linux AppImage/deb
```

### macOS 签名 / 公证

CI 已接好**签名 + 公证**:配置以下仓库 secrets(`Settings → Secrets and variables → Actions`)后，
推送到 `main` 产出的 mac 包会自动签名并公证、Gatekeeper 直接放行(`source=Notarized Developer ID`)。

签名(必填):

| Secret | 说明 |
|--------|------|
| `MAC_CSC_LINK` | `Developer ID Application` 证书 `.p12` 的 base64 |
| `MAC_CSC_KEY_PASSWORD` | `.p12` 导出密码 |
| `APPLE_TEAM_ID` | 10 位 Team ID |

公证(二选一)：

- **A. App Store Connect API Key(推荐)**：`APPLE_API_KEY_P8`(`.p8` 的 base64)、`APPLE_API_KEY_ID`、`APPLE_API_ISSUER`。
- **B. Apple ID**：`APPLE_ID`、`APPLE_APP_SPECIFIC_PASSWORD`(+ 上面的 `APPLE_TEAM_ID`)。

> Developer ID 证书只能由「账号持有人」在 developer.apple.com / Xcode 创建(API 不行);
> 但**公证**可用 API Key。未配置 secrets 时仍出**未签名**包,首次打开右键「打开」或
> `xattr -dr com.apple.quarantine /Applications/Clawdy.app`。
