// Claude Desktop "Third-Party Inference" integration — Rust port of claudeDesktop.js.
//
// Claude Desktop reads inference settings from macOS Managed Preferences, delivered as a
// Configuration Profile (.mobileconfig). connect() generates the profile pre-filled with the
// local gateway and opens it for the user to approve; disconnect() removes it via an admin
// prompt. The profile advertises the claude-* tier models (matching /v1/models) so a fresh
// Claude Desktop can pick a model and drive the gateway with zero per-user setup.

#![allow(dead_code)]

use crate::gateway::CLAUDE_TIER_MODELS;
use serde_json::{json, Value};
use std::path::PathBuf;

const BUNDLE_ID: &str = "com.anthropic.claudefordesktop";
const PROFILE_IDENTIFIER: &str = "dev.ccbud.gateway.claude-desktop-inference";
const PROFILES_PANE: &str =
    "x-apple.systempreferences:com.apple.preferences.configurationprofiles";

fn home() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("."))
}
fn is_mac() -> bool {
    cfg!(target_os = "macos")
}
fn endpoint(port: u16) -> String {
    format!("http://localhost:{}", port)
}
pub fn profile_path() -> PathBuf {
    home().join(".ccbud").join("claude-desktop-inference.mobileconfig")
}

fn uuid_from(seed: &str) -> String {
    use sha1::{Digest, Sha1};
    let mut h = Sha1::new();
    h.update(seed.as_bytes());
    let hex = format!("{:x}", h.finalize());
    format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32]).to_uppercase()
}
fn xml_esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn app_installed() -> bool {
    if !is_mac() {
        return false;
    }
    [
        PathBuf::from("/Applications/Claude.app"),
        home().join("Applications").join("Claude.app"),
        home().join("Library").join("Application Support").join("Claude"),
    ]
    .iter()
    .any(|p| p.exists())
}

pub fn build_profile(port: u16, token: &str) -> String {
    // Gateway picker needs an explicit model list as a SINGLE JSON string; names carry Anthropic
    // keywords (so its validation accepts them) and match what /v1/models returns.
    let models: Vec<Value> = CLAUDE_TIER_MODELS
        .iter()
        .map(|(name, tier)| {
            let mut m = serde_json::Map::new();
            m.insert("name".into(), json!(name));
            m.insert("anthropicFamilyTier".into(), json!(tier));
            if *tier == "sonnet" {
                m.insert("isFamilyDefault".into(), json!(true));
            }
            Value::Object(m)
        })
        .collect();
    let inference_models = serde_json::to_string(&models).unwrap_or_default();

    let settings = [
        ("inferenceProvider", "gateway".to_string()),
        ("inferenceCredentialKind", "static".to_string()),
        ("inferenceGatewayBaseUrl", endpoint(port)),
        ("inferenceGatewayApiKey", if token.is_empty() { "ccbud-local".to_string() } else { token.to_string() }),
        ("inferenceGatewayAuthScheme", "bearer".to_string()),
        ("inferenceModels", inference_models),
    ];
    let body = settings
        .iter()
        .map(|(k, v)| format!("      <key>{}</key>\n      <string>{}</string>", k, xml_esc(v)))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>PayloadContent</key>
  <array>
    <dict>
      <key>PayloadType</key>
      <string>{bundle}</string>
      <key>PayloadIdentifier</key>
      <string>{ident}.settings</string>
      <key>PayloadUUID</key>
      <string>{uuid_settings}</string>
      <key>PayloadVersion</key>
      <integer>1</integer>
      <key>PayloadDisplayName</key>
      <string>Claude Desktop Third-Party Inference (CC Buddy)</string>
{body}
    </dict>
  </array>
  <key>PayloadDisplayName</key>
  <string>CC Buddy · Claude Desktop 第三方推理</string>
  <key>PayloadDescription</key>
  <string>将 Claude 桌面版的模型推理指向本地 CC Buddy 网关（{ep}）。可随时移除以还原为官方推理。</string>
  <key>PayloadIdentifier</key>
  <string>{ident}</string>
  <key>PayloadOrganization</key>
  <string>CC Buddy</string>
  <key>PayloadRemovalDisallowed</key>
  <false/>
  <key>PayloadScope</key>
  <string>User</string>
  <key>PayloadType</key>
  <string>Configuration</string>
  <key>PayloadUUID</key>
  <string>{uuid_root}</string>
  <key>PayloadVersion</key>
  <integer>1</integer>
</dict>
</plist>
"#,
        bundle = BUNDLE_ID,
        ident = PROFILE_IDENTIFIER,
        uuid_settings = uuid_from(&format!("{}.settings", PROFILE_IDENTIFIER)),
        uuid_root = uuid_from(PROFILE_IDENTIFIER),
        body = body,
        ep = endpoint(port),
    )
}

fn managed_base_url() -> Option<String> {
    if !is_mac() {
        return None;
    }
    let user = std::env::var("USER").unwrap_or_default();
    let mut paths = vec![format!("/Library/Managed Preferences/{}.plist", BUNDLE_ID)];
    if !user.is_empty() {
        paths.push(format!("/Library/Managed Preferences/{}/{}.plist", user, BUNDLE_ID));
    }
    for p in paths {
        if !std::path::Path::new(&p).exists() {
            continue;
        }
        if let Ok(out) = std::process::Command::new("/usr/bin/plutil")
            .args(["-extract", "inferenceGatewayBaseUrl", "raw", "-o", "-", &p])
            .output()
        {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    None
}

pub fn status(port: u16) -> Value {
    json!({
        "supported": is_mac(),
        "installed": app_installed(),
        "connected": is_mac() && managed_base_url().as_deref() == Some(endpoint(port).as_str()),
        "endpoint": endpoint(port),
    })
}

pub fn connect(port: u16, token: &str) -> Value {
    if !is_mac() {
        return json!({ "ok": false, "reason": "unsupported" });
    }
    if !app_installed() {
        return json!({ "ok": false, "reason": "notInstalled" });
    }
    let file = profile_path();
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if std::fs::write(&file, build_profile(port, token)).is_err() {
        return json!({ "ok": false, "reason": "write" });
    }
    let f = file.to_string_lossy().to_string();
    let _ = std::process::Command::new("/usr/bin/open").arg(&f).spawn();
    // Take the user to System Settings › Profiles shortly after, so they can approve it (matches
    // claudeDesktop.js — without this many users get stuck not knowing where to approve).
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(1200));
        let _ = std::process::Command::new("/usr/bin/open").arg(PROFILES_PANE).spawn();
    });
    json!({ "ok": true, "needsApproval": true, "path": f })
}

pub fn disconnect() -> Value {
    if !is_mac() {
        return json!({ "ok": false, "reason": "unsupported" });
    }
    let osa = format!(
        "do shell script \"/usr/bin/profiles remove -identifier {}\" with administrator privileges",
        PROFILE_IDENTIFIER
    );
    match std::process::Command::new("/usr/bin/osascript").args(["-e", &osa]).output() {
        Ok(o) if o.status.success() => json!({ "ok": true, "removed": true }),
        Ok(o) => {
            // User canceled the admin-password prompt (-128 / "User canceled") → report it as
            // cancelled instead of pretending it still needs approval. Otherwise (CLI unavailable)
            // fall back to opening System Settings so the user can remove it manually.
            let stderr = String::from_utf8_lossy(&o.stderr);
            if stderr.contains("-128") || stderr.to_lowercase().contains("user canceled") {
                json!({ "ok": false, "cancelled": true })
            } else {
                let _ = std::process::Command::new("/usr/bin/open").arg(PROFILES_PANE).spawn();
                json!({ "ok": true, "removed": false, "needsApproval": true })
            }
        }
        Err(_) => {
            let _ = std::process::Command::new("/usr/bin/open").arg(PROFILES_PANE).spawn();
            json!({ "ok": true, "removed": false, "needsApproval": true })
        }
    }
}
