// Sidecar plugin manager.
//
// A ccbud plugin is a standalone local program that reuses some coding agent's
// subscription login (e.g. Grok) and exposes a standard inference endpoint on
// localhost. The host does not do protocol/vendor work for it — see
// docs/plugin-system.md. This module owns the piece the gateway can't: process
// lifecycle, port assignment, and health gating.
//
// Key design choice: a running plugin is surfaced as an ordinary provider whose
// baseUrl points at the plugin's localhost port. Enabling a plugin upserts a
// `backend:"plugin"` provider (id = `plugin:<id>`); disabling removes it. The
// gateway then routes to it with zero plugin-specific code.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::store;

/// A plugin's parsed manifest (plugin.json). Only the fields the host needs.
pub struct Manifest {
    pub dir: PathBuf,
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    /// Optional icon file relative to the plugin dir, e.g. "icon.svg".
    pub icon: String,
    /// endpoint.protocol → provider wire protocol.
    pub protocol: String,
    /// endpoint.basePath, e.g. "/v1".
    pub base_path: String,
    /// endpoint.healthPath, e.g. "/healthz".
    pub health_path: String,
    /// endpoint.readyTimeoutMs.
    pub ready_timeout_ms: u64,
    /// runtime.exec: { "<os>-<arch>": "bin/..." }.
    exec: Value,
    /// runtime.args, with {port}/{home} placeholders.
    args: Vec<String>,
    /// (alias, upstream) model pairs.
    pub models: Vec<(String, String)>,
    pub primary: String,
    pub light: String,
    /// Control-plane auth paths.
    pub auth_status_path: String,
    pub auth_login_path: String,
    pub auth_logout_path: String,
    /// source.git — upstream git repo used for install/update (optional).
    pub source_git: String,
    pub source_branch: String,
    /// source.build — shell command run in the clone to produce the binary.
    pub source_build: String,
}

impl Manifest {
    fn load(dir: PathBuf) -> Option<Manifest> {
        let raw = std::fs::read(dir.join("plugin.json")).ok()?;
        let v: Value = serde_json::from_slice(&raw).ok()?;
        let id = v.get("id")?.as_str()?.to_string();

        let s = |path: &[&str], default: &str| -> String {
            let mut cur = &v;
            for k in path {
                match cur.get(*k) {
                    Some(next) => cur = next,
                    None => return default.to_string(),
                }
            }
            cur.as_str().unwrap_or(default).to_string()
        };

        let args = v
            .get("runtime")
            .and_then(|r| r.get("args"))
            .and_then(|a| a.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_else(|| vec!["serve".into(), "--port".into(), "{port}".into(), "--home".into(), "{home}".into()]);

        let mut models = vec![];
        if let Some(arr) = v.get("models").and_then(|m| m.as_array()) {
            for m in arr {
                let alias = m.get("alias").and_then(|x| x.as_str()).unwrap_or("").to_string();
                let upstream = m
                    .get("upstream")
                    .and_then(|x| x.as_str())
                    .unwrap_or(alias.as_str())
                    .to_string();
                if !alias.is_empty() {
                    models.push((alias, upstream));
                }
            }
        }

        let ready_timeout_ms = v
            .get("endpoint")
            .and_then(|e| e.get("readyTimeoutMs"))
            .and_then(|x| x.as_u64())
            .unwrap_or(8000);

        Some(Manifest {
            dir,
            id,
            name: s(&["name"], "Plugin"),
            version: s(&["version"], "0.0.0"),
            description: s(&["description"], ""),
            icon: s(&["icon"], ""),
            protocol: s(&["endpoint", "protocol"], "openai-responses"),
            base_path: s(&["endpoint", "basePath"], "/v1"),
            health_path: s(&["endpoint", "healthPath"], "/healthz"),
            ready_timeout_ms,
            exec: v.get("runtime").and_then(|r| r.get("exec")).cloned().unwrap_or(Value::Null),
            args,
            models,
            primary: s(&["modelMapping", "primary"], ""),
            light: s(&["modelMapping", "light"], ""),
            auth_status_path: s(&["auth", "statusPath"], "/v1/plugin/auth"),
            auth_login_path: s(&["auth", "loginPath"], "/v1/plugin/auth/login"),
            auth_logout_path: s(&["auth", "logoutPath"], "/v1/plugin/auth/logout"),
            source_git: s(&["source", "git"], ""),
            source_branch: {
                let b = s(&["source", "branch"], "");
                if b.trim().is_empty() { "main".to_string() } else { b }
            },
            source_build: s(&["source", "build"], ""),
        })
    }

    /// Absolute path to the executable for the current platform, if declared.
    fn exec_path(&self) -> Option<PathBuf> {
        let rel = self.exec.get(platform_key()).and_then(|x| x.as_str())?;
        Some(self.dir.join(rel))
    }

    fn resolved_args(&self, port: u16, home: &str) -> Vec<String> {
        self.args
            .iter()
            .map(|a| a.replace("{port}", &port.to_string()).replace("{home}", home))
            .collect()
    }

    fn base_url(&self, port: u16) -> String {
        format!("http://127.0.0.1:{}{}", port, self.base_path)
    }

    /// The plugin's icon as a data URI (data:image/...;base64,...), if declared
    /// and readable — lets a plugin ship its own logo for the UI.
    fn icon_data_uri(&self) -> Option<String> {
        let rel = self.icon.trim();
        if rel.is_empty() {
            return None;
        }
        let path = self.dir.join(rel);
        let bytes = std::fs::read(&path).ok()?;
        if bytes.is_empty() || bytes.len() > 512 * 1024 {
            return None;
        }
        let ext = path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase());
        let mime = match ext.as_deref() {
            Some("svg") => "image/svg+xml",
            Some("png") => "image/png",
            Some("jpg") | Some("jpeg") => "image/jpeg",
            Some("webp") => "image/webp",
            Some("gif") => "image/gif",
            _ => return None,
        };
        Some(format!("data:{};base64,{}", mime, base64_encode(&bytes)))
    }
}

struct RunningPlugin {
    child: Child,
    port: u16,
}

/// Owns running plugin processes and their derived providers.
pub struct PluginManager {
    running: Mutex<HashMap<String, RunningPlugin>>,
    client: reqwest::Client,
}

impl PluginManager {
    pub fn new() -> Arc<PluginManager> {
        Arc::new(PluginManager {
            running: Mutex::new(HashMap::new()),
            client: reqwest::Client::new(),
        })
    }

    fn plugins_dir(&self) -> PathBuf {
        plugins_root()
    }

    fn plugin_dir(&self, id: &str) -> PathBuf {
        self.plugins_dir().join(id)
    }

    fn manifest(&self, id: &str) -> Option<Manifest> {
        Manifest::load(self.plugin_dir(id))
    }

    fn discover(&self) -> Vec<Manifest> {
        let mut out = vec![];
        if let Ok(rd) = std::fs::read_dir(self.plugins_dir()) {
            for e in rd.flatten() {
                if e.path().is_dir() {
                    if let Some(m) = Manifest::load(e.path()) {
                        out.push(m);
                    }
                }
            }
        }
        out
    }

    fn running_port(&self, id: &str) -> Option<u16> {
        self.running.lock().unwrap().get(id).map(|rp| rp.port)
    }

    /// True if the plugin process is alive; reaps and forgets an exited one.
    pub fn is_running(&self, id: &str) -> bool {
        let mut g = self.running.lock().unwrap();
        if let Some(rp) = g.get_mut(id) {
            match rp.child.try_wait() {
                Ok(Some(_)) => {
                    g.remove(id);
                    false
                }
                _ => true,
            }
        } else {
            false
        }
    }

    /// Port for a plugin: the live one if running, else a remembered one from
    /// runtime.json, else a freshly assigned free port (persisted).
    fn port_for(&self, id: &str) -> u16 {
        if let Some(p) = self.running_port(id) {
            return p;
        }
        let rt = self.plugin_dir(id).join("runtime.json");
        if let Ok(raw) = std::fs::read(&rt) {
            if let Ok(v) = serde_json::from_slice::<Value>(&raw) {
                if let Some(p) = v.get("port").and_then(|x| x.as_u64()) {
                    if p > 0 {
                        return p as u16;
                    }
                }
            }
        }
        let p = free_port().unwrap_or(8899);
        let _ = std::fs::create_dir_all(self.plugin_dir(id));
        let _ = std::fs::write(&rt, serde_json::to_vec(&json!({ "port": p })).unwrap_or_default());
        p
    }

    /// Enable a plugin: spawn it, health-gate, then upsert its provider.
    pub async fn start(&self, id: &str) -> Result<(), String> {
        let man = self.manifest(id).ok_or_else(|| format!("plugin '{}' not found", id))?;

        if self.is_running(id) {
            self.ensure_provider(&man, self.running_port(id).unwrap_or_else(|| self.port_for(id)));
            return Ok(());
        }

        let exec = man
            .exec_path()
            .ok_or_else(|| format!("no binary for this platform ({})", platform_key()))?;
        if !exec.exists() {
            return Err(format!("plugin binary missing: {}", exec.display()));
        }

        let port = self.port_for(id);
        let dir = self.plugin_dir(id);
        let _ = std::fs::create_dir_all(&dir);
        let home = dir.to_string_lossy().to_string();
        let args = man.resolved_args(port, &home);

        // stderr → plugin.log for diagnosis; stdout is the plugin's ready channel
        // (we already know the port, so we discard it).
        let stderr = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("plugin.log"))
            .map(Stdio::from)
            .unwrap_or_else(|_| Stdio::null());

        let child = Command::new(&exec)
            .args(&args)
            .current_dir(&man.dir)
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .map_err(|e| format!("spawn {}: {}", exec.display(), e))?;

        self.running.lock().unwrap().insert(id.to_string(), RunningPlugin { child, port });

        if !self.wait_ready(port, &man.health_path, man.ready_timeout_ms).await {
            let _ = self.stop(id);
            return Err("plugin did not become ready (see plugin.log)".into());
        }

        self.ensure_provider(&man, port);
        Ok(())
    }

    /// Disable a plugin: kill the process and remove its provider.
    pub fn stop(&self, id: &str) -> Result<(), String> {
        if let Some(mut rp) = self.running.lock().unwrap().remove(id) {
            let _ = rp.child.kill();
            let _ = rp.child.wait();
        }
        let pid = provider_id(id);
        let mut cfg = store::read_config();
        if let Some(arr) = cfg.get_mut("providers").and_then(|v| v.as_array_mut()) {
            arr.retain(|x| x.get("id").and_then(|v| v.as_str()) != Some(pid.as_str()));
        }
        store::write_config(cfg); // normalize fixes activeProviderId
        Ok(())
    }

    async fn wait_ready(&self, port: u16, health_path: &str, timeout_ms: u64) -> bool {
        let url = format!("http://127.0.0.1:{}{}", port, health_path);
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            if let Ok(r) = self.client.get(&url).timeout(Duration::from_secs(2)).send().await {
                if r.status().is_success() {
                    return true;
                }
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// Upsert the `backend:"plugin"` provider that fronts this plugin.
    fn ensure_provider(&self, man: &Manifest, port: u16) {
        let pid = provider_id(&man.id);
        let models: Vec<Value> = man
            .models
            .iter()
            .map(|(a, u)| json!({ "alias": a, "upstream": u }))
            .collect();
        let provider = json!({
            "id": pid,
            "name": man.name,
            "backend": "plugin",
            "pluginId": man.id,
            "baseUrl": man.base_url(port),
            "authToken": "",
            "protocol": man.protocol,
            "defaultModel": man.primary,
            "smallFastModel": man.light,
            "mapDefaultModels": true,
            "models": models,
            "icon": man.icon_data_uri(),
        });

        let mut cfg = store::read_config();
        let arr = match cfg.get_mut("providers").and_then(|v| v.as_array_mut()) {
            Some(a) => a,
            None => {
                cfg["providers"] = json!([]);
                cfg["providers"].as_array_mut().unwrap()
            }
        };
        if let Some(i) = arr
            .iter()
            .position(|x| x.get("id").and_then(|v| v.as_str()) == Some(pid.as_str()))
        {
            arr[i] = provider;
        } else {
            arr.push(provider);
        }
        store::write_config(cfg);
    }

    /// Full snapshot for the UI: install info + running + auth (queried live).
    pub async fn status(&self, id: &str) -> Value {
        let man = self.manifest(id);
        let running = self.is_running(id);
        let mut auth = Value::Null;
        if running {
            if let (Some(m), Some(port)) = (man.as_ref(), self.running_port(id)) {
                let url = format!("http://127.0.0.1:{}{}", port, m.auth_status_path);
                if let Ok(r) = self.client.get(&url).timeout(Duration::from_secs(3)).send().await {
                    if let Ok(v) = r.json::<Value>().await {
                        auth = v;
                    }
                }
            }
        }
        json!({
            "id": id,
            "name": man.as_ref().map(|m| m.name.clone()).unwrap_or_default(),
            "version": man.as_ref().map(|m| m.version.clone()).unwrap_or_default(),
            "description": man.as_ref().map(|m| m.description.clone()).unwrap_or_default(),
            "protocol": man.as_ref().map(|m| m.protocol.clone()).unwrap_or_default(),
            "icon": man.as_ref().and_then(|m| m.icon_data_uri()),
            "hasSource": man.as_ref().map(|m| !m.source_git.trim().is_empty()).unwrap_or(false),
            "official": man.as_ref().map(|m| is_official_source(&m.source_git)).unwrap_or(false),
            "providerId": provider_id(id),
            "running": running,
            "auth": auth,
        })
    }

    /// List all discovered plugins with their status.
    pub async fn list(&self) -> Value {
        let mut out = vec![];
        for m in self.discover() {
            out.push(self.status(&m.id).await);
        }
        json!(out)
    }

    /// Forward a login request to the plugin's control plane.
    pub async fn auth_login(&self, id: &str) -> Result<Value, String> {
        let man = self.manifest(id).ok_or("plugin not found")?;
        let port = self.running_port(id).ok_or("plugin not running")?;
        let url = format!("http://127.0.0.1:{}{}", port, man.auth_login_path);
        let r = self
            .client
            .post(&url)
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        r.json::<Value>().await.map_err(|e| e.to_string())
    }

    /// Forward a logout request to the plugin's control plane.
    pub async fn auth_logout(&self, id: &str) -> Result<Value, String> {
        let man = self.manifest(id).ok_or("plugin not found")?;
        let port = self.running_port(id).ok_or("plugin not running")?;
        let url = format!("http://127.0.0.1:{}{}", port, man.auth_logout_path);
        let r = self
            .client
            .post(&url)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        r.json::<Value>().await.map_err(|e| e.to_string())
    }

    /// Install a plugin from a local directory (must contain plugin.json) into
    /// ~/.ccbud/plugins/<id>. Reinstalling replaces the existing copy. Returns
    /// the installed plugin id.
    pub fn install(&self, src: &std::path::Path) -> Result<String, String> {
        let src_dir = if src.is_file() {
            src.parent().map(|p| p.to_path_buf()).ok_or("无效的路径")?
        } else {
            src.to_path_buf()
        };
        let man = Manifest::load(src_dir.clone()).ok_or("所选目录没有有效的 plugin.json")?;
        let dst = self.plugin_dir(&man.id);
        if self.is_running(&man.id) {
            return Err("请先停用同名插件，再重新安装".into());
        }
        // Picking the already-installed dir itself is a no-op, not a self-copy.
        let same = src_dir.canonicalize().ok() == dst.canonicalize().ok();
        if same && dst.exists() {
            return Ok(man.id);
        }
        if dst.exists() {
            std::fs::remove_dir_all(&dst).map_err(|e| e.to_string())?;
        }
        copy_dir_all(&src_dir, &dst).map_err(|e| format!("拷贝失败: {}", e))?;
        Ok(man.id)
    }

    /// Uninstall a plugin: stop it, drop its provider, and delete its directory.
    pub fn uninstall(&self, id: &str) -> Result<(), String> {
        let _ = self.stop(id);
        let dir = self.plugin_dir(id);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Install (or update) a plugin from a git repository: shallow-clone, run the
    /// manifest's build command, verify the binary exists, then install. Returns
    /// the plugin id.
    ///
    /// SECURITY: this clones and *builds* code from a user-supplied URL — i.e. it
    /// runs arbitrary code. The UI warns the user to import only trusted sources.
    pub fn install_from_git(&self, url: &str) -> Result<String, String> {
        let url = url.trim();
        if url.is_empty() {
            return Err("git 地址为空".into());
        }
        let _ = std::fs::create_dir_all(plugins_root());
        let tmp = plugins_root().join(format!(".import-{}", unique_suffix()));
        let _ = std::fs::remove_dir_all(&tmp);

        let out = Command::new("git")
            .args(["clone", "--depth", "1", url])
            .arg(&tmp)
            .output()
            .map_err(|e| format!("git 不可用: {}", e))?;
        if !out.status.success() {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(format!("git clone 失败: {}", String::from_utf8_lossy(&out.stderr).trim()));
        }

        let man = match Manifest::load(tmp.clone()) {
            Some(m) => m,
            None => {
                let _ = std::fs::remove_dir_all(&tmp);
                return Err("仓库根目录没有有效的 plugin.json".into());
            }
        };

        if !man.source_build.trim().is_empty() {
            let built = Command::new("sh")
                .arg("-c")
                .arg(man.source_build.trim())
                .current_dir(&tmp)
                .output();
            match built {
                Ok(o) if o.status.success() => {}
                Ok(o) => {
                    let _ = std::fs::remove_dir_all(&tmp);
                    return Err(format!(
                        "构建失败 (`{}`): {}",
                        man.source_build.trim(),
                        String::from_utf8_lossy(&o.stderr).trim()
                    ));
                }
                Err(e) => {
                    let _ = std::fs::remove_dir_all(&tmp);
                    return Err(format!("执行构建命令失败: {}", e));
                }
            }
        }

        match man.exec_path() {
            Some(p) if p.exists() => {}
            _ => {
                let _ = std::fs::remove_dir_all(&tmp);
                return Err(format!(
                    "构建后未找到当前平台二进制 ({})；请检查 plugin.json 的 runtime.exec / source.build",
                    platform_key()
                ));
            }
        }

        let _ = self.stop(&man.id);
        let dst = self.plugin_dir(&man.id);
        if dst.exists() {
            if let Err(e) = std::fs::remove_dir_all(&dst) {
                let _ = std::fs::remove_dir_all(&tmp);
                return Err(format!("移除旧版本失败: {}", e));
            }
        }
        if let Err(e) = copy_dir_all(&tmp, &dst) {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(format!("安装失败: {}", e));
        }
        let _ = std::fs::remove_dir_all(&tmp);
        Ok(man.id)
    }

    /// Check the plugin's git source for a newer version by fetching the remote
    /// plugin.json (GitHub raw) and comparing versions.
    pub async fn check_update(&self, id: &str) -> Value {
        let man = match self.manifest(id) {
            Some(m) => m,
            None => return json!({ "hasSource": false }),
        };
        if man.source_git.trim().is_empty() {
            return json!({ "hasSource": false, "current": man.version });
        }
        let raw = match github_raw(&man.source_git, &man.source_branch, "plugin.json") {
            Some(u) => u,
            None => {
                return json!({ "hasSource": true, "current": man.version, "error": "仅支持 github.com 来源的更新检查" })
            }
        };
        let latest = match self.client.get(&raw).timeout(Duration::from_secs(10)).send().await {
            Ok(r) if r.status().is_success() => match r.json::<Value>().await {
                Ok(v) => v.get("version").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                Err(_) => String::new(),
            },
            _ => String::new(),
        };
        if latest.is_empty() {
            return json!({ "hasSource": true, "current": man.version, "error": "无法获取远端版本" });
        }
        json!({
            "hasSource": true,
            "current": man.version,
            "latest": latest,
            "updateAvailable": version_gt(&latest, &man.version),
        })
    }

    /// Update a plugin by re-installing from its recorded git source.
    pub fn update(&self, id: &str) -> Result<String, String> {
        let man = self.manifest(id).ok_or_else(|| format!("插件 '{}' 未找到", id))?;
        if man.source_git.trim().is_empty() {
            return Err("该插件没有 git 来源，无法更新".into());
        }
        let url = man.source_git.clone();
        self.install_from_git(&url)
    }
}

fn provider_id(plugin_id: &str) -> String {
    format!("plugin:{}", plugin_id)
}

/// `<os>-<arch>` matching plugin.json's runtime.exec keys.
fn platform_key() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "darwin-arm64",
        ("macos", "x86_64") => "darwin-amd64",
        ("linux", "x86_64") => "linux-amd64",
        ("linux", "aarch64") => "linux-arm64",
        ("windows", "x86_64") => "windows-amd64",
        _ => "unknown",
    }
}

fn free_port() -> Option<u16> {
    std::net::TcpListener::bind("127.0.0.1:0")
        .ok()
        .and_then(|l| l.local_addr().ok())
        .map(|a| a.port())
}

/// ccbud config home (~/.ccbud, overridable via CCBUD_HOME) — mirrors store.rs.
fn ccbud_home() -> PathBuf {
    if let Ok(v) = std::env::var("CCBUD_HOME") {
        if !v.trim().is_empty() {
            return PathBuf::from(v);
        }
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".ccbud")
}

/// ~/.ccbud/plugins — where plugins are installed.
pub fn plugins_root() -> PathBuf {
    ccbud_home().join("plugins")
}

/// Minimal standard base64 — used only to embed a small plugin icon as a data URI.
fn base64_encode(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        out.push(T[(b0 >> 2) as usize] as char);
        out.push(T[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 { T[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[(b2 & 0x3f) as usize] as char } else { '=' });
    }
    out
}

/// Convert a github.com repo URL + branch into a raw file URL.
fn github_raw(git: &str, branch: &str, path: &str) -> Option<String> {
    let g = git.trim().trim_end_matches('/').trim_end_matches(".git");
    let rest = g
        .strip_prefix("https://github.com/")
        .or_else(|| g.strip_prefix("http://github.com/"))
        .or_else(|| g.strip_prefix("git@github.com:"))?;
    let br = if branch.trim().is_empty() { "main" } else { branch.trim() };
    Some(format!("https://raw.githubusercontent.com/{}/{}/{}", rest, br, path))
}

/// True if a git URL points at the official `ccbud` org on github.
fn is_official_source(git: &str) -> bool {
    let g = git.trim().trim_end_matches('/').trim_end_matches(".git");
    g.strip_prefix("https://github.com/")
        .or_else(|| g.strip_prefix("http://github.com/"))
        .or_else(|| g.strip_prefix("git@github.com:"))
        .and_then(|rest| rest.split('/').next())
        .map(|owner| owner.eq_ignore_ascii_case("ccbud"))
        .unwrap_or(false)
}

/// True if semver-ish `a` is strictly newer than `b` (e.g. "0.2.0" > "0.1.9").
fn version_gt(a: &str, b: &str) -> bool {
    parse_ver(a) > parse_ver(b)
}
fn parse_ver(v: &str) -> Vec<u64> {
    v.trim()
        .trim_start_matches('v')
        .split(|c| c == '.' || c == '-' || c == '+')
        .map(|s| s.parse::<u64>().unwrap_or(0))
        .collect()
}
/// A process-unique-ish suffix for temporary import directories.
fn unique_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{}", std::process::id(), nanos)
}

/// Recursively copy a directory tree (files + subdirs; symlinks skipped).
fn copy_dir_all(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        if entry.file_name() == ".git" {
            continue; // never copy VCS metadata into an install dir
        }
        let ty = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&from, &to)?;
        } else if ty.is_file() {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}
