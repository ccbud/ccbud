// ccbud Tauri backend.
//
// Every IPC command the renderer calls is registered here. config/provider commands are
// now backed by real persistence (store.rs → ~/.ccbud/config.json); the rest are still
// STUBS returning sensible empty shapes, filled in over later phases:
//   Phase 2 — gateway (proxy.js → Rust)        Phase 4 — system (window/tray/dialogs/claude)
//   Phase 3 — history/usage/export (in progress) Phase 5 — auto-update (tauri-plugin-updater)
//
// Stub params keep their real names (Tauri maps JS invoke args by name), so unused-var
// warnings are suppressed crate-wide until the bodies are filled in.
#![allow(unused_variables)]

mod gateway;
mod history;
mod store;

use serde_json::{json, Value};
use tauri::Manager;

// ---- config / providers (real, store.rs) ----
#[tauri::command]
fn config_get() -> Value {
    store::read_config()
}
#[tauri::command]
fn config_save(cfg: Value) -> Value {
    store::write_config(cfg)
}
#[tauri::command]
fn provider_upsert(p: Value) -> Value {
    let mut cfg = store::read_config();
    let mut provider = p;
    let pid = provider.get("id").and_then(|v| v.as_str()).map(|s| s.to_string());
    {
        let provs = cfg["providers"].as_array_mut().unwrap();
        match pid {
            Some(id) if !id.is_empty() => {
                if let Some(i) = provs
                    .iter()
                    .position(|x| x.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
                {
                    provs[i] = provider;
                } else {
                    provs.push(provider);
                }
            }
            _ => {
                let id = store::gen_id();
                provider
                    .as_object_mut()
                    .unwrap()
                    .insert("id".into(), json!(id.clone()));
                provs.push(provider);
                if cfg["activeProviderId"].is_null() {
                    cfg["activeProviderId"] = json!(id);
                }
            }
        }
    }
    store::write_config(cfg)
}
#[tauri::command]
fn provider_delete(id: String) -> Value {
    let mut cfg = store::read_config();
    let kept: Vec<Value> = cfg["providers"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|p| p.get("id").and_then(|v| v.as_str()) != Some(id.as_str()))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    cfg["providers"] = json!(kept);
    if cfg["activeProviderId"].as_str() == Some(id.as_str()) {
        cfg["activeProviderId"] = cfg["providers"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(|p| p.get("id").cloned())
            .unwrap_or(Value::Null);
    }
    store::write_config(cfg)
}
#[tauri::command]
fn provider_set_active(id: String) -> Value {
    let mut cfg = store::read_config();
    cfg["activeProviderId"] = json!(id);
    store::write_config(cfg)
}
#[tauri::command]
fn provider_test(p: Value) -> Value {
    json!({ "ok": false, "error": "not implemented yet" })
}

// ---- claude code / desktop integration (Phase 4) ----
#[tauri::command] fn claude_connect() -> Value { Value::Null }
#[tauri::command] fn claude_disconnect() -> Value { Value::Null }
#[tauri::command] fn desktop_status() -> Value { json!({ "supported": true, "connected": false, "profileInstalled": false }) }
#[tauri::command] fn desktop_connect() -> Value { Value::Null }
#[tauri::command] fn desktop_disconnect() -> Value { Value::Null }
#[tauri::command] fn desktop_replay(file: String) -> Value { Value::Null }

// ---- server / usage / monitor / logs (Phase 2/3) ----
#[tauri::command]
async fn server_status(
    gw: tauri::State<'_, std::sync::Arc<gateway::GatewayState>>,
) -> Result<Value, String> {
    let mut s = gw.status().await;
    if let Some(o) = s.as_object_mut() {
        o.insert("connected".into(), json!(false));
        o.insert("lastStartError".into(), Value::Null);
        o.insert("claudePath".into(), json!(""));
    }
    Ok(s)
}
#[tauri::command] fn usage_get(range: Option<String>) -> Value { json!({ "tokens": 0, "requests": 0, "favoriteModel": "—", "heatmap": [], "days": [], "models": [] }) }
#[tauri::command] fn monitor_get(id: Value) -> Value { Value::Null }
#[tauri::command] fn monitor_clear() -> Value { Value::Null }
#[tauri::command] fn logs_get() -> Value { json!([]) }
#[tauri::command] fn logs_clear() -> Value { Value::Null }

// ---- window / app lifecycle (Phase 4) ----
#[tauri::command] fn app_open_main() -> Value { Value::Null }
#[tauri::command] fn app_quit() -> Value { Value::Null }
#[tauri::command] fn window_settings_mode(on: bool) -> Value { Value::Null }
#[tauri::command] fn window_view_min_width(w: i64) -> Value { Value::Null }

// ---- conversation history (Phase 3) ----
#[tauri::command]
fn history_projects() -> Value {
    let cfg = store::read_config();
    let active = cfg.get("historyActive").and_then(|v| v.as_str()).unwrap_or("all").to_string();
    json!(history::list_projects(&cfg, &active))
}
#[tauri::command]
fn history_list() -> Value {
    let cfg = store::read_config();
    let active = cfg.get("historyActive").and_then(|v| v.as_str()).unwrap_or("all").to_string();
    json!(history::list_sessions(&cfg, &active, 400))
}
#[tauri::command]
fn history_get(file: String) -> Value {
    history::get_session(&file)
}
#[tauri::command]
fn history_dirs() -> Value {
    let cfg = store::read_config();
    let active = cfg.get("historyActive").and_then(|v| v.as_str()).unwrap_or("all").to_string();
    json!({ "dirs": history::dir_stats(&cfg), "active": active })
}
#[tauri::command] fn history_pick_dir() -> Value { Value::Null }
#[tauri::command] fn history_set_active(id: String) -> Value { Value::Null }
#[tauri::command] fn history_import() -> Value { Value::Null }
#[tauri::command] fn history_import_paths(paths: Value) -> Value { Value::Null }
#[tauri::command] fn history_remove_import(file: String) -> Value { Value::Null }
#[tauri::command] fn history_set_meta(file: String, patch: Value) -> Value { Value::Null }
#[tauri::command] fn history_export_raw(file: String) -> Value { json!("") }
#[tauri::command] fn history_export_html(payload: Value) -> Value { json!("") }

// ---- utilities (Phase 4) ----
#[tauri::command] fn util_copy(text: String) -> Value { Value::Null }
#[tauri::command] fn util_open_external(url: String) -> Value { Value::Null }

// ---- in-app updates (Phase 5) ----
#[tauri::command] fn update_state() -> Value { json!({ "current": "1.0.18", "status": "idle", "available": null }) }
#[tauri::command] fn update_check() -> Value { Value::Null }
#[tauri::command] fn update_download() -> Value { Value::Null }
#[tauri::command] fn update_apply() -> Value { Value::Null }
#[tauri::command] fn update_set_auto(patch: Value) -> Value { json!({ "check": true, "autoDownload": false }) }

// ---- debug self-check (gated by CCBUD_SELFCHECK env; injected via on_page_load) ----
#[tauri::command]
fn selfcheck_report(report: Value) {
    eprintln!("[SELFCHECK] {}", serde_json::to_string(&report).unwrap_or_default());
}
#[tauri::command]
fn selfcheck_routing() -> Value {
    gateway::routing_selftest()
}
#[tauri::command]
async fn selfcheck_gateway(
    gw: tauri::State<'_, std::sync::Arc<gateway::GatewayState>>,
) -> Result<Value, String> {
    // Mutates config (writes a mock provider) — only ever allowed in a throwaway self-check run.
    if std::env::var("CCBUD_SELFCHECK").is_err() {
        return Err("selfcheck disabled".into());
    }
    let port = gw.current_port().await.unwrap_or(0);
    Ok(gateway::gateway_selftest(port).await)
}
const SELFCHECK_JS: &str = r#"
(function(){
  if (window.__ccbud_sc) return; window.__ccbud_sc = 1;
  window.__ccbud_errors = [];
  window.addEventListener('error', function(e){ try{window.__ccbud_errors.push(String((e&&e.message)||(e&&e.error)||e));}catch(_){} }, true);
  window.addEventListener('unhandledrejection', function(e){ try{window.__ccbud_errors.push('promise:'+String((e.reason&&e.reason.message)||e.reason));}catch(_){} });
  function rep(o){ try{ window.__TAURI__.core.invoke('selfcheck_report',{report:o}); }catch(_){} }
  setTimeout(async function(){
    var o={};
    try{
      o.hasCcbud=!!window.ccbud;
      o.hasTauri=!!(window.__TAURI__&&window.__TAURI__.core);
      o.bodyLen=(document.body&&document.body.innerHTML.length)||0;
      o.navItems=document.querySelectorAll('.nav-item,[data-view],[data-nav]').length;
      o.colorMix=!!(window.CSS&&CSS.supports&&CSS.supports('color','color-mix(in srgb,red,blue)'));
      o.highlight=!!(window.CSS&&CSS.highlights);
      // store round-trip — self-check runs point CCBUD_HOME at a throwaway dir
      try{
        var before=await window.ccbud.getConfig();
        o.provBefore=((before&&before.providers)||[]).length;
        var saved=await window.ccbud.upsertProvider({name:'SelfTest',baseUrl:'https://x.test',authToken:'tok',defaultModel:'m1',smallFastModel:'m1',extra:'shouldDrop'});
        o.provAfter=((saved&&saved.providers)||[]).length;
        o.savedName=saved&&saved.providers&&saved.providers[0]&&saved.providers[0].name;
        o.savedHasId=!!(saved&&saved.providers&&saved.providers[0]&&saved.providers[0].id);
        o.savedActiveMatches=!!(saved&&saved.activeProviderId&&saved.providers[0]&&saved.activeProviderId===saved.providers[0].id);
        o.droppedExtra=!(saved&&saved.providers&&saved.providers[0]&&('extra' in saved.providers[0]));
        var reread=await window.ccbud.getConfig();
        o.rereadProv=((reread&&reread.providers)||[]).length;
      }catch(e){ o.storeErr=String(e); }
      try{ o.routing=await window.__TAURI__.core.invoke('selfcheck_routing'); }catch(e){ o.routingErr=String(e); }
      try{ o.server=await window.ccbud.serverStatus(); }catch(e){ o.serverErr=String(e); }
      try{ o.gateway=await window.__TAURI__.core.invoke('selfcheck_gateway'); }catch(e){ o.gatewayErr=String(e); }
      try{
        o.histDirs=(await window.ccbud.historyDirs()).dirs.length;
        var hl=await window.ccbud.historyList();
        o.histCount=(hl||[]).length;
        o.histSample=hl&&hl[0]?{title:String(hl[0].title||'').slice(0,40),project:hl[0].project,hasCwd:!!hl[0].cwd,hasFile:!!hl[0].file}:null;
        if(hl&&hl[0]){ var ss=await window.ccbud.historyGet(hl[0].file); o.histMsgs=ss&&ss.messages?ss.messages.length:-1; o.histTotals=ss&&ss.meta?ss.meta.totals:null; }
      }catch(e){ o.histErr=String(e); }
      o.errors=window.__ccbud_errors.slice(0,20);
    }catch(e){o.fatal=String((e&&e.stack)||e);}
    rep(o);
  },2200);
})();
"#;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .on_page_load(|webview, payload| {
            if matches!(payload.event(), tauri::webview::PageLoadEvent::Finished)
                && std::env::var("CCBUD_SELFCHECK").is_ok()
            {
                let _ = webview.eval(SELFCHECK_JS);
            }
        })
        .setup(|app| {
            if cfg!(debug_assertions) {
                app.handle().plugin(
                    tauri_plugin_log::Builder::default()
                        .level(log::LevelFilter::Info)
                        .build(),
                )?;
            }
            // Start the localhost gateway on the configured port (proxy.js parity).
            let gw = gateway::GatewayState::new(app.handle().clone());
            app.manage(gw.clone());
            let port = store::read_config()
                .get("port")
                .and_then(|v| v.as_u64())
                .unwrap_or(8788) as u16;
            tauri::async_runtime::spawn(async move {
                if let Err(e) = gw.start(port).await {
                    eprintln!("[ccbud] gateway start failed: {}", e);
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            config_get, config_save, provider_upsert, provider_delete, provider_set_active, provider_test,
            claude_connect, claude_disconnect, desktop_status, desktop_connect, desktop_disconnect, desktop_replay,
            server_status, usage_get, monitor_get, monitor_clear, logs_get, logs_clear,
            app_open_main, app_quit, window_settings_mode, window_view_min_width,
            history_projects, history_list, history_get, history_dirs, history_pick_dir, history_set_active,
            history_import, history_import_paths, history_remove_import, history_set_meta, history_export_raw, history_export_html,
            util_copy, util_open_external,
            update_state, update_check, update_download, update_apply, update_set_auto,
            selfcheck_report, selfcheck_routing, selfcheck_gateway
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
