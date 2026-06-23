#!/usr/bin/env node
'use strict';

/*
 * Regenerates the Homebrew cask (homebrew/Casks/ccbud.rb) for the current package.json version.
 * If the per-arch .dmg files are available it pins their SHA-256; otherwise it falls back to
 * `sha256 :no_check` (still installable, just unverified).
 *
 * Usage:
 *   node scripts/update-cask.js [dmgDir] [outFile]
 *     dmgDir  : directory holding ccbud-<version>-mac-arm64.dmg / -x64.dmg (default: dist)
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

const arm = sha256Of(path.join(dmgDir, `ccbud-${version}-mac-arm64.dmg`));
const intel = sha256Of(path.join(dmgDir, `ccbud-${version}-mac-x64.dmg`));

let shaStanza;
if (arm && intel) {
  shaStanza = `  sha256 arm:   "${arm}",\n         intel: "${intel}"`;
  console.log('[update-cask] pinned per-arch sha256');
} else {
  shaStanza = '  sha256 :no_check';
  console.log('[update-cask] dmgs not found in', dmgDir, '— using :no_check');
}

const cask = `cask "ccbud" do
  arch arm: "arm64", intel: "x64"

  version "${version}"
${shaStanza}

  url "https://github.com/ccbud/ccbud/releases/download/v#{version}/ccbud-#{version}-mac-#{arch}.dmg",
      verified: "github.com/ccbud/ccbud/"
  name "ccbud"
  desc "Claude Code Gateway — proxy Claude Code to any Anthropic-compatible provider"
  homepage "https://github.com/ccbud/ccbud"

  # ccbud applies JS-only releases in place (in-app hot update); \`brew upgrade\` handles the rest.
  auto_updates true
  depends_on macos: :big_sur

  app "ccbud.app"

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
