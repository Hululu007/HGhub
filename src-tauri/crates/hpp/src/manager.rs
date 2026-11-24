use crate::{
    model::{self, Meta},
    native::Logs,
    network::Network,
    network::{ClearanceCookie, WebViewCookie},
    node::{NodeBootstrap, UnavailableNode},
    python::{PyWorker, PythonBootstrap, RuntimeWorker, UnavailablePython},
    runtime::{self, Worker},
    store::{atomic_write, Store},
    webhome::clear_webhome_prefix_keys,
    webhome_data::MiddleClearReport,
    wire::Transfer,
    PluginError, Result,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Instant,
};

#[path = "distribution.rs"]
mod distribution;
#[path = "endpoints.rs"]
mod endpoints;
#[path = "updater.rs"]
mod updater;
pub use distribution::{DocumentReader, ImportPreview, ImportReport, PluginOrigin, UpdateCheck};
pub use updater::{Subscription, UpdatePolicy, UpdateState};

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IndexEntry {
    meta: Meta,
    enabled: bool,
    source: Option<String>,
    #[serde(default)]
    origin: Option<PluginOrigin>,
    #[serde(default)]
    usable: bool,
    #[serde(default)]
    settings: BTreeMap<String, Value>,
    #[serde(default)]
    endpoint: Value,
}
struct Installed {
    entry: IndexEntry,
    worker: Option<RuntimeWorker>,
    network: Arc<Network>,
    store: Arc<Store>,
    logs: Logs,
    error: Option<PluginError>,
    revision: String,
    retired: bool,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginInfo {
    pub meta: Meta,
    pub enabled: bool,
    pub source: Option<String>,
    pub origin: Option<PluginOrigin>,
    pub can_restore: bool,
    pub status: String,
    pub methods: Vec<String>,
    pub error: Option<PluginError>,
    pub endpoint: Value,
    pub revision: String,
}
pub struct PluginManager {
    media_capabilities: Mutex<crate::capabilities::MediaCapabilities>,
    root: PathBuf,
    document_reader: Option<Arc<dyn DocumentReader>>,
    web_login: Option<Arc<dyn crate::web_login::WebLoginProvider>>,
    /// Python 边车引导：iOS 实现启动 CPython，非 iOS 用桩实现（见
    /// `python::UnavailablePython`）。未注入时默认用桩实现。
    python_bootstrap: Arc<dyn PythonBootstrap>,
    /// Node 边车引导：iOS 实现启动 Node 引擎，非 iOS 用桩实现（见
    /// `node::UnavailableNode`）。未注入时默认用桩实现，插件 net
    /// 请求走 wreq（今天的行为）。
    node_bootstrap: Arc<dyn NodeBootstrap>,
    /// 边车代理入口的实际地址（/health 返回；9978 被占时会换临时端口），
    /// 供回环取流时识别自家代理地址用。
    py_proxy_base: Mutex<Option<String>>,
    plugins: tokio::sync::RwLock<BTreeMap<String, Arc<tokio::sync::Mutex<Installed>>>>,
    mutation: tokio::sync::Mutex<()>,
    strict: Option<bool>,
    cooldown: Arc<Mutex<HashMap<String, Instant>>>,
    distribution: distribution::Distribution,
    updater: updater::Updater,
    pub(crate) playback_gate: tokio::sync::RwLock<()>,
    pub(crate) resolver_preferences: crate::resolver_preferences::ResolverPreferences,
    pub sessions: crate::media::MediaSessions,
    pub library: crate::library::Library,
}
impl PluginManager {
    pub fn open(root: PathBuf) -> Result<Self> {
        Self::with_mode(root, None)
    }
    pub fn with_mode(root: PathBuf, strict: Option<bool>) -> Result<Self> {
        std::fs::create_dir_all(&root)?;
        let cooldown: Arc<Mutex<HashMap<String, Instant>>> = Default::default();
        let mut plugins = BTreeMap::new();
        let path = root.join("index.json");
        let index: Vec<IndexEntry> = if path.exists() {
            serde_json::from_slice(&std::fs::read(path)?)
                .map_err(|_| PluginError::internal("插件索引损坏，原文件已保留"))?
        } else {
            vec![]
        };
        for mut entry in index {
            let indexed = Meta::parse(serde_json::to_value(&entry.meta).unwrap())?;
            // 启动恢复：JS 读 plugin.js 做完整检查；Python 读 plugin.py
            // 只做静态扫描（名字与 id 已在索引里，不从代码重新派生）。
            let recovered = if indexed.runtime == "python" {
                std::fs::read_to_string(root.join(&indexed.id).join("plugin.py"))
                    .map_err(PluginError::from)
                    .and_then(|code| {
                        crate::python::scan_source(&code).map(|_| indexed.clone())
                    })
            } else {
                std::fs::read_to_string(root.join(&indexed.id).join("plugin.js"))
                    .map_err(PluginError::from)
                    .and_then(|code| runtime::inspect(&code))
            };
            let mut error = None;
            match recovered {
                Ok(meta) if meta.id == indexed.id => {
                    if serde_json::to_value(&meta).unwrap()
                        != serde_json::to_value(&indexed).unwrap()
                    {
                        entry.usable = false;
                    }
                    entry.meta = meta;
                }
                Ok(_) => error = Some(PluginError::invalid("插件文件与索引身份不一致")),
                Err(e) => error = Some(e),
            }
            let mut installed =
                Self::installed(root.join(&indexed.id), entry, strict, cooldown.clone(), Arc::new(UnavailableNode))?;
            installed.error = error;
            plugins.insert(indexed.id, Arc::new(tokio::sync::Mutex::new(installed)));
        }
        let library = crate::library::Library::open(root.join("library.json"))?;
        let updater = updater::Updater::open(root.clone())?;
        let resolver_preferences = crate::resolver_preferences::ResolverPreferences::open(
            root.join("resolver-preferences.json"),
        )?;
        Ok(Self {
            media_capabilities: Default::default(),
            root,
            document_reader: None,
            web_login: None,
            python_bootstrap: Arc::new(UnavailablePython),
            node_bootstrap: Arc::new(UnavailableNode),
            py_proxy_base: Default::default(),
            plugins: tokio::sync::RwLock::new(plugins),
            mutation: tokio::sync::Mutex::new(()),
            strict,
            cooldown,
            distribution: Default::default(),
            updater,
            playback_gate: Default::default(),
            resolver_preferences,
            sessions: Default::default(),
            library,
        })
    }
    pub fn with_web_login(mut self, provider: Arc<dyn crate::web_login::WebLoginProvider>) -> Self {
        self.web_login = Some(provider);
        self
    }
    /// 注入 Python 边车引导（iOS 实现启动 CPython；不注入时默认桩实现，
    /// Python 插件初始化报 UNSUPPORTED）。
    pub fn with_python_bootstrap(mut self, provider: Arc<dyn PythonBootstrap>) -> Self {
        self.python_bootstrap = provider;
        self
    }
    /// 注入 Node 边车引导（iOS 实现启动 Node 引擎；不注入时默认桩实现，
    /// 插件 net 请求走 wreq）。已装插件的 Network 也同步换引导——
    /// 管理器先建、引导后注，Network 建时还拿不到它。
    pub fn with_node_bootstrap(mut self, provider: Arc<dyn NodeBootstrap>) -> Self {
        self.node_bootstrap = provider.clone();
        if let Ok(plugins) = self.plugins.try_read() {
            for p in plugins.values() {
                if let Ok(p) = p.try_lock() {
                    p.network.set_node_bootstrap(provider.clone());
                }
            }
        }
        self
    }
    /// 记录边车代理入口的实际地址（PyWorker spawn 成功后调）。
    pub(crate) fn set_py_proxy_base(&self, base: String) {
        *self.py_proxy_base.lock().unwrap() = Some(base);
    }
    /// 该地址是不是自家 Python 代理入口（回环取流时豁免网络目标校验）。
    pub(crate) fn is_py_proxy_url(&self, url: &str) -> bool {
        let Some(base) = self.py_proxy_base.lock().unwrap().clone() else {
            return false;
        };
        url.starts_with(base.as_str())
    }
    pub fn with_document_reader(mut self, reader: Arc<dyn DocumentReader>) -> Self {
        self.document_reader = Some(reader);
        self
    }
    pub async fn configure_media(
        &self,
        media: crate::capabilities::MediaCapabilities,
    ) -> Result<bool> {
        media.validate()?;
        let _playback = self.playback_gate.write().await;
        let _mutation = self.mutation.lock().await;
        {
            let mut current = self.media_capabilities.lock().unwrap();
            if *current == media {
                return Ok(false);
            }
            *current = media;
        }
        // Environment is a load-time snapshot: cancel old workers and sessions;
        // the next list/call initializes using the new capabilities.
        let plugins = self
            .plugins
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for plugin in plugins {
            let mut p = plugin.lock().await;
            if let Some(worker) = p.worker.take() {
                worker.stop();
            }
            self.sessions.release_plugin(&p.entry.meta.id);
            p.revision = uuid::Uuid::new_v4().to_string();
            p.error = None;
        }
        Ok(true)
    }
    fn installed(
        dir: PathBuf,
        entry: IndexEntry,
        strict: Option<bool>,
        cooldown: Arc<Mutex<HashMap<String, Instant>>>,
        node_bootstrap: Arc<dyn NodeBootstrap>,
    ) -> Result<Installed> {
        std::fs::create_dir_all(&dir)?;
        let logs: Logs = Default::default();
        let network = Arc::new(Network::new(
            entry.meta.clone(),
            dir.clone(),
            strict,
            cooldown,
            node_bootstrap,
        )?);
        // net.request 的代理决议写进该插件的 ext.log（v0.0.78）。
        network.set_logs(logs.clone());
        Ok(Installed {
            network,
            store: Arc::new(Store::open(dir.join("storage.json"))?),
            entry,
            worker: None,
            logs,
            error: None,
            revision: uuid::Uuid::new_v4().to_string(),
            retired: false,
        })
    }
    async fn save(&self) -> Result<()> {
        let list = self
            .plugins
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut entries = vec![];
        for p in list {
            entries.push(p.lock().await.entry.clone());
        }
        atomic_write(
            &self.root.join("index.json"),
            &serde_json::to_vec_pretty(&entries)
                .map_err(|_| PluginError::internal("无法保存插件索引"))?,
        )
    }
    async fn get(&self, id: &str) -> Result<Arc<tokio::sync::Mutex<Installed>>> {
        self.plugins
            .read()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| PluginError::new("NOT_FOUND", "插件未安装"))
    }
    fn settings(entry: &IndexEntry) -> Value {
        let mut v = serde_json::Map::new();
        for s in &entry.meta.settings {
            if let Some(k) = s["key"].as_str() {
                if let Some(val) = entry.settings.get(k).or_else(|| s.get("default")) {
                    v.insert(k.into(), val.clone());
                }
            }
        }
        Value::Object(v)
    }
    fn requirements(&self, meta: &Meta) -> Result<()> {
        for r in &meta.requires {
            let supported = match r.r#type.as_str() {
                "feature" if r.name == "webview" => self.web_login.is_some(),
                "api" if r.name == "webview.open" => self.web_login.is_some(),
                "feature" => r.name == "charset.legacy",
                "api" => [
                    "http.fetch",
                    "storage.get",
                    "storage.set",
                    "storage.delete",
                    "storage.clear",
                    "storage.keys",
                    "cookies.get",
                    "cookies.set",
                    "cookies.clear",
                    "html.parse",
                    "crypto.digest",
                    "crypto.hmac",
                    "crypto.aesEncrypt",
                    "crypto.aesDecrypt",
                    "crypto.rsaEncrypt",
                    "crypto.randomBytes",
                    "crypto.randomUUID",
                    "codec.base64Encode",
                    "codec.base64Decode",
                    "codec.hexEncode",
                    "codec.hexDecode",
                    "codec.utf8Encode",
                    "codec.utf8Decode",
                    "codec.decodeText",
                    "settings.get",
                    "settings.getAll",
                    "endpoint.url",
                ]
                .contains(&r.name.as_str()),
                _ => false,
            };
            if !supported {
                return Err(PluginError::new(
                    "UNSUPPORTED",
                    format!("缺少必需能力 {}:{}", r.r#type, r.name),
                ));
            }
        }
        Ok(())
    }
    async fn initialize(&self, p: &mut Installed, full: bool) -> Result<RuntimeWorker> {
        if p.retired {
            return Err(PluginError::cancelled());
        }
        if !p.entry.enabled {
            return Err(PluginError::new("UNSUPPORTED", "插件已停用"));
        }
        if !full {
            if let Some(w) = &p.worker {
                if let Some(error) = w.fault() {
                    p.error = Some(error.clone());
                    return Err(error);
                }
                if !w.closed() {
                    return Ok(w.clone());
                }
                if p.error.is_some() {
                    return Err(p.error.clone().unwrap());
                }
            }
        }
        if full {
            self.sessions.release_plugin(&p.entry.meta.id);
            p.revision = uuid::Uuid::new_v4().to_string();
        }
        self.requirements(&p.entry.meta)?;
        if let Some(w) = p.worker.take() {
            w.stop();
        }
        // Python 运行时：读 plugin.py，走 CPython 边车（PyWorker），不走
        // QuickJS。media_base 是宿主回环地址，/load 时传给边车，
        // 蜘蛛的 fetch/log 桥以它为目标；服务已在管理器创建时预热，
        // 这里只读不启动。
        if p.entry.meta.runtime == "python" {
            let code =
                std::fs::read_to_string(self.root.join(&p.entry.meta.id).join("plugin.py"))?;
            let media_base = crate::media::MediaSessions::base(self)?;
            let media_caps = self.media_capabilities.lock().unwrap().clone();
            let (py, proxy_base) = PyWorker::spawn(
                p.entry.meta.id.clone(),
                code,
                media_base,
                media_caps,
                &self.python_bootstrap,
                p.entry.meta.budget("media.getDetail"),
            )
            .await?;
            self.set_py_proxy_base(proxy_base);
            let w = RuntimeWorker::Py(py);
            p.worker = Some(w.clone());
            p.error = None;
            p.entry.usable = true;
            return Ok(w);
        }
        let source = std::fs::read_to_string(self.root.join(&p.entry.meta.id).join("plugin.js"))?;
        let spawn = |endpoint: Value| {
            Worker::spawn(
                source.clone(),
                p.entry.meta.clone(),
                Self::settings(&p.entry),
                endpoint,
                p.store.clone(),
                p.network.clone(),
                p.logs.clone(),
                self.media_capabilities.lock().unwrap().clone(),
                self.web_login.clone(),
            )
        };
        let mut w = spawn(p.entry.endpoint.clone()).await?;
        if full && w.methods.contains(&"endpoints.list".into()) {
            let result = w
                .call(
                    uuid::Uuid::new_v4().to_string(),
                    "endpoints.list".into(),
                    Transfer::from_json(json!({"refresh":false})),
                    p.entry.meta.budget("endpoints.list"),
                )
                .await?;
            let endpoints = model::validate_result(
                "endpoints.list",
                result.json(),
                p.entry.meta.allow_insecure_http,
            )?;
            let array = endpoints.as_array().unwrap();
            if array.is_empty() {
                w.stop();
                return Err(PluginError::internal("线路列表为空"));
            }
            let selected = array
                .iter()
                .find(|e| e["id"] == p.entry.endpoint["id"])
                .unwrap_or(&array[0])
                .clone();
            if selected != p.entry.endpoint {
                w.stop();
                w = spawn(selected.clone()).await?;
                p.entry.endpoint = selected;
            }
        }
        p.worker = Some(RuntimeWorker::Js(w.clone()));
        p.error = None;
        p.entry.usable = true;
        Ok(RuntimeWorker::Js(w))
    }
    fn info(p: &Installed) -> PluginInfo {
        let error = p
            .error
            .clone()
            .or_else(|| p.worker.as_ref().and_then(RuntimeWorker::fault));
        let methods = p
            .worker
            .as_ref()
            .map(|w| w.methods().to_vec())
            .unwrap_or_default();
        let status = if !p.entry.enabled {
            "disabled"
        } else if error.as_ref().is_some_and(|e| e.code == "UNSUPPORTED") {
            "incompatible"
        } else if error.is_some() {
            "failed"
        } else if p.worker.as_ref().is_some_and(|w| !w.closed()) {
            "ready"
        } else {
            "unloaded"
        };
        PluginInfo {
            meta: p.entry.meta.clone(),
            enabled: p.entry.enabled,
            source: p.entry.source.clone(),
            origin: p.entry.origin.clone(),
            can_restore: false,
            status: status.into(),
            methods,
            error,
            endpoint: p.entry.endpoint.clone(),
            revision: p.revision.clone(),
        }
    }
    pub async fn list(&self) -> Vec<PluginInfo> {
        let all = self
            .plugins
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut out = vec![];
        for p in all {
            let mut p = p.lock().await;
            if p.entry.enabled && p.worker.is_none() && p.error.is_none() {
                if let Err(error) = self.initialize(&mut p, true).await {
                    p.error = Some(error);
                }
            }
            let mut info = Self::info(&p);
            info.can_restore = self
                .root
                .join(&p.entry.meta.id)
                .join("plugin.previous.json")
                .exists();
            out.push(info);
        }
        out
    }
    pub async fn initialize_enabled(&self) {
        let _mutation = self.mutation.lock().await;
        let plugins = self
            .plugins
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for plugin in plugins {
            let mut p = plugin.lock().await;
            if p.entry.enabled && p.worker.is_none() && p.error.is_none() {
                if let Err(error) = self.initialize(&mut p, true).await {
                    p.error = Some(error);
                }
            }
        }
        let _ = self.save().await;
    }
    pub async fn import_code(
        &self,
        code: String,
        source: Option<String>,
        allow_downgrade: bool,
    ) -> Result<PluginInfo> {
        self.import_code_mode(code, source, allow_downgrade, false)
            .await
    }
    pub async fn import_code_mode(
        &self,
        code: String,
        source: Option<String>,
        allow_downgrade: bool,
        install_only: bool,
    ) -> Result<PluginInfo> {
        self.install_code(
            code,
            source,
            None,
            allow_downgrade,
            Some(!install_only),
            None,
            false,
            None,
            None,
        )
        .await
    }
    async fn install_code(
        &self,
        code: String,
        source: Option<String>,
        origin: Option<PluginOrigin>,
        allow_downgrade: bool,
        enabled: Option<bool>,
        expected_revision: Option<&str>,
        restoring: bool,
        // 直接导入的本地 HTML 页面内容；None 时保留磁盘上已有的页面文件
        // （恢复旧版本走这条路），全新安装且 meta 有 localPage 却没内容时报错。
        page: Option<String>,
        cancel: Option<&tokio_util::sync::CancellationToken>,
    ) -> Result<PluginInfo> {
        let cancel = cancel.cloned().unwrap_or_default();
        let _mutation = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(PluginError::cancelled()),
            guard = self.mutation.lock() => guard,
        };
        let meta = Self::inspect_document(&code, source.as_deref()).await?;
        if cancel.is_cancelled() {
            return Err(PluginError::cancelled());
        }
        let existing = self.plugins.read().await.get(&meta.id).cloned();
        let mut settings = BTreeMap::new();
        let mut endpoint = Value::Null;
        let mut was_enabled = true;
        let mut previous = None;
        if let Some(p) = &existing {
            let p = p.lock().await;
            if expected_revision.is_some_and(|r| r != p.revision) {
                return Err(PluginError::invalid("插件已变化，请重新检查更新"));
            }
            if model::version(&meta.version)? < model::version(&p.entry.meta.version)?
                && !allow_downgrade
            {
                return Err(PluginError::invalid("该版本更低；确认降级后再导入"));
            }
            settings = p.entry.settings.clone();
            endpoint = p.entry.endpoint.clone();
            was_enabled = p.entry.enabled;
            if !restoring && p.entry.usable {
                // 快照读旧运行时的文件：JS 是 plugin.js，Python 是 plugin.py。
                let old_file = if p.entry.meta.runtime == "python" {
                    "plugin.py"
                } else {
                    "plugin.js"
                };
                previous = Some(distribution::Snapshot {
                    code: std::fs::read_to_string(
                        self.root.join(&meta.id).join(old_file),
                    )?,
                    entry: p.entry.clone(),
                });
            }
        } else if expected_revision.is_some() {
            return Err(PluginError::invalid("插件已移除，请重新导入"));
        }
        if restoring {
            let snapshot: distribution::Snapshot = serde_json::from_slice(&std::fs::read(
                self.root.join(&meta.id).join("plugin.previous.json"),
            )?)
            .map_err(|_| PluginError::invalid("恢复文件损坏"))?;
            let current = settings;
            settings = snapshot.entry.settings;
            settings.extend(current);
        }
        settings.retain(|key, value| {
            meta.settings
                .iter()
                .find(|s| s["key"].as_str() == Some(key))
                .is_some_and(|s| model::validate_setting(s, value).is_ok())
        });
        if cancel.is_cancelled() {
            return Err(PluginError::cancelled());
        }
        let dir = self.root.join(&meta.id);
        std::fs::create_dir_all(&dir)?;
        // 运行时切换时旧文件是死文件：保存成功后再清（回滚路径不动它，
        // 回滚后旧文件仍是旧运行时的正确内容）。
        let file = dir.join(if meta.runtime == "python" {
            "plugin.py"
        } else {
            "plugin.js"
        });
        if let Some(previous) = previous {
            atomic_write(
                &dir.join("plugin.previous.json"),
                &serde_json::to_vec(&previous)
                    .map_err(|_| PluginError::internal("无法保存恢复文件"))?,
            )?;
        }
        // Keep stale references from reinitializing against a replaced file. Wait for the old
        // thread to finish before reopening storage/cookies, so final writes are visible.
        let old_code = std::fs::read(&file).ok();
        let local_page = meta
            .webhome
            .as_ref()
            .and_then(|w| w.local_page.clone());
        let old_page = local_page
            .as_deref()
            .and_then(|name| std::fs::read(dir.join(name)).ok());
        let entry = IndexEntry {
            meta: meta.clone(),
            enabled: enabled.unwrap_or(was_enabled),
            source,
            origin,
            usable: false,
            settings,
            endpoint,
        };
        let mut old_guard = match &existing {
            Some(old) => Some(old.lock().await),
            None => None,
        };
        if let Some(old) = old_guard.as_mut() {
            if let Some(w) = old.worker.take() {
                w.stop_and_wait().await?;
            }
        }
        let mut p = Self::installed(dir.clone(), entry, self.strict, self.cooldown.clone(), self.node_bootstrap.clone())?;
        if let Some(old) = old_guard.as_ref() {
            p.store = old.store.clone();
        }
        if cancel.is_cancelled() {
            return Err(PluginError::cancelled());
        }
        // 直接导入的本地 HTML 页面与插件文件一起落盘，文件名取自
        // meta.webhome.localPage；先写页面，失败时插件文件还没动。
        if let Some(name) = &local_page {
            if let Some(content) = page.as_deref() {
                atomic_write(&dir.join(name), content.as_bytes())?;
            } else if !dir.join(name).exists() {
                return Err(PluginError::internal("本地 WebHome 页面内容缺失"));
            }
        }
        atomic_write(&file, code.as_bytes())?;
        if let Some(old) = old_guard.as_mut() {
            old.retired = true;
        }
        drop(old_guard);
        self.sessions.release_plugin(&meta.id);
        if p.entry.enabled {
            if let Err(e) = self.initialize(&mut p, true).await {
                p.error = Some(e);
            }
        }
        let mut info = Self::info(&p);
        info.can_restore = dir.join("plugin.previous.json").exists();
        self.plugins
            .write()
            .await
            .insert(meta.id.clone(), Arc::new(tokio::sync::Mutex::new(p)));
        if let Err(e) = self.save().await {
            if let Some(new) = self.plugins.write().await.remove(&meta.id) {
                if let Some(w) = new.lock().await.worker.take() {
                    w.stop();
                }
            }
            if let Some(bytes) = old_code {
                atomic_write(&file, &bytes)?;
            } else {
                let _ = std::fs::remove_file(&file);
            }
            if let Some(name) = &local_page {
                if let Some(bytes) = old_page {
                    atomic_write(&dir.join(name), &bytes)?;
                } else {
                    let _ = std::fs::remove_file(dir.join(name));
                }
            }
            if let Some(old) = existing {
                old.lock().await.retired = false;
                self.plugins.write().await.insert(meta.id, old);
            }
            return Err(e);
        }
        // 保存成功：清掉另一运行时的旧文件（js↔py 切换时）。
        let stale = dir.join(if meta.runtime == "python" {
            "plugin.js"
        } else {
            "plugin.py"
        });
        let _ = std::fs::remove_file(stale);
        Ok(info)
    }
    pub async fn retry(&self, id: &str) -> Result<PluginInfo> {
        let _mutation = self.mutation.lock().await;
        let p = self.get(id).await?;
        let info = {
            let mut p = p.lock().await;
            if let Err(e) = self.initialize(&mut p, true).await {
                p.error = Some(e);
            }
            Self::info(&p)
        };
        self.save().await?;
        Ok(info)
    }
    pub async fn set_enabled(&self, id: &str, enabled: bool) -> Result<PluginInfo> {
        let _mutation = self.mutation.lock().await;
        let p = self.get(id).await?;
        let info = {
            let mut p = p.lock().await;
            p.entry.enabled = enabled;
            if let Some(w) = p.worker.take() {
                w.stop();
            }
            self.sessions.release_plugin(id);
            p.error = None;
            if enabled {
                if let Err(e) = self.initialize(&mut p, true).await {
                    p.error = Some(e);
                }
            }
            Self::info(&p)
        };
        self.save().await?;
        Ok(info)
    }
    pub async fn remove(&self, id: &str) -> Result<()> {
        let _mutation = self.mutation.lock().await;
        let p = self.get(id).await?;
        {
            let p = p.lock().await;
            if let Some(w) = &p.worker {
                w.stop();
            }
            self.sessions.release_plugin(id);
        }
        self.plugins.write().await.remove(id);
        self.save().await?;
        // The ID was validated on import/load; resolved target must remain an immediate child.
        let target = self.root.join(id).canonicalize()?;
        let root = self.root.canonicalize()?;
        if target.parent() != Some(root.as_path()) {
            return Err(PluginError::invalid("插件目录越界"));
        }
        std::fs::remove_dir_all(target)?;
        Ok(())
    }
    pub async fn call_transfer(
        &self,
        id: &str,
        method: &str,
        params: Transfer,
        request_id: String,
    ) -> Result<Transfer> {
        self.call_scoped(id, method, params, request_id, false).await
    }
    pub(crate) async fn call_scoped(
        &self,
        id: &str,
        method: &str,
        mut params: Transfer,
        request_id: String,
        user_auth: bool,
    ) -> Result<Transfer> {
        if request_id.is_empty() || request_id.len() > 128 {
            return Err(PluginError::invalid("requestId 无效"));
        }
        crate::forms::validate_params(method, &params.json())?;
        let plugin = self.get(id).await?;
        let (w, meta, logs) = {
            let mut p = plugin.lock().await;
            let full = p.worker.is_none();
            let w = self.initialize(&mut p, full).await?;
            if !w.methods().iter().any(|m| m == method) {
                return Err(PluginError::new("UNSUPPORTED", "插件未实现该方法"));
            }
            if matches!(
                method,
                "media.resolvePlayback" | "live.resolve" | "resolver.resolve"
            ) {
                let Transfer::Object(ref mut fields) = params else {
                    return Err(PluginError::invalid("播放参数必须是对象"));
                };
                fields.insert("codecs".into(), Transfer::from_json(json!(w.media().codecs)));
            }
            (w, p.entry.meta.clone(), p.logs.clone())
        };
        let result = w
            .call_scoped(
                request_id.clone(),
                method.into(),
                params,
                meta.budget(method),
                user_auth && method == "auth.submit",
            )
            .await
            .map_err(|mut e| {
                if matches!(e.code.as_str(), "AUTH_REQUIRED" | "AUTH_EXPIRED") {
                    e.plugin_id = Some(id.into());
                }
                e
            })?;
        if method == "transformResource" {
            result.validate(8 * 1024 * 1024)?;
            return Ok(result);
        }
        let mut warnings = Vec::new();
        let validated = model::validate_result_with_warnings(
            method,
            result.json(),
            meta.allow_insecure_http,
            &mut warnings,
        );
        for warning in warnings {
            crate::native::validation_log(&logs, "warn", &warning, &request_id);
        }
        if let Err(e) = &validated {
            crate::native::validation_log(&logs, "error", &e.message, &request_id);
        }
        validated.map(Transfer::from_json)
    }
    pub async fn call(
        &self,
        id: &str,
        method: &str,
        params: Value,
        request_id: String,
    ) -> Result<Value> {
        self.call_transfer(id, method, Transfer::from_json(params), request_id)
            .await
            .map(|v| v.json())
    }
    pub async fn cancel(&self, request_id: &str) {
        self.sessions.cancel_refresh_waiter(request_id);
        self.distribution.cancel(request_id);
        let list = self
            .plugins
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for p in list {
            if let Some(w) = &p.lock().await.worker {
                w.cancel(request_id);
            }
        }
    }
    pub async fn logs(&self, id: &str) -> Result<Vec<Value>> {
        let p = self.get(id).await?;
        let p = p.lock().await;
        let logs = p.logs.lock().unwrap().snapshot();
        Ok(logs)
    }
    pub async fn settings_get(&self, id: &str) -> Result<Value> {
        Ok(Self::settings(&self.get(id).await?.lock().await.entry))
    }
    pub async fn settings_set(&self, id: &str, key: String, value: Value) -> Result<PluginInfo> {
        let _mutation = self.mutation.lock().await;
        let plugin = self.get(id).await?;
        let info = {
            let mut p = plugin.lock().await;
            let s = p
                .entry
                .meta
                .settings
                .iter()
                .find(|s| s["key"] == key)
                .ok_or_else(|| PluginError::invalid("未知设置 key"))?;
            model::validate_setting(s, &value)?;
            p.entry.settings.insert(key, value);
            self.sessions.release_plugin(id);
            if let Some(w) = p.worker.take() {
                w.stop();
            }
            if let Err(e) = self.initialize(&mut p, true).await {
                p.error = Some(e);
            }
            Self::info(&p)
        };
        self.save().await?;
        Ok(info)
    }
    pub async fn endpoint_set(&self, id: &str, endpoint_id: &str) -> Result<PluginInfo> {
        let entries = self
            .call(
                id,
                "endpoints.list",
                json!({"refresh":false}),
                uuid::Uuid::new_v4().to_string(),
            )
            .await?;
        let selected = entries
            .as_array()
            .and_then(|a| a.iter().find(|e| e["id"] == endpoint_id))
            .ok_or_else(|| PluginError::invalid("线路 ID 不存在"))?
            .clone();
        let _mutation = self.mutation.lock().await;
        let plugin = self.get(id).await?;
        let info = {
            let mut p = plugin.lock().await;
            p.entry.endpoint = selected;
            p.revision = uuid::Uuid::new_v4().to_string();
            self.sessions.release_plugin(id);
            if let Some(w) = p.worker.take() {
                w.stop();
            }
            if let Err(e) = self.initialize(&mut p, false).await {
                p.error = Some(e);
            }
            Self::info(&p)
        };
        self.save().await?;
        Ok(info)
    }
    pub(crate) async fn network(&self, id: &str) -> Result<Arc<Network>> {
        let p = self.get(id).await?;
        let p = p.lock().await;
        if !p.entry.enabled {
            return Err(PluginError::new("UNSUPPORTED", "插件已停用"));
        }
        Ok(p.network.clone())
    }
    /// WebView clearance 拿到的通行证存到该插件的内存存储（v0.0.77）：
    /// 只认 `cf_clearance`，不落盘、不进 Cookie 罐。
    pub async fn set_clearance(&self, id: &str, cookie: ClearanceCookie) -> Result<()> {
        self.network(id).await?.set_clearance(cookie);
        Ok(())
    }

    /// WebView→引擎 Cookie 桥（v0.0.86）：把 WebHome 窗口里读到的
    /// 目标 URL 的 Cookie 合并进该插件的普通 Cookie 罐，并把桥了几
    /// 个、名字是什么写进 ext.log（requestId `"net"`，只报名字、
    /// 不报值）。窗口不存在时调用方根本不调这里；插件已停用时报
    /// `UNSUPPORTED`（与 `set_clearance` 同一条路），调用方静默回落。
    pub async fn bridge_webview_cookies(
        &self,
        id: &str,
        cookies: Vec<WebViewCookie>,
    ) -> Result<usize> {
        let network = self.network(id).await?;
        let names = network.merge_webview_cookies(cookies);
        let count = names.len();
        network.log_cookie_bridge(&names);
        Ok(count)
    }

    /// 加宽桥（v0.0.87）：把 WebHome 窗口里首页 URL 的 Cookie 记下来，
    /// 供下一次发往 `target_origin` 的引擎请求附上（只附当次、不进
    /// 罐子、不落盘）；并把桥了几个、名字是什么、来源是哪个域写进
    /// ext.log（requestId `"net"`，只报名字与域名、不报值）。窗口不
    /// 存在时调用方根本不调这里；插件已停用时报 `UNSUPPORTED`（与
    /// `bridge_webview_cookies` 同一条路），调用方静默回落。
    pub async fn bridge_home_cookies(
        &self,
        id: &str,
        target_origin: &str,
        source_host: &str,
        cookies: Vec<WebViewCookie>,
    ) -> Result<usize> {
        let network = self.network(id).await?;
        let names = network.set_home_cookies(target_origin, cookies);
        let count = names.len();
        network.log_home_cookie_bridge(&names, source_host);
        Ok(count)
    }

    /// 清空某插件的请求 Cookie 罐（v0.0.83）：内存 + 落盘的 cookies.json
    /// 一起清，返回清掉的条数。只动罐子本身。
    pub async fn clear_cookie_jar(&self, id: &str) -> Result<usize> {
        let network = self.network(id).await?;
        network.clear_cookie_jar()
    }

    /// 全清全部插件的运行时缓存（v0.0.84 中间步骤，hpp 侧）：每个已装
    /// 插件的 Cookie 罐（内存 + cookies.json）、Network 响应缓存、
    /// `webhome:` 前缀的存储键；再加全部会话的媒体预加载缓存、插件
    /// 目录下的 `__pycache__` 与 `*.pyc`。按类别尽力清理：某一类失败
    /// 记入 warnings 并继续下一类。已停用的插件也在内（network/store
    /// 在 installed() 里对全部索引条目都建好了，与启用无关）。
    /// 只清运行时缓存：插件源文件（*.py/*.js/manifest）、插件存储里非
    /// `webhome:` 的键、设置、下载一律不动。
    pub async fn clear_all_runtime_caches(&self) -> MiddleClearReport {
        let mut report = MiddleClearReport::default();
        report.media_entries_dropped = self.sessions.clear_all_caches();
        let (dirs, files, mut warnings) = Self::clear_pycache(&self.root);
        report.pycache_dirs = dirs;
        report.pycache_files = files;
        report.warnings.append(&mut warnings);
        let plugins = self
            .plugins
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        report.plugins = plugins.len();
        for p in plugins {
            let p = p.lock().await;
            let id = p.entry.meta.id.clone();
            match p.network.clear_cookie_jar() {
                Ok(n) => report.jar_cleared += n,
                Err(e) => report
                    .warnings
                    .push(format!("插件 {id} 的 Cookie 罐清理失败：{e}")),
            }
            report.response_cache_dropped += p.network.clear_response_cache();
            match clear_webhome_prefix_keys(&p.store) {
                Ok(n) => report.webhome_keys_deleted += n,
                Err(e) => report
                    .warnings
                    .push(format!("插件 {id} 的 WebHome 缓存键清理失败：{e}")),
            }
        }
        report
    }

    /// 清空全部插件的运行日志缓冲（v0.0.84 最后一步，hpp 侧）：每个插件
    /// 的 ext.log 内存缓冲全清，返回丢掉的总行数。日志是内存态的
    /// （`native/logs.rs` 的 LogBuffer），不落盘。
    pub async fn clear_all_logs(&self) -> usize {
        let plugins = self
            .plugins
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut total = 0;
        for p in plugins {
            let p = p.lock().await;
            let mut logs = p.logs.lock();
            if let Ok(logs) = logs.as_mut() {
                total += logs.clear();
            }
        }
        total
    }

    /// 递归删掉 `root` 下全部 `__pycache__` 目录与 `*.pyc` 文件，返回
    /// （删掉的目录数，删掉的文件数，警告）。只删字节码：`*.py` 与其他
    /// 文件一律不动；符号链接不跟进（`DirEntry::file_type` 对链接的
    /// is_dir/is_file 都返回 false，直接跳过）；单个目录/文件失败时记
    /// 警告并继续，不中断整轮。
    fn clear_pycache(root: &std::path::Path) -> (usize, usize, Vec<String>) {
        let mut dirs = 0;
        let mut files = 0;
        let mut warnings = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    warnings.push(format!("无法读取目录 {}：{e}", dir.display()));
                    continue;
                }
            };
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(e) => {
                        warnings.push(format!("无法读取目录项：{e}"));
                        continue;
                    }
                };
                let ft = match entry.file_type() {
                    Ok(ft) => ft,
                    Err(e) => {
                        warnings.push(format!("无法读取 {} 的类型：{e}", entry.path().display()));
                        continue;
                    }
                };
                let name = entry.file_name().to_string_lossy().into_owned();
                let path = entry.path();
                if ft.is_dir() {
                    if name == "__pycache__" {
                        match std::fs::remove_dir_all(&path) {
                            Ok(()) => dirs += 1,
                            Err(e) => warnings.push(format!("无法删除 {}：{e}", path.display())),
                        }
                    } else {
                        stack.push(path);
                    }
                } else if ft.is_file() && name.ends_with(".pyc") {
                    match std::fs::remove_file(&path) {
                        Ok(()) => files += 1,
                        Err(e) => warnings.push(format!("无法删除 {}：{e}", path.display())),
                    }
                }
            }
        }
        (dirs, files, warnings)
    }

    /// 网络诊断探针（v0.0.80；v0.0.82：用对话框当前选中的模式跑）：
    /// 只读诊断，见 `Network::diagnose`。走首个已启用 WebHome 插件的
    /// Network（探针量的就是 WebHome 那条路；代理决议与 Node 边车全局
    /// 共享，换哪个插件结果相同，差别只在报告写进哪个插件的 ext.log）；
    /// 没有 WebHome 插件时走首个已启用插件的 Network；没有可用插件时报错。
    /// mode/url/version 是设置页对话框当前的值：mode 是当前选中的代理
    /// 模式（跟随系统/自定义/直连），url 是自定义地址（非自定义时为空），
    /// version 是应用版本（前端 `__APP_VERSION__`，报告头用）。
    pub async fn network_diagnose(&self, mode: &str, url: &str, version: &str) -> Result<String> {
        let plugins = self.list().await;
        let target = plugins
            .iter()
            .filter(|p| p.enabled)
            .find(|p| p.meta.webhome.is_some())
            .or_else(|| plugins.iter().find(|p| p.enabled))
            .map(|p| p.meta.id.clone());
        let Some(id) = target else {
            return Err(PluginError::new("UNSUPPORTED", "没有可用的插件网络"));
        };
        let network = self.network(&id).await?;
        network
            .diagnose(&tokio_util::sync::CancellationToken::new(), mode, url, version)
            .await
    }
    /// WebHome 缓存与插件共用同一份存储（store.rs），不是另一份副本。
    pub(crate) async fn store(&self, id: &str) -> Result<Arc<Store>> {
        let p = self.get(id).await?;
        let p = p.lock().await;
        if !p.entry.enabled {
            return Err(PluginError::new("UNSUPPORTED", "插件已停用"));
        }
        Ok(p.store.clone())
    }
    /// WebHome 页面日志写入绑定插件的日志面板（与插件自身的 hg.log 同一处）。
    pub(crate) async fn webhome_log(&self, id: &str, text: &str) -> Result<()> {
        let p = self.get(id).await?;
        let p = p.lock().await;
        if !p.entry.enabled {
            return Err(PluginError::new("UNSUPPORTED", "插件已停用"));
        }
        p.logs
            .lock()
            .map_err(|_| PluginError::internal("日志锁不可用"))?
            .push("info", text, "webhome", None);
        Ok(())
    }

    /// Python 蜘蛛日志（/pylog 桥）：写进该插件的日志面板，来源记 "python"。
    pub(crate) async fn python_log(&self, id: &str, level: &str, text: &str) -> Result<()> {
        let p = self.get(id).await?;
        let p = p.lock().await;
        if !p.entry.enabled {
            return Err(PluginError::new("UNSUPPORTED", "插件已停用"));
        }
        p.logs
            .lock()
            .map_err(|_| PluginError::internal("日志锁不可用"))?
            .push(level, text, "python", None);
        Ok(())
    }
    pub async fn meta(&self, id: &str) -> Result<Meta> {
        Ok(self.get(id).await?.lock().await.entry.meta.clone())
    }
    /// 插件根目录（回环服务解析本地 WebHome 页面时用）。
    pub(crate) fn root(&self) -> &std::path::Path {
        &self.root
    }
    pub(crate) async fn revision(&self, id: &str) -> Result<String> {
        let p = self.get(id).await?;
        let p = p.lock().await;
        if !p.entry.enabled {
            return Err(PluginError::cancelled());
        }
        Ok(p.revision.clone())
    }
}
impl Drop for PluginManager {
    fn drop(&mut self) {
        if let Ok(all) = self.plugins.try_read() {
            for p in all.values() {
                if let Ok(p) = p.try_lock() {
                    if let Some(w) = &p.worker {
                        w.stop();
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 两个 fixture 插件的索引：a 已启用、b 已停用（全清必须覆盖已停用
    /// 的插件——network/store 在 installed() 里对全部索引条目都建好了）。
    fn write_index(root: &std::path::Path) {
        let index = json!([
            {"meta": {"id": "fixture.a", "name": "A", "protocol": 1, "version": "1.0.0", "allowInsecureHttp": true}, "enabled": true, "source": null, "origin": null, "usable": true, "settings": {}, "endpoint": {}},
            {"meta": {"id": "fixture.b", "name": "B", "protocol": 1, "version": "1.0.0", "allowInsecureHttp": true}, "enabled": false, "source": null, "origin": null, "usable": true, "settings": {}, "endpoint": {}}
        ]);
        std::fs::write(root.join("index.json"), serde_json::to_vec(&index).unwrap()).unwrap();
    }

    fn write_cookies(dir: &std::path::Path, ordinary: usize, web_logins: usize) {
        let cookie = |name: &str| {
            json!({"name": name, "value": "1", "domain": "example.com", "path": "/", "hostOnly": false, "secure": false, "httpOnly": false, "expiresAt": null})
        };
        let ordinary: Vec<Value> = (0..ordinary).map(|i| cookie(&format!("o{i}"))).collect();
        let web_logins: Value = (0..web_logins)
            .map(|i| {
                (
                    format!("https://example{i}.com"),
                    json!({"generation": "g1", "cookies": [cookie(&format!("w{i}"))]}),
                )
            })
            .collect::<serde_json::Map<String, Value>>()
            .into();
        std::fs::write(
            dir.join("cookies.json"),
            json!({"ordinary": ordinary, "web_logins": web_logins}).to_string(),
        )
        .unwrap();
    }

    fn set_key(store: &Store, key: &str) {
        store
            .operation(
                "set",
                &Transfer::from_json(json!({ "key": key, "value": "v" })),
            )
            .unwrap();
    }

    fn has_key(store: &Store, key: &str) -> bool {
        store
            .operation("get", &Transfer::from_json(json!({ "key": key })))
            .unwrap()
            .json()["found"]
            .as_bool()
            == Some(true)
    }

    async fn seed(manager: &PluginManager) {
        let plugins = manager
            .plugins
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for p in plugins {
            let p = p.lock().await;
            // 响应缓存：每插件 1 条（Network 的 #[cfg(test)] 种子方法）。
            p.network.seed_response_cache("https://example.com/cached", 8);
            // 存储：每插件 1 个 webhome: 键 + 1 个插件自己的键。
            set_key(&p.store, "webhome:domains");
            set_key(&p.store, "own:progress");
            // 运行日志：每插件 2 行。
            let mut logs = p.logs.lock().unwrap();
            logs.push("info", "line one", "r1", None);
            logs.push("info", "line two", "r2", None);
        }
    }

    #[tokio::test]
    async fn clear_all_runtime_caches_clears_all_plugins_and_keeps_own_data() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_index(root);
        // Cookie 罐：a 2 条（1 普通 + 1 web 登录）、b 1 条（普通）。
        std::fs::create_dir_all(root.join("fixture.a")).unwrap();
        std::fs::create_dir_all(root.join("fixture.b")).unwrap();
        write_cookies(&root.join("fixture.a"), 1, 1);
        write_cookies(&root.join("fixture.b"), 1, 0);
        // __pycache__：a 有一个目录（含 .pyc + .py）、一个子目录的
        // __pycache__；b 有一个散落的 .pyc；两边都有必须保留的 .py。
        std::fs::create_dir_all(root.join("fixture.a/__pycache__")).unwrap();
        std::fs::write(root.join("fixture.a/__pycache__/mod.pyc"), b"bytecode").unwrap();
        std::fs::write(root.join("fixture.a/plugin.py"), b"# source").unwrap();
        std::fs::create_dir_all(root.join("fixture.a/sub/__pycache__")).unwrap();
        std::fs::write(root.join("fixture.a/sub/__pycache__/x.pyc"), b"bytecode").unwrap();
        std::fs::write(root.join("fixture.a/sub/helper.py"), b"# source").unwrap();
        std::fs::write(root.join("fixture.b/loose.pyc"), b"bytecode").unwrap();
        std::fs::write(root.join("fixture.b/plugin.py"), b"# source").unwrap();

        let manager = PluginManager::open(root.into()).unwrap();
        seed(&manager).await;
        let report = manager.clear_all_runtime_caches().await;
        assert_eq!(report.plugins, 2);
        assert_eq!(report.jar_cleared, 3);
        assert_eq!(report.response_cache_dropped, 2);
        assert_eq!(report.webhome_keys_deleted, 2);
        assert_eq!(report.media_entries_dropped, 0);
        assert_eq!(report.pycache_dirs, 2);
        assert_eq!(report.pycache_files, 1);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);

        // 罐子落盘已空、响应缓存已空、webhome: 键已删、自己的键还在。
        for id in ["fixture.a", "fixture.b"] {
            assert_eq!(
                std::fs::read_to_string(root.join(id).join("cookies.json")).unwrap(),
                "[]"
            );
            let plugins = manager.plugins.read().await;
            let p = plugins.get(id).unwrap().lock().await;
            assert_eq!(p.network.clear_response_cache(), 0);
            assert!(!has_key(&p.store, "webhome:domains"));
            assert!(has_key(&p.store, "own:progress"));
        }
        // .py 全在，只删了字节码。
        assert!(root.join("fixture.a/plugin.py").exists());
        assert!(root.join("fixture.a/sub/helper.py").exists());
        assert!(root.join("fixture.b/plugin.py").exists());
        assert!(!root.join("fixture.a/__pycache__").exists());
        assert!(!root.join("fixture.a/sub/__pycache__").exists());
        assert!(!root.join("fixture.b/loose.pyc").exists());
    }

    #[tokio::test]
    async fn clear_all_logs_drops_entries_from_all_plugins() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_index(root);
        let manager = PluginManager::open(root.into()).unwrap();
        seed(&manager).await;
        assert_eq!(manager.clear_all_logs().await, 4);
        assert_eq!(manager.clear_all_logs().await, 0);
        let plugins = manager
            .plugins
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for p in plugins {
            let p = p.lock().await;
            assert!(p.logs.lock().unwrap().snapshot().is_empty());
        }
    }
}
