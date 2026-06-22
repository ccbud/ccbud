<div align="center">

<img src="docs/img/icon.png" alt="CCBUD" width="120" height="120" style="border-radius: 26px; box-shadow: 0 12px 32px rgba(0,0,0,0.18);">

# CCBUD

### Claude Code Buddy

**Point Claude Code at any Anthropic-compatible provider — one click, all local.**

[![Platform](https://img.shields.io/badge/platform-macOS%20%C2%B7%20Windows%20%C2%B7%20Linux-5b6cff?style=flat-square)](#-installation)
[![Built with Electron](https://img.shields.io/badge/built%20with-Electron-47848F?style=flat-square&logo=electron&logoColor=white)](https://www.electronjs.org/)
[![License: GPL-3.0](https://img.shields.io/badge/license-GPL--3.0-3b82f6?style=flat-square)](./LICENSE)

[Installation](#-installation) · [Quick Start](#-quick-start) · [How it works](#-how-it-works)

**English** · [简体中文](./README.zh-CN.md)

</div>

---

**CCBUD** (`cc` Claude Code + `bud` buddy) is a cross-platform desktop app that runs a tiny **local gateway** between Claude Code and any Anthropic-compatible provider — Kimi, DeepSeek, GLM, MiMo and more. Add your providers once, switch with a single click, and let CCBUD wire up Claude Code for you. **You never touch an environment variable.**

<div align="center">
  <img src="docs/img/services.jpg" alt="CCBUD — services view" width="820">
</div>

- **One-click Connect** — CCBUD writes `~/.claude/settings.json` for you, and restores it exactly when you disconnect.
- **Switch in a click** — keep many providers side by side; tap a card to switch instantly.
- **Automatic model mapping** — Claude's default model names are routed to the active provider's models for you.
- **Stays on your machine** — the gateway binds to `127.0.0.1`; nothing leaves your computer.

## 📥 Installation

### Download (recommended)

Grab the latest build for your platform from the **[Releases page](https://github.com/loadchange/clawdy/releases)**:

| Platform | File |
| :-- | :-- |
| **macOS** (Apple Silicon & Intel) | `.dmg` |
| **Windows** | `.exe` installer |
| **Linux** | `.AppImage` / `.deb` |

> **macOS Gatekeeper:** if the first launch is blocked, right-click the app → **Open**, or run
> `xattr -dr com.apple.quarantine /Applications/ccbud.app`.

### Build from source

```bash
git clone https://github.com/loadchange/clawdy.git
cd clawdy
npm install
npm start                 # run in development

# package a distributable for your OS:
npm run dist:mac          # or: dist:win · dist:linux
```

## 🚀 Quick Start

**1 · Add a provider**
Open CCBUD, click **`+`**, pick a preset (GLM · DeepSeek · MiMo · Kimi …) or enter a custom base URL, and paste your API key.

<div align="center"><img src="docs/img/switch.jpg" alt="Add and switch providers" width="760"></div>

**2 · Connect**
Hit the big **Connect** button. CCBUD points Claude Code at the local gateway by writing `env.ANTHROPIC_BASE_URL` and `env.ANTHROPIC_AUTH_TOKEN` into `~/.claude/settings.json` — backing up whatever was there before.

**3 · Use Claude Code**
Start a new Claude Code session and you're on your chosen provider. Switch anytime by clicking another card; hit **Disconnect** to restore your original settings, untouched. Keep CCBUD running while you work — closing the window tucks it into the menu bar / tray.

## 🔧 How it works

```text
Claude Code ──(ANTHROPIC_BASE_URL = 127.0.0.1:port)──▶ CCBUD gateway ──▶ active provider
                                                          │
                                          · swaps in the upstream's real token
                                          · routes / maps model names
                                          · rewrites the model name in the response
                                            back to the name the client asked for
```

CCBUD never edits your providers' own configs — it just runs a forwarding server on your machine. When you use a model alias, the gateway rewrites the `model` field in the response (including the streaming `message_start`) back to the alias, so Claude Code always sees the name it requested. If you don't set `ANTHROPIC_MODEL`, Claude's default `claude-*` model names are mapped to the active provider's main model (with `haiku` → its small model).

## 📸 A look inside

| | |
| :--: | :--: |
| <img src="docs/img/switch.jpg" width="420"><br>Switch providers and map models in a click | <img src="docs/img/monitor.jpg" width="420"><br>Watch every request flow through, live |
| <img src="docs/img/usage.jpg" width="420"><br>Usage at a glance — even from the menu bar | <img src="docs/img/privacy.jpg" width="420"><br>Redact sensitive data before it ever leaves |

## 📄 License

Released under the [GPL-3.0](./LICENSE) license.
