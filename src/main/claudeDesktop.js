'use strict';

/**
 * One-click integration with the Claude Desktop app's "Third-Party Inference".
 *
 * Unlike Claude Code (a plain ~/.claude/settings.json we write directly), Claude Desktop
 * (bundle `com.anthropic.claudefordesktop`) reads its third-party-inference settings from macOS
 * *Managed Preferences*, delivered as a Configuration Profile (.mobileconfig). So:
 *
 *   connect():  generate a profile pre-filled with the local ccbud gateway, then hand it to macOS.
 *               `profiles` CLI "no longer supports installs", so the user approves it once in
 *               System Settings › Profiles (admin password). This is NOT a risky operation — it
 *               only changes where inference is sent; macOS just requires you to confirm it.
 *   disconnect(): `profiles remove -identifier …` still works, run via an admin prompt → ~one-click
 *               restore. Falls back to opening System Settings if that path is unavailable.
 *
 * Schema (extracted from Claude Desktop's own profile generator):
 *   PayloadType (inner) = com.anthropic.claudefordesktop, with managed keys:
 *   inferenceProvider="gateway", inferenceCredentialKind="static",
 *   inferenceGatewayBaseUrl, inferenceGatewayApiKey, inferenceGatewayAuthScheme="bearer".
 */

const fs = require('fs');
const path = require('path');
const os = require('os');
const crypto = require('crypto');
const { exec, execFile, execFileSync } = require('child_process');
const { CLAUDE_TIER_MODELS } = require('./claudeModels');

const BUNDLE_ID = 'com.anthropic.claudefordesktop';
const PROFILE_IDENTIFIER = 'dev.ccbud.gateway.claude-desktop-inference';
const PROFILES_PANE = 'x-apple.systempreferences:com.apple.preferences.configurationprofiles';

const isMac = () => process.platform === 'darwin';
const endpoint = (port) => `http://localhost:${port || 8788}`;
const profilePath = () =>
  path.join(os.homedir(), '.ccbud', 'claude-desktop-inference.mobileconfig');

function appInstalled() {
  if (!isMac()) return false;
  const candidates = [
    '/Applications/Claude.app',
    path.join(os.homedir(), 'Applications', 'Claude.app'),
    path.join(os.homedir(), 'Library', 'Application Support', 'Claude'),
  ];
  return candidates.some((p) => { try { return fs.existsSync(p); } catch (_) { return false; } });
}

// Stable UUIDs so re-generating the profile updates it in place rather than duplicating.
function uuidFrom(seed) {
  const h = crypto.createHash('sha1').update(String(seed)).digest('hex');
  return `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20, 32)}`.toUpperCase();
}
const xmlEsc = (s) => String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');

function buildProfile(port, token) {
  // Claude Desktop's Gateway picker needs an explicit model list, stored as a SINGLE JSON string.
  // Names carry Anthropic keywords so its client-side validation accepts them, and they match what
  // ccbud returns from /v1/models; the gateway then tier-maps each onto the active provider.
  const inferenceModels = JSON.stringify(CLAUDE_TIER_MODELS.map((m) => Object.assign(
    { name: m.name, anthropicFamilyTier: m.tier }, m.familyDefault ? { isFamilyDefault: true } : {},
  )));
  const settings = {
    inferenceProvider: 'gateway',
    inferenceCredentialKind: 'static',
    inferenceGatewayBaseUrl: endpoint(port),
    inferenceGatewayApiKey: token || 'ccbud-local',
    inferenceGatewayAuthScheme: 'bearer',
    inferenceModels,
  };
  const body = Object.entries(settings)
    .map(([k, v]) => `      <key>${k}</key>\n      <string>${xmlEsc(v)}</string>`).join('\n');
  return `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>PayloadContent</key>
  <array>
    <dict>
      <key>PayloadType</key>
      <string>${BUNDLE_ID}</string>
      <key>PayloadIdentifier</key>
      <string>${PROFILE_IDENTIFIER}.settings</string>
      <key>PayloadUUID</key>
      <string>${uuidFrom(PROFILE_IDENTIFIER + '.settings')}</string>
      <key>PayloadVersion</key>
      <integer>1</integer>
      <key>PayloadDisplayName</key>
      <string>Claude Desktop Third-Party Inference (CC Buddy)</string>
${body}
    </dict>
  </array>
  <key>PayloadDisplayName</key>
  <string>CC Buddy · Claude Desktop 第三方推理</string>
  <key>PayloadDescription</key>
  <string>将 Claude 桌面版的模型推理指向本地 CC Buddy 网关（${endpoint(port)}）。可随时移除以还原为官方推理。</string>
  <key>PayloadIdentifier</key>
  <string>${PROFILE_IDENTIFIER}</string>
  <key>PayloadOrganization</key>
  <string>CC Buddy</string>
  <key>PayloadRemovalDisallowed</key>
  <false/>
  <key>PayloadScope</key>
  <string>User</string>
  <key>PayloadType</key>
  <string>Configuration</string>
  <key>PayloadUUID</key>
  <string>${uuidFrom(PROFILE_IDENTIFIER)}</string>
  <key>PayloadVersion</key>
  <integer>1</integer>
</dict>
</plist>
`;
}

// Read the effective managed baseUrl Claude Desktop would use (mirrors how it reads managed prefs).
function managedBaseUrl() {
  let user = '';
  try { user = os.userInfo().username; } catch (_) {}
  const paths = [`/Library/Managed Preferences/${BUNDLE_ID}.plist`];
  if (user) paths.push(`/Library/Managed Preferences/${user}/${BUNDLE_ID}.plist`);
  for (const p of paths) {
    try {
      if (!fs.existsSync(p)) continue;
      const out = execFileSync('/usr/bin/plutil', ['-extract', 'inferenceGatewayBaseUrl', 'raw', '-o', '-', p], {
        encoding: 'utf8', timeout: 4000,
      }).trim();
      if (out) return out;
    } catch (_) {}
  }
  return null;
}

function status(port) {
  return {
    supported: isMac(),
    installed: appInstalled(),
    connected: isMac() && managedBaseUrl() === endpoint(port),
    endpoint: endpoint(port),
  };
}

// Write the profile and open it + the Profiles pane for the user to approve (one-time macOS step).
function connect(port, token) {
  if (!isMac()) return { ok: false, reason: 'unsupported' };
  if (!appInstalled()) return { ok: false, reason: 'notInstalled' };
  const file = profilePath();
  try {
    fs.mkdirSync(path.dirname(file), { recursive: true });
    fs.writeFileSync(file, buildProfile(port, token), 'utf8');
  } catch (e) {
    return { ok: false, reason: 'write', message: e.message };
  }
  exec(`/usr/bin/open ${JSON.stringify(file)}`, () => {
    setTimeout(() => exec(`/usr/bin/open ${JSON.stringify(PROFILES_PANE)}`, () => {}), 1200);
  });
  return { ok: true, needsApproval: true, path: file };
}

// Remove the profile via an admin prompt (~one-click); fall back to System Settings on failure.
function disconnect() {
  if (!isMac()) return Promise.resolve({ ok: false, reason: 'unsupported' });
  return new Promise((resolve) => {
    const osa = `do shell script "/usr/bin/profiles remove -identifier ${PROFILE_IDENTIFIER}" with administrator privileges`;
    execFile('/usr/bin/osascript', ['-e', osa], (err, _stdout, stderr) => {
      if (!err) { resolve({ ok: true, removed: true }); return; }
      const msg = String((stderr || '') + (err && err.message ? err.message : ''));
      if (/-128|User canceled/i.test(msg)) { resolve({ ok: false, cancelled: true }); return; }
      // CLI removal unavailable → open System Settings so the user can remove it manually.
      exec(`/usr/bin/open ${JSON.stringify(PROFILES_PANE)}`, () => {});
      resolve({ ok: true, removed: false, needsApproval: true });
    });
  });
}

module.exports = {
  appInstalled, status, connect, disconnect, buildProfile, profilePath,
  BUNDLE_ID, PROFILE_IDENTIFIER,
};
