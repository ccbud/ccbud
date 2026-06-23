# Hot updates & releases

CCBUD ships a two-tier in-app updater so installed copies stay current without you chasing
installers.

## The two tiers

| | Hot update | Full update |
| :-- | :-- | :-- |
| **What changed** | JS / renderer only (`src/**`) | Native layer (Electron, bundled binaries, new native deps) |
| **How it applies** | Downloads a verified bundle, extracts it under `<userData>/hot/<version>/`, the bootstrap loads it on next launch | User reinstalls (`.dmg` / `.exe` / `.AppImage` / `.deb`) or `brew upgrade --cask ccbud` |
| **Reinstall / signing** | None — interpreted JS loaded by the already-installed shell | Normal install |
| **Gate** | installed **shell** version ≥ release's `minShellVersion` | otherwise |

### Why it's safe to load JS from `userData`

A hot bundle is plain JS run by the installed native shell — no unsigned native code is loaded, so
macOS hardened runtime / Gatekeeper are unaffected. Bundles are **SHA-256-verified** against the
release manifest (fetched over GitHub HTTPS) and can additionally be **Ed25519-signed**.

## Moving parts

| File | Role |
| :-- | :-- |
| `src/main/bootstrap.js` | Electron entry (`package.json` `main`). Resolves packaged vs. staged bundle; promotes a pending bundle; rolls back a bundle that fails to boot. |
| `src/main/hotpaths.js` | Shared on-disk layout + `state.json` schema (used by both bootstrap and updater). |
| `src/main/updater.js` | Checks GitHub, decides hot vs full, downloads/verifies/stages a bundle, confirms boot, relaunches. |
| `scripts/build-hotupdate.js` | Builds `ccbud-hotupdate-<ver>.json.gz` + `hotupdate-manifest.json`. |
| `scripts/gen-update-keys.js` | Generates an Ed25519 signing keypair. |
| `scripts/update-cask.js` | Regenerates the Homebrew cask for the current version. |

### `state.json` (under `<userData>/hot/`)

```jsonc
{
  "active":   { "version": "1.1.0", "dir": "1.1.0" },  // live hot bundle (null = packaged shell)
  "pending":  null,                                     // staged, promoted on next launch
  "previous": null,                                     // last known-good, for rollback
  "trying":   null                                      // promoted-but-unconfirmed → rollback next boot
}
```

The bootstrap marks a freshly promoted bundle as `trying`; `main.js` clears it ~4 s after a clean
boot. If the app crashes before that, the next launch rolls back to `previous` (or the packaged
shell) and quarantines the bad bundle.

## Cutting a release

1. Bump `version` in `package.json`.
2. **If the release changes the native layer** (Electron upgrade, anything in `vendor/`, a new
   native dependency), also set `hotUpdate.minShellVersion` to the new version. This forces older
   installs to do a full reinstall instead of a (incompatible) hot update. JS-only releases leave
   `minShellVersion` untouched so existing installs hot-update.
3. Push to `main`. The **Build & Release** workflow:
   - builds installers for macOS / Windows / Linux (x64 + arm64),
   - runs `build:css` + `build:hotupdate` and attaches `hotupdate-manifest.json` +
     `ccbud-hotupdate-<ver>.json.gz` to the GitHub Release,
   - refreshes the Homebrew cask in the tap repo (if configured).

## Optional: sign hot-update bundles

```bash
npm run gen:updatekeys
```

- Paste the **public** key into `UPDATE_PUBLIC_KEY` in `src/main/updater.js` (this makes the client
  *require* a valid signature — ship it in a normal release so future hot updates are signed).
- Store the **private** key as the `CCBUD_UPDATE_PRIVATE_KEY` repo secret; CI signs each bundle.

With `UPDATE_PUBLIC_KEY` empty, bundles are verified by SHA-256 only.

## Homebrew tap

`brew install --cask ccbud/tap/ccbud` resolves to the `ccbud/homebrew-tap` repo's
`Casks/ccbud.rb`. To enable automatic cask updates on release, add a `HOMEBREW_TAP_TOKEN` repo
secret (a PAT with push access to the tap repo); optionally override the repo with the
`HOMEBREW_TAP_REPO` repo variable. The canonical cask lives at `homebrew/Casks/ccbud.rb`.
```
