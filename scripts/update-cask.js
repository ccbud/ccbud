#!/usr/bin/env node
'use strict';

/*
 * Regenerates the Homebrew cask (homebrew/Casks/ccbud.rb) for the current package.json version.
 * If the per-arch .dmg files are available it pins their SHA-256; otherwise it falls back to
 * `sha256 :no_check` (still installable, just unverified).
 *
 * Usage:
 *   node scripts/update-cask.js [dmgDir] [outFile]
 *     dmgDir  : directory holding CC.Buddy_<version>_aarch64.dmg / _x64.dmg (default: dist)
 *     outFile : cask path to write (default: homebrew/Casks/ccbud.rb)
 *
 * In CI this runs after the release is published, then the cask is pushed to the tap repo.
 */

const fs = require('fs');
const path = require('path');
const crypto = require('crypto');

const ROOT = path.resolve(__dirname, '..');
const pkg = JSON.parse(fs.readFileSync(path.join(ROOT, 'package.json'), 'utf8'));
const version = pkg.version;

const dmgDir = process.argv[2] ? path.resolve(process.argv[2]) : path.join(ROOT, 'dist');
const outFile = process.argv[3] ? path.resolve(process.argv[3]) : path.join(ROOT, 'homebrew', 'Casks', 'ccbud.rb');

function sha256Of(file) {
  try { return crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex'); }
  catch (_) { return null; }
}

// Tauri names the dmgs "CC Buddy_<version>_<arch>.dmg" (arch aarch64 / x64); GitHub release
// assets replace the space with a period, so downloaded copies are CC.Buddy_<version>_<arch>.dmg.
const dmgSha = (arch) =>
  sha256Of(path.join(dmgDir, `CC.Buddy_${version}_${arch}.dmg`)) ||
  sha256Of(path.join(dmgDir, `CC Buddy_${version}_${arch}.dmg`));
const arm = dmgSha('aarch64');
const intel = dmgSha('x64');

let shaStanza;
if (arm && intel) {
  shaStanza = `  sha256 arm:   "${arm}",\n         intel: "${intel}"`;
  console.log('[update-cask] pinned per-arch sha256');
} else {
  shaStanza = '  sha256 :no_check';
  console.log('[update-cask] dmgs not found in', dmgDir, '— using :no_check');
}

const cask = `cask "ccbud" do
  arch arm: "aarch64", intel: "x64"

  version "${version}"
${shaStanza}

  url "https://github.com/ccbud/ccbud/releases/download/v#{version}/CC.Buddy_#{version}_#{arch}.dmg",
      verified: "github.com/ccbud/ccbud/"
  name "CC Buddy"
  desc "CC Buddy — Claude Code gateway plus Claude Code/Codex session browser"
  homepage "https://github.com/ccbud/ccbud"

  # CC Buddy can update itself in-app; Homebrew handles normal cask upgrades.
  auto_updates true
  depends_on macos: :big_sur

  app "CC Buddy.app"

  zap trash: [
    "~/Library/Application Support/ccbud",
    "~/Library/Preferences/dev.ccbud.gateway.plist",
    "~/Library/Saved Application State/dev.ccbud.gateway.savedState",
    "~/Library/Logs/ccbud",
  ]
end
`;

fs.mkdirSync(path.dirname(outFile), { recursive: true });
fs.writeFileSync(outFile, cask);
console.log('[update-cask] wrote', path.relative(ROOT, outFile), 'for v' + version);
