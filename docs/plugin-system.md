# CC Buddy Plugin System

A CC Buddy plugin lets Claude Code and Codex run on some other coding agent's models
(the first one is **Grok/xAI**) by **reusing that agent's CLI login** — no API key,
no per-token billing. This doc is the contract: what a plugin is and what it must
implement. The reference plugin is
[`grok-build-plugin`](https://github.com/ccbud/grok-build-plugin).

## The one idea

**A running plugin is just an ordinary provider whose `baseUrl` points at
`127.0.0.1:<port>`.** CC Buddy's gateway already forwards requests to any provider and
translates between the standard protocols, so a plugin is simply a small local HTTP
server. All CC Buddy adds on top is lifecycle: find the plugin, start it, wait until it's
healthy, and register it as a provider. Enabling a plugin turns it into a service you
can pick under *Services*; disabling it just stops the process.

```
Claude Code / Codex -> CC Buddy gateway -(one standard protocol)-> your plugin -> the vendor API
```

## Who does what

- **CC Buddy translates protocols.** It converts between Anthropic Messages, OpenAI Chat,
  and OpenAI Responses, and always forwards **one standard protocol** to the plugin.
- **The plugin is the vendor layer.** Standard protocol in, standard protocol out — it
  absorbs everything specific to the vendor: reusing the login, cleaning up the request,
  and normalizing the response. When the vendor changes its API, only the plugin
  changes; CC Buddy doesn't care.

So a plugin picks whichever wire protocol fits its vendor best (Grok is closest to
OpenAI Responses). CC Buddy's translation layer connects it to whatever the client speaks.

## The three pieces

1. **Manifest** (`plugin.json`) — declares the plugin: id, binary, models, protocol,
   and control-plane paths.
2. **Control plane** — management endpoints CC Buddy calls to check the plugin, never for
   inference: `GET /healthz`, `GET /v1/plugin/status`, `GET /v1/plugin/auth`.
3. **Data plane** — the endpoints CC Buddy forwards inference to: `POST /v1/responses`
   and `GET /v1/models`.

## Auth: reuse a CLI login, don't manage it

The whole point is *reuse*. The user already has a coding-agent CLI installed and
signed in (e.g. the Grok CLI, which writes `~/.grok/auth.json`); the plugin reads that
and uses the token. **The plugin never signs in or out — that's the CLI's job.** CC Buddy
never touches the credentials either; it only reads the login *state* (via
`/v1/plugin/auth`) to show "signed in / expired / not signed in" in the UI. If the user
isn't signed in, they run their CLI's login, not anything in CC Buddy.

## The manifest

```jsonc
{
  "spec": "ccbud-plugin/1",
  "id": "grok-build",
  "name": "Grok Build",
  "version": "0.1.1",
  "description": "Run Claude Code / Codex on Grok by reusing your Grok CLI login.",
  "icon": "icon.svg",
  "source": {                       // optional: lets CC Buddy install/update from Git
    "git": "https://github.com/ccbud/grok-build-plugin",
    "branch": "main",
    "build": "make dist"            // run in the clone to produce the binary
  },
  "runtime": {
    "exec": {                       // one binary per platform
      "darwin-arm64": "bin/darwin-arm64/grok-build-plugin",
      "linux-amd64":  "bin/linux-amd64/grok-build-plugin"
    },
    "args": ["serve", "--port", "{port}", "--home", "{home}"]  // {port}/{home} filled in
  },
  "endpoint": {
    "protocol": "openai-responses", // anthropic | openai-chat | openai-responses
    "basePath": "/v1",
    "healthPath": "/healthz",
    "readyTimeoutMs": 8000
  },
  "auth": {
    "statusPath": "/v1/plugin/auth" // read-only login state
  },
  "models": [
    { "alias": "grok-4.5", "upstream": "grok-4.5" },
    { "alias": "grok-build", "upstream": "grok-build" }
  ],
  "modelMapping": { "primary": "grok-4.5", "light": "grok-build" }
}
```

`modelMapping.primary` / `.light` become the service's default and fast models.

## How CC Buddy runs a plugin

Plugins live in `~/.ccbud/plugins/<id>/`. When you enable one, CC Buddy:

1. picks the binary for your platform and spawns it with `{port}`/`{home}` filled in;
2. polls `healthPath` until it returns 200 (or gives up after `readyTimeoutMs`);
3. registers a `backend:"plugin"` provider pointing at `http://127.0.0.1:<port>`.

From then on the gateway routes to it like any other provider — no plugin-specific code.

**Service lifecycle:** the service exists whenever the plugin is *installed* (installing
adds it, uninstalling removes it). Enable/disable only controls whether the process is
running. You can't switch to a plugin service while its plugin is stopped, and the
plugin behind the active service auto-starts on launch.

## Install and update

On the plugins page, **Add from Git** clones the repo, runs `source.build`, and installs
the binary. To publish an update, bump `version` in `plugin.json`; CC Buddy compares it
against the installed copy and offers an in-app update. (Installing from Git builds code
from a URL — only add sources you trust.)

## Declarative UI (`ui.actions`)

Everything in a plugin's card is read from the manifest/control-plane, and a plugin can
declare its **own buttons** too. CC Buddy renders them and forwards clicks back to the
plugin — it never hard-codes a specific plugin's controls.

```jsonc
"ui": {
  "actions": [
    {
      "id": "settings",
      "label": "Settings",
      "kind": "form",              // link | call | form
      "submitPath": "/v1/plugin/action/settings",  // default: /v1/plugin/action/<id>
      "loadPath": "/v1/plugin/action/settings",    // optional: prefill current values
      "fields": [
        { "key": "port", "label": "Port", "type": "number", "min": 1, "max": 65535 }
      ]
    },
    { "id": "docs", "label": "Docs", "kind": "link", "url": "https://..." }
  ]
}
```

- **link** — opens a URL in the browser.
- **call** — POSTs to `submitPath` (optionally after a `confirm`); good for "reset".
- **form** — opens a modal built from `fields`, prefilled via `loadPath`, submitted to
  `submitPath`. Field types: text, number, password, textarea, select, checkbox.

The plugin implements the endpoint: `POST submitPath` with the form values ->
`{ "ok": true, "message": "..." }` (a non-2xx surfaces `message` as an error);
`GET loadPath` -> `{ "values": { ... } }`. CC Buddy only draws the form and forwards — it
never interprets the fields.

## Adding another coding agent

Write another plugin that implements this same contract, drop it in
`~/.ccbud/plugins/`, and CC Buddy discovers it — **the host needs no changes**. Each plugin
handles its own login reuse, its own inference call, and its own wire protocol.
