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

mod claude;
mod claudedesktop;
mod counttokens;
mod exporthtml;
mod gateway;
mod history;
mod store;
mod usage;

use serde_json::{json, Value};
use tauri::{Emitter, Manager};

// ---- config / providers (real, store.rs) ----
#[tauri::command]
fn config_get() -> Value {
    store::read_config()
}
#[tauri::command]
fn config_save(app: tauri::AppHandle, cfg: Value) -> Value {
    let saved = store::write_config(cfg);
    // Apply the open-at-login preference via the autostart plugin.
    use tauri_plugin_autostart::ManagerExt;
    let want = saved.get("openAtLogin").and_then(|v| v.as_bool()).unwrap_or(false);
    let mgr = app.autolaunch();
    let _ = if want { mgr.enable() } else { mgr.disable() };
    saved
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
#[tauri::command]
async fn claude_connect(
    gw: tauri::State<'_, std::sync::Arc<gateway::GatewayState>>,
) -> Result<Value, String> {
    let cfg = store::read_config();
    let n = cfg.get("providers").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
    if n == 0 {
        return Ok(json!({ "ok": false, "message": "no provider configured" }));
    }
    let port = cfg.get("port").and_then(|v| v.as_u64()).unwrap_or(8788) as u16;
    if let Err(e) = gw.start(port).await {
        return Ok(json!({ "ok": false, "message": format!("port {} failed: {}", port, e) }));
    }
    claude::connect(port, &claude::current_token(&cfg));
    let status = full_status(&gw).await;
    gw.emit("gateway:status", status);
    Ok(json!({ "ok": true }))
}
#[tauri::command]
async fn claude_disconnect(
    gw: tauri::State<'_, std::sync::Arc<gateway::GatewayState>>,
) -> Result<Value, String> {
    claude::disconnect();
    gw.stop().await;
    let status = full_status(&gw).await;
    gw.emit("gateway:status", status);
    Ok(json!({ "ok": true }))
}
#[tauri::command]
async fn desktop_status(
    gw: tauri::State<'_, std::sync::Arc<gateway::GatewayState>>,
) -> Result<Value, String> {
    let port = gw
        .current_port()
        .await
        .unwrap_or_else(|| store::read_config().get("port").and_then(|v| v.as_u64()).unwrap_or(8788) as u16);
    Ok(claudedesktop::status(port))
}
#[tauri::command]
async fn desktop_connect(
    gw: tauri::State<'_, std::sync::Arc<gateway::GatewayState>>,
) -> Result<Value, String> {
    let cfg = store::read_config();
    let port = cfg.get("port").and_then(|v| v.as_u64()).unwrap_or(8788) as u16;
    if cfg.get("providers").and_then(|v| v.as_array()).map(|a| a.is_empty()).unwrap_or(true) {
        return Ok(json!({ "ok": false, "reason": "noProvider" }));
    }
    let _ = gw.start(port).await;
    Ok(claudedesktop::connect(port, &claude::current_token(&cfg)))
}
#[tauri::command]
fn desktop_disconnect() -> Value {
    claudedesktop::disconnect()
}
#[tauri::command] fn desktop_replay(file: String) -> Value { Value::Null }

// ---- server / usage / monitor / logs (Phase 2/3) ----
async fn full_status(gw: &std::sync::Arc<gateway::GatewayState>) -> Value {
    let mut s = gw.status().await;
    let port = gw
        .current_port()
        .await
        .unwrap_or_else(|| store::read_config().get("port").and_then(|v| v.as_u64()).unwrap_or(8788) as u16);
    if let Some(o) = s.as_object_mut() {
        o.insert("connected".into(), json!(claude::is_connected(port)));
        o.insert("lastStartError".into(), Value::Null);
        o.insert("claudePath".into(), json!(claude::settings_path().to_string_lossy()));
    }
    s
}
#[tauri::command]
async fn server_status(
    gw: tauri::State<'_, std::sync::Arc<gateway::GatewayState>>,
) -> Result<Value, String> {
    Ok(full_status(&gw).await)
}
#[tauri::command]
fn usage_get(range: Option<String>) -> Value {
    let cfg = store::read_config();
    let active = cfg.get("historyActive").and_then(|v| v.as_str()).unwrap_or("all").to_string();
    usage::usage_get(&cfg, &active, range.as_deref().unwrap_or("7d"))
}
#[tauri::command]
async fn monitor_get(
    gw: tauri::State<'_, std::sync::Arc<gateway::GatewayState>>,
    id: Value,
) -> Result<Value, String> {
    let idn = id.as_i64().or_else(|| id.as_str().and_then(|s| s.parse().ok())).unwrap_or(-1);
    Ok(gw.monitor_get(idn).await)
}
#[tauri::command]
async fn monitor_clear(
    gw: tauri::State<'_, std::sync::Arc<gateway::GatewayState>>,
) -> Result<Value, String> {
    gw.monitor_clear().await;
    Ok(json!(true))
}
#[tauri::command] fn logs_get() -> Value { json!([]) }
#[tauri::command] fn logs_clear() -> Value { Value::Null }

// ---- window / app lifecycle (Phase 4) ----
#[tauri::command]
fn app_open_main(app: tauri::AppHandle) -> Value {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
    Value::Null
}
#[tauri::command]
fn app_quit(app: tauri::AppHandle) -> Value {
    app.exit(0);
    Value::Null
}
#[tauri::command] fn window_settings_mode(on: bool) -> Value { Value::Null }
#[tauri::command]
fn window_view_min_width(app: tauri::AppHandle, w: i64) -> Value {
    if let Some(win) = app.get_webview_window("main") {
        let min_w = std::cmp::max(600, if w > 0 { w } else { 900 }) as f64;
        let _ = win.set_min_size(Some(tauri::Size::Logical(tauri::LogicalSize::new(min_w, 600.0))));
    }
    Value::Null
}

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
#[tauri::command]
async fn history_pick_dir(app: tauri::AppHandle) -> Result<Value, String> {
    let folder = rfd::AsyncFileDialog::new().set_title("选择 Claude 配置目录").pick_folder().await;
    match folder {
        Some(f) => {
            let dir = f.path().to_string_lossy().to_string();
            let mut cfg = store::read_config();
            let mut dirs: Vec<Value> = cfg.get("historyDirs").and_then(|v| v.as_array()).cloned().unwrap_or_default();
            if !dirs.iter().any(|d| d.as_str() == Some(dir.as_str())) {
                dirs.push(json!(dir));
            }
            cfg["historyDirs"] = json!(dirs);
            let saved = store::write_config(cfg);
            let _ = app.emit("history:changed", json!({ "files": [] }));
            Ok(json!({ "ok": true, "dir": dir, "config": saved }))
        }
        None => Ok(json!({ "ok": false, "canceled": true })),
    }
}
#[tauri::command]
fn history_set_active(app: tauri::AppHandle, id: String) -> Value {
    let mut cfg = store::read_config();
    cfg["historyActive"] = json!(if id.is_empty() { "all".to_string() } else { id });
    let saved = store::write_config(cfg);
    let _ = app.emit(
        "history:changed",
        json!({ "files": [], "active": saved.get("historyActive").cloned().unwrap_or(json!("all")) }),
    );
    saved
}
#[tauri::command]
async fn history_import(app: tauri::AppHandle) -> Result<Value, String> {
    match rfd::AsyncFileDialog::new().add_filter("JSONL", &["jsonl"]).set_title("导入对话记录").pick_files().await {
        Some(files) => {
            let paths: Vec<String> = files.iter().map(|f| f.path().to_string_lossy().to_string()).collect();
            let r = history::import_paths(&paths);
            let _ = app.emit("history:changed", json!({ "files": [] }));
            Ok(r)
        }
        None => Ok(json!({ "canceled": true })),
    }
}
#[tauri::command]
fn history_import_paths(app: tauri::AppHandle, paths: Value) -> Value {
    let list: Vec<String> = paths
        .as_array()
        .map(|a| a.iter().filter_map(|p| p.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
    let r = history::import_paths(&list);
    let _ = app.emit("history:changed", json!({ "files": [] }));
    r
}
#[tauri::command]
fn history_remove_import(app: tauri::AppHandle, file: String) -> Value {
    let r = history::remove_import(&file);
    if r.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        let _ = app.emit("history:changed", json!({ "files": [] }));
    }
    r
}
#[tauri::command]
fn history_set_meta(app: tauri::AppHandle, file: String, patch: Value) -> Value {
    let cfg = store::read_config();
    let r = history::set_ccbud(&file, &patch, &cfg);
    if r.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        let _ = app.emit("history:changed", json!({ "files": [file] }));
    }
    r
}
#[tauri::command]
async fn history_export_raw(file: String) -> Result<Value, String> {
    let data = std::fs::read_to_string(&file).map_err(|e| e.to_string())?;
    let base = std::path::Path::new(&file).file_stem().and_then(|s| s.to_str()).unwrap_or("export").to_string();
    match rfd::AsyncFileDialog::new().set_file_name(format!("{}.jsonl", base)).save_file().await {
        Some(d) => {
            let p = d.path().to_path_buf();
            std::fs::write(&p, data).map_err(|e| e.to_string())?;
            Ok(json!({ "canceled": false, "filePath": p.to_string_lossy() }))
        }
        None => Ok(json!({ "canceled": true })),
    }
}
#[tauri::command]
async fn history_export_html(payload: Value) -> Result<Value, String> {
    let file = payload
        .get("file")
        .and_then(|v| v.as_str())
        .or_else(|| payload.as_str())
        .ok_or("no file")?
        .to_string();
    let html = exporthtml::build_export_html(&file);
    let base = std::path::Path::new(&file).file_stem().and_then(|s| s.to_str()).unwrap_or("export").to_string();
    match rfd::AsyncFileDialog::new().set_file_name(format!("{}.html", base)).save_file().await {
        Some(d) => {
            let p = d.path().to_path_buf();
            std::fs::write(&p, html).map_err(|e| e.to_string())?;
            Ok(json!({ "canceled": false, "filePath": p.to_string_lossy() }))
        }
        None => Ok(json!({ "canceled": true })),
    }
}

// ---- utilities (Phase 4) ----
#[tauri::command]
fn util_copy(text: String) -> bool {
    match arboard::Clipboard::new() {
        Ok(mut cb) => cb.set_text(text).is_ok(),
        Err(_) => false,
    }
}
#[tauri::command]
fn util_open_external(url: String) -> bool {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return false;
    }
    let spawned = {
        #[cfg(target_os = "macos")]
        {
            std::process::Command::new("open").arg(&url).spawn()
        }
        #[cfg(target_os = "windows")]
        {
            std::process::Command::new("cmd").args(["/C", "start", "", &url]).spawn()
        }
        #[cfg(target_os = "linux")]
        {
            std::process::Command::new("xdg-open").arg(&url).spawn()
        }
    };
    spawned.is_ok()
}

// ---- in-app updates (Phase 5) ----
#[tauri::command]
fn update_state(app: tauri::AppHandle) -> Value {
    let cfg = store::read_config();
    json!({
        "current": app.package_info().version.to_string(),
        "status": "idle",
        "available": Value::Null,
        "autoUpdate": cfg.get("autoUpdate").cloned().unwrap_or(json!({ "check": true, "autoDownload": true })),
    })
}
#[tauri::command]
async fn update_check(app: tauri::AppHandle) -> Result<Value, String> {
    use tauri_plugin_updater::UpdaterExt;
    match app.updater() {
        Ok(updater) => match updater.check().await {
            Ok(Some(u)) => Ok(json!({ "available": true, "version": u.version, "notes": u.body })),
            Ok(None) => Ok(json!({ "available": false })),
            Err(e) => Ok(json!({ "available": false, "error": e.to_string() })),
        },
        Err(e) => Ok(json!({ "available": false, "error": e.to_string() })),
    }
}
#[tauri::command]
async fn update_download(app: tauri::AppHandle) -> Result<Value, String> {
    use tauri_plugin_updater::UpdaterExt;
    let updater = app.updater().map_err(|e| e.to_string())?;
    match updater.check().await.map_err(|e| e.to_string())? {
        Some(u) => {
            u.download_and_install(|_chunk, _total| {}, || {}).await.map_err(|e| e.to_string())?;
            Ok(json!({ "ok": true, "staged": true }))
        }
        None => Ok(json!({ "ok": false, "reason": "no update" })),
    }
}
#[tauri::command]
fn update_apply(app: tauri::AppHandle) -> Value {
    app.restart();
}
#[tauri::command]
fn update_set_auto(patch: Value) -> Value {
    let mut cfg = store::read_config();
    let mut au = cfg.get("autoUpdate").cloned().unwrap_or(json!({ "check": true, "autoDownload": true }));
    if let Some(o) = au.as_object_mut() {
        if let Some(c) = patch.get("check") {
            o.insert("check".into(), c.clone());
        }
        if let Some(d) = patch.get("autoDownload") {
            o.insert("autoDownload".into(), d.clone());
        }
    }
    cfg["autoUpdate"] = au.clone();
    store::write_config(cfg);
    au
}

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
fn selfcheck_history() -> Value {
    history::history_selftest(&store::ccbud_home())
}
#[tauri::command]
fn selfcheck_desktop() -> Value {
    let p = claudedesktop::build_profile(18799, "tok");
    json!({
        "hasBaseUrl": p.contains("http://localhost:18799"),
        "hasProvider": p.contains("inferenceProvider") && p.contains("gateway"),
        "hasModels": p.contains("claude-sonnet-4-6") && p.contains("anthropicFamilyTier") && p.contains("isFamilyDefault"),
        "hasBundleId": p.contains("com.anthropic.claudefordesktop"),
        "validXml": p.starts_with("<?xml") && p.contains("</plist>"),
    })
}
#[tauri::command]
fn selfcheck_import() -> Value {
    history::import_selftest(&store::ccbud_home())
}
#[tauri::command]
fn selfcheck_export() -> Value {
    let base = store::ccbud_home();
    let _ = history::history_selftest(&base);
    let file = base.join("test-claude").join("projects").join("-test-cwd").join("sess1.jsonl");
    let html = exporthtml::build_export_html(&file.to_string_lossy());
    json!({
        "len": html.len(),
        "hasConv": html.contains("__CONV__"),
        "hasContent": html.contains("hello world from selfcheck"),
        "hasSkin": html.contains("</style>"),
        "embedded": html.len() > 180000,
        "validHtml": html.starts_with("<!doctype html") && html.contains("</html>"),
    })
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
    let mut r = gateway::gateway_selftest(port).await;
    let recent = gw.monitor_recent().await;
    if let Some(o) = r.as_object_mut() {
        let req_ok = recent.get("reqBody").and_then(|b| b.get("text")).and_then(|t| t.as_str()).map(|s| !s.is_empty()).unwrap_or(false);
        let res_ok = recent.get("resBody").and_then(|b| b.get("text")).and_then(|t| t.as_str()).map(|s| s.contains("test-alias")).unwrap_or(false);
        let redacted = recent.get("reqHeaders").map(|h| h.to_string().contains("已隐藏")).unwrap_or(false);
        o.insert("monitorReqBody".into(), json!(req_ok));
        o.insert("monitorResBody".into(), json!(res_ok));
        o.insert("monitorRedacted".into(), json!(redacted));
    }
    Ok(r)
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
      try{ var ug=await window.ccbud.usageGet('all'); o.usage={tokens:ug.tokens,requests:ug.requests,fav:ug.favoriteModel,heatmap:(ug.heatmap||[]).length,byModel:(ug.byModel||[]).length,activeDays:ug.activeDays}; }catch(e){ o.usageErr=String(e); }
      try{ var cc=await window.ccbud.connect(); var s1=await window.ccbud.serverStatus(); var dd=await window.ccbud.disconnect(); var s2=await window.ccbud.serverStatus(); o.claude={connOk:cc&&cc.ok,connected:s1.connected,discOk:dd&&dd.ok,afterDisc:s2.connected}; }catch(e){ o.claudeErr=String(e); }
      try{ o.copyOk=await window.ccbud.copy('selfcheck-clip'); }catch(e){ o.copyErr=String(e); }
      try{ o.histMeta=await window.__TAURI__.core.invoke('selfcheck_history'); }catch(e){ o.histMetaErr=String(e); }
      try{ o.desktop=await window.__TAURI__.core.invoke('selfcheck_desktop'); }catch(e){ o.desktopErr=String(e); }
      try{ o.export=await window.__TAURI__.core.invoke('selfcheck_export'); }catch(e){ o.exportErr=String(e); }
      try{ o.import=await window.__TAURI__.core.invoke('selfcheck_import'); }catch(e){ o.importErr=String(e); }
      try{ var us=await window.ccbud.updateState(); var sa=await window.ccbud.updateSetAuto({check:false}); o.update={current:us.current,status:us.status,setAutoCheck:sa.check}; }catch(e){ o.updateErr=String(e); }
      o.errors=window.__ccbud_errors.slice(0,20);
    }catch(e){o.fatal=String((e&&e.stack)||e);}
    rep(o);
  },2200);
})();
"#;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // single-instance MUST be the first plugin registered.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .on_page_load(|webview, payload| {
            if webview.label() == "main"
                && matches!(payload.event(), tauri::webview::PageLoadEvent::Finished)
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

            // System tray: icon + menu (open / quit) + click-to-open.
            {
                use tauri::menu::{Menu, MenuItem};
                use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
                let open_i = MenuItem::with_id(app, "tray_open", "打开 ccbud", true, None::<&str>)?;
                let quit_i = MenuItem::with_id(app, "tray_quit", "退出 ccbud", true, None::<&str>)?;
                let menu = Menu::with_items(app, &[&open_i, &quit_i])?;
                let _ = TrayIconBuilder::with_id("main")
                    .icon(app.default_window_icon().cloned().unwrap())
                    .tooltip("ccbud")
                    .menu(&menu)
                    .show_menu_on_left_click(false)
                    .on_menu_event(|app, event| match event.id.as_ref() {
                        "tray_open" => {
                            if let Some(w) = app.get_webview_window("main") {
                                let _ = w.show();
                                let _ = w.unminimize();
                                let _ = w.set_focus();
                            }
                        }
                        "tray_quit" => app.exit(0),
                        _ => {}
                    })
                    .on_tray_icon_event(|tray, event| {
                        if let TrayIconEvent::Click {
                            button: MouseButton::Left,
                            button_state: MouseButtonState::Up,
                            position,
                            ..
                        } = event
                        {
                            let app = tray.app_handle();
                            if let Some(pop) = app.get_webview_window("popover") {
                                if pop.is_visible().unwrap_or(false) {
                                    let _ = pop.hide();
                                } else {
                                    let x = (position.x as i32 - 150).max(0);
                                    let y = position.y as i32 + 8;
                                    let _ = pop.set_position(tauri::PhysicalPosition::new(x, y));
                                    let _ = pop.show();
                                    let _ = pop.set_focus();
                                    let _ = app.emit("popover:show", ());
                                }
                            }
                        }
                    })
                    .build(app)?;
            }

            // History live-watch: fs events on the projects dirs → history:changed.
            {
                use notify_debouncer_mini::{new_debouncer, notify::RecursiveMode, DebounceEventResult};
                let app_w = app.handle().clone();
                if let Ok(mut deb) = new_debouncer(
                    std::time::Duration::from_millis(250),
                    move |res: DebounceEventResult| {
                        if let Ok(events) = res {
                            let files: Vec<String> = events.iter().map(|e| e.path.to_string_lossy().to_string()).collect();
                            let _ = app_w.emit("history:changed", json!({ "files": files }));
                        }
                    },
                ) {
                    for root in history::watch_roots(&store::read_config()) {
                        if root.is_dir() {
                            let _ = deb.watcher().watch(&root, RecursiveMode::Recursive);
                        }
                    }
                    std::mem::forget(deb); // keep watching for the app's lifetime
                }
            }

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
            selfcheck_report, selfcheck_routing, selfcheck_gateway, selfcheck_history, selfcheck_desktop, selfcheck_export, selfcheck_import
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            // Keep running in the tray when the user closes the window (hide instead of quit).
            if let tauri::RunEvent::WindowEvent {
                label,
                event: tauri::WindowEvent::CloseRequested { api, .. },
                ..
            } = event
            {
                if let Some(w) = app_handle.get_webview_window(&label) {
                    let _ = w.hide();
                }
                api.prevent_close();
            }
        });
}
