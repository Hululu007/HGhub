use crate::{
    cat::{
        subscriptions::{
            derive_plugin_id_from_stem, derive_plugin_id_from_url, extract_zip,
            fetch_subscription, md5_hex, CatSourceRecord, CatStore,
        },
        worker::CatWorker,
    },
    model::{self, Meta},
    native::Logs,
    network::Network,
    network::{ClearanceCookie, WebViewCookie},
    node::{NodeBootstrap, NodeClient, UnavailableNode},
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
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
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
    /// 已加载的猫源服务地址（每次加载的临时端口各不相同），供回环取流
    /// 时识别自家服务地址用；与 `py_proxy_base` 同理。
    cat_proxy_bases: Mutex<BTreeSet<String>>,
    /// 猫源订阅记录文件（App 数据目录的 `cat-sources.json`，与
    /// `node-sidecar.json` 同一目录层级）。
    cat_sources_path: PathBuf,
    cat_store: Mutex<CatStore>,
    /// 崩溃面包屑落盘接收端（App 的 debug.log）：猫源加载/卸载/调用
    /// 边界各记一行，崩后重开能从 panic 行前一行看到最后一步。
    /// 未注入时不记（测试与非 iOS 默认）。
    crumb_sink: Option<Arc<dyn Fn(&str) + Send + Sync>>,
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

/// 面包屑里的猫源调用标签：方法 + 站点 key。只取 id 里 `::` 前的
/// 站点段：分集 id 后半段可能含分享链接原文，绝不记日志
/// （cat-source.md §2.8）；无 `::` 的 id 只取前 48 字。
fn cat_call_label(method: &str, params: &Value, list_category: Option<&str>) -> String {
    match method {
        "catalog.list" => format!(
            "catalog.list {}",
            list_category
                .map(|c| c.chars().take(64).collect::<String>())
                .unwrap_or_else(|| "?".to_string())
        ),
        "media.getDetail" | "media.resolvePlayback" => {
            let key = if method == "media.getDetail" {
                "id"
            } else {
                "episodeId"
            };
            let site = params
                .get(key)
                .and_then(Value::as_str)
                .and_then(|s| s.split("::").next())
                .filter(|s| !s.is_empty())
                .map(|s| s.chars().take(48).collect::<String>())
                .unwrap_or_else(|| "?".to_string());
            format!("{method} {site}")
        }
        _ => method.to_string(),
    }
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
            // 只做静态扫描（名字与 id 已在索引里，不从代码重新派生）；
            // 猫源读 plugin.js，文件存在即保留索引里的 meta（猫源 JS
            // 没有 manifest，不跑 runtime::inspect）。
            let recovered = if indexed.runtime == "python" {
                std::fs::read_to_string(root.join(&indexed.id).join("plugin.py"))
                    .map_err(PluginError::from)
                    .and_then(|code| {
                        crate::python::scan_source(&code).map(|_| indexed.clone())
                    })
            } else if indexed.runtime == "cat" {
                std::fs::read_to_string(root.join(&indexed.id).join("plugin.js"))
                    .map_err(PluginError::from)
                    .map(|_| indexed.clone())
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
            // 迁移：v0.0.100 之前 cat_install 没声明 allowInsecureHttp，
            // 老猫源的 index.json 里就没有这一项；而猫源边车是回环 http，
            // /play 与 /proxy 返回的播放地址天生是
            // `http://127.0.0.1:<端口>/…`，没有这个声明点播会被播放校验
            // 把流全丢掉（2026-11-26 齐天圣瞳真机故障）。恢复时在内存里
            // 补上即可，不另写 index.json——下次启动恢复时还会再补。
            if entry.meta.runtime == "cat" {
                entry.meta.allow_insecure_http = true;
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
        // 猫源订阅记录：App 数据目录的 cat-sources.json（root 的父目录，
        // 与 node-sidecar.json 同一层级；父目录不存在时回落到 root 内）。
        let cat_sources_path = root
            .parent()
            .map(|p| p.join("cat-sources.json"))
            .unwrap_or_else(|| root.join("cat-sources.json"));
        let cat_store = CatStore::load(&cat_sources_path)?;
        Ok(Self {
            media_capabilities: Default::default(),
            root,
            document_reader: None,
            web_login: None,
            python_bootstrap: Arc::new(UnavailablePython),
            node_bootstrap: Arc::new(UnavailableNode),
            py_proxy_base: Default::default(),
            cat_proxy_bases: Default::default(),
            cat_sources_path,
            cat_store: Mutex::new(cat_store),
            crumb_sink: None,
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
    /// 登记已加载的猫源服务地址（回环取流时识别自家服务地址用）。
    pub(crate) fn register_cat_proxy_base(&self, base: String) {
        let base = base.trim_end_matches('/').to_string();
        self.cat_proxy_bases
            .lock()
            .unwrap()
            .insert(format!("{base}/"));
    }
    /// 该地址是不是宿主自己加载的本机服务地址（Python 边车代理入口
    /// 或已加载的猫源服务）：回环取流时豁免网络目标校验——地址由宿主
    /// 自己加载、可信，不经过系统代理直取。登记只增不减：旧地址对应
    /// 的服务已卸载、端口已死，取流失败而已（与 py_proxy_base 只记
    /// 最新值同理；端口是每次加载的临时端口）。
    pub(crate) fn is_trusted_loopback_url(&self, url: &str) -> bool {
        if self.is_py_proxy_url(url) {
            return true;
        }
        self.cat_proxy_bases
            .lock()
            .unwrap()
            .iter()
            .any(|base| url.starts_with(base.as_str()))
    }
    pub fn with_document_reader(mut self, reader: Arc<dyn DocumentReader>) -> Self {
        self.document_reader = Some(reader);
        self
    }
    /// 测试覆盖：改用另一份猫源订阅记录文件（同时重读它；读不出时保留
    /// 现有记录）。
    pub fn with_cat_sources_path(mut self, path: PathBuf) -> Self {
        self.cat_sources_path = path.clone();
        if let Ok(store) = CatStore::load(&path) {
            self.cat_store = Mutex::new(store);
        }
        self
    }

    /// 注入崩溃面包屑落盘接收端（App 的 debug.log）。只记猫源边界，
    /// 不记插件日志面板里的行；接收端只管写日志，永不抛错。
    pub fn with_crumb_sink(mut self, sink: Arc<dyn Fn(&str) + Send + Sync>) -> Self {
        self.crumb_sink = Some(sink);
        self
    }

    /// 写一行崩溃面包屑；未注入接收端时不做任何事。
    fn crumb(&self, line: &str) {
        if let Some(sink) = &self.crumb_sink {
            sink(line);
        }
    }

    /// 停掉旧 worker 并记面包屑（重载/重装前等卸载完成；只有猫源记，
    /// 其他运行时只停不停、行为与原来一致）。
    async fn stop_old_worker(&self, id: &str, w: RuntimeWorker) -> Result<()> {
        let is_cat = matches!(w, RuntimeWorker::Cat(_));
        if is_cat {
            self.crumb(&format!("[crumb] cat {id} 卸载开始"));
        }
        let r = w.stop_and_wait().await;
        if is_cat {
            self.crumb(&format!("[crumb] cat {id} 卸载完成"));
        }
        r
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
    async fn initialize(&self, p: &mut Installed, mut full: bool) -> Result<RuntimeWorker> {
        if p.retired {
            return Err(PluginError::cancelled());
        }
        if !p.entry.enabled {
            return Err(PluginError::new("UNSUPPORTED", "插件已停用"));
        }
        // 猫源连接失败的 fault 不是永久错误：源服务已死（端口失效），
        // 下次调用应重新加载而不是永远返回旧错（见
        // CatWorker::note_call_error 的注释）。其他运行时的 fault 仍按旧错返回。
        if !full {
            if let Some(w) = &p.worker {
                if let Some(error) = w.fault() {
                    if matches!(w, RuntimeWorker::Cat(_)) {
                        full = true;
                    } else {
                        p.error = Some(error.clone());
                        return Err(error);
                    }
                }
            }
        }
        if !full {
            if let Some(w) = &p.worker {
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
            // 猫源：等旧服务的卸载完成再加载新的，否则后台卸载可能落在
            // 新加载之后、把刚起的服务卸掉（同 key）。
            if matches!(w, RuntimeWorker::Cat(_)) {
                let _ = self.stop_old_worker(&p.entry.meta.id, w).await;
            } else {
                w.stop();
            }
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
        // 猫源运行时：读 plugin.js（index.config.js 存在时一并传），
        // 走 Node 边车加载（CatWorker），不走 QuickJS。
        if p.entry.meta.runtime == "cat" {
            let dir = self.root.join(&p.entry.meta.id);
            let file = std::fs::canonicalize(dir.join("plugin.js"))
                .unwrap_or_else(|_| dir.join("plugin.js"));
            let config_path = dir.join("index.config.js");
            let config_file = if config_path.exists() {
                Some(
                    std::fs::canonicalize(&config_path)
                        .unwrap_or(config_path)
                        .to_string_lossy()
                        .into_owned(),
                )
            } else {
                None
            };
            let media_caps = self.media_capabilities.lock().unwrap().clone();
            self.crumb(&format!("[crumb] cat {} 加载开始", p.entry.meta.id));
            let worker = CatWorker::spawn(
                p.entry.meta.id.clone(),
                file.to_string_lossy().into_owned(),
                config_file,
                media_caps,
                &self.node_bootstrap,
                p.entry.meta.budget("media.getDetail"),
            )
            .await
            .map_err(|e| {
                self.crumb(&format!(
                    "[crumb] cat {} 加载失败：{}",
                    p.entry.meta.id, e.code
                ));
                e
            })?;
            self.crumb(&format!("[crumb] cat {} 加载完成", p.entry.meta.id));
            // 取流豁免：媒体回环服务取源的 /proxy 回环地址时，严格模式的
            // Network::target 会拦回环；登记服务地址，取流时直取。
            self.register_cat_proxy_base(worker.base_url().to_string());
            let w = RuntimeWorker::Cat(worker);
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
                self.stop_old_worker(&meta.id, w).await?;
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
        // 猫源：先删订阅记录（失败时直接返回错误，插件不动，避免
        // “列表里没了但记录还在”或反过来的不一致）。
        if p.lock().await.entry.meta.runtime == "cat" {
            let mut store = self.cat_store.lock().unwrap();
            if store.remove(id) {
                store.save()?;
            }
        }
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
    /// 猫源 worker 的第一个 type-3 站点名（导入成功后用作插件显示名）。
    fn cat_first_site_name(worker: &RuntimeWorker) -> Option<String> {
        if let RuntimeWorker::Cat(w) = worker {
            w.sites()
                .first()
                .map(|s| s.name.clone())
                .filter(|n| !n.trim().is_empty())
        } else {
            None
        }
    }

    /// 写猫源插件目录文件（plugin.js、index.config.js、附带文件）。
    /// 附带文件的相对路径必须安全（不含 `..`、不是绝对路径）。
    fn write_cat_files(
        dir: &Path,
        plugin_js: &[u8],
        config: Option<&[u8]>,
        extra: &[(String, Vec<u8>)],
    ) -> Result<()> {
        std::fs::create_dir_all(dir)?;
        atomic_write(&dir.join("plugin.js"), plugin_js)?;
        if let Some(config) = config {
            atomic_write(&dir.join("index.config.js"), config)?;
        }
        for (rel, bytes) in extra {
            let rel_path = Path::new(rel);
            if rel_path.is_absolute()
                || rel_path.components().any(|c| {
                    matches!(c, std::path::Component::ParentDir)
                })
            {
                return Err(PluginError::invalid("猫源附带文件路径无效"));
            }
            atomic_write(&dir.join(rel_path), bytes)?;
        }
        Ok(())
    }

    /// 猫源导入的公共尾巴：写插件目录 → 建索引条目 → 初始化（失败留错）
    /// → 成功时用第一个 type-3 站点名重命名 → 保存索引。顺序照抄
    /// `install_code`。`temp_name` 是导入时的临时名。
    async fn cat_install(
        &self,
        id: &str,
        temp_name: &str,
        explicit_name: Option<&str>,
        source: Option<String>,
        plugin_js: &[u8],
        config: Option<&[u8]>,
        extra: &[(String, Vec<u8>)],
    ) -> Result<PluginInfo> {
        let _mutation = self.mutation.lock().await;
        let existing = self.plugins.read().await.get(id).cloned();
        if let Some(p) = &existing {
            if p.lock().await.entry.meta.runtime != "cat" {
                return Err(PluginError::invalid("该 ID 已被非猫源插件占用"));
            }
        }
        let dir = self.root.join(id);
        let is_new_dir = !dir.exists();
        let old_code = std::fs::read(dir.join("plugin.js")).ok();
        if let Err(e) = Self::write_cat_files(&dir, plugin_js, config, extra) {
            if is_new_dir {
                let _ = std::fs::remove_dir_all(&dir);
            }
            return Err(e);
        }
        let name: String = temp_name.chars().take(60).collect();
        let name = if name.trim().is_empty() {
            id.to_string()
        } else {
            name
        };
        // 导入时用户给了名字：它就是最终显示名，初始化成功也不用首个站点名覆盖
        // （协议里 `name` 的定义：源自 `/config` 的站点名，或用户导入时给的名字）。
        let explicit_name: Option<String> = explicit_name
            .filter(|n| !n.trim().is_empty())
            .map(|n| n.chars().take(60).collect());
        let name = explicit_name.clone().unwrap_or(name);
        // 猫源边车服务是回环 http：/play 与 /proxy 返回的播放地址就是
        // `http://127.0.0.1:<端口>/…`。没有这个声明，播放校验会把全部
        // http 流丢掉、报“没有合法可播放流”（2026-11-26 齐天圣瞳真机故障）。
        let meta = Meta::parse(json!({
            "id": id,
            "name": name,
            "version": "1.0.0",
            "protocol": 1,
            "kind": "source",
            "runtime": "cat",
            "allowInsecureHttp": true,
        }))?;
        let entry = IndexEntry {
            meta,
            enabled: true,
            source,
            origin: None,
            usable: false,
            settings: BTreeMap::new(),
            endpoint: Value::Null,
        };
        let mut old_guard = match &existing {
            Some(old) => Some(old.lock().await),
            None => None,
        };
        if let Some(old) = old_guard.as_mut() {
            if let Some(w) = old.worker.take() {
                self.stop_old_worker(id, w).await?;
            }
        }
        let mut p = Self::installed(
            dir.clone(),
            entry,
            self.strict,
            self.cooldown.clone(),
            self.node_bootstrap.clone(),
        )?;
        if let Some(old) = old_guard.as_ref() {
            p.store = old.store.clone();
        }
        if let Some(old) = old_guard.as_mut() {
            old.retired = true;
        }
        drop(old_guard);
        self.sessions.release_plugin(id);
        if p.entry.enabled {
            match self.initialize(&mut p, true).await {
                Ok(w) => {
                    if explicit_name.is_none() {
                        if let Some(site_name) = Self::cat_first_site_name(&w) {
                            p.entry.meta.name = site_name.chars().take(60).collect();
                        }
                    }
                }
                Err(e) => {
                    p.error = Some(e);
                }
            }
        }
        let mut info = Self::info(&p);
        info.can_restore = dir.join("plugin.previous.json").exists();
        self.plugins
            .write()
            .await
            .insert(id.to_string(), Arc::new(tokio::sync::Mutex::new(p)));
        if let Err(e) = self.save().await {
            if let Some(new) = self.plugins.write().await.remove(id) {
                if let Some(w) = new.lock().await.worker.take() {
                    w.stop();
                }
            }
            if let Some(bytes) = old_code {
                let _ = atomic_write(&dir.join("plugin.js"), &bytes);
            } else if is_new_dir {
                let _ = std::fs::remove_dir_all(&dir);
            }
            if let Some(old) = existing {
                old.lock().await.retired = false;
                self.plugins.write().await.insert(id.to_string(), old);
            }
            return Err(e);
        }
        Ok(info)
    }

    /// 猫源列表：Node 运行状态（读 `node-sidecar.json` 的 apiBase，
    /// 健康检查 2 秒，`v18` 开头即运行中）与全部订阅记录；每条记录用
    /// 同 id 插件的信息覆盖 `enabled` 与 `status`（插件不存在时
    /// `status: "unloaded"`）。
    pub async fn cat_list(&self) -> Result<Value> {
        let mut node = json!({"running": false});
        let info_path = self.cat_sources_path.with_file_name("node-sidecar.json");
        if let Ok(text) = std::fs::read_to_string(&info_path) {
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                if let Some(api_base) = v.get("apiBase").and_then(Value::as_str) {
                    if let Ok(client) = NodeClient::new(api_base.to_string()) {
                        if let Ok(version) = client.health(Duration::from_secs(2)).await {
                            node = json!({"running": true, "version": version});
                        }
                    }
                }
            }
        }
        let plugins = self.list().await;
        let records = self.cat_store.lock().unwrap().list();
        let mut sources = vec![];
        for record in records {
            let mut v = serde_json::to_value(&record)
                .map_err(|_| PluginError::internal("猫源订阅记录序列化失败"))?;
            if let Some(p) = plugins.iter().find(|p| p.meta.id == record.id) {
                v["enabled"] = json!(p.enabled);
                v["status"] = json!(p.status);
            } else {
                v["status"] = json!("unloaded");
            }
            sources.push(v);
        }
        Ok(json!({"node": node, "sources": sources}))
    }

    /// 启动时 Node 预热（供首屏在首帧绘制后调用）：平台不支持时返回
    /// `{supported: false}` 而不是报错；iOS 上保证边车已启动，返回
    /// `{supported: true, running: true}`。启动失败报明错，前端只显示
    /// 不阻塞启动（见 docs/developers/node-engine.md §4.1）。
    pub async fn node_ensure_started(&self) -> Result<Value> {
        if !self.node_bootstrap.is_available() {
            return Ok(json!({"supported": false}));
        }
        let info = self.node_bootstrap.ensure_started().await?;
        Ok(json!({"supported": true, "running": true, "apiBase": info.api_base}))
    }

    /// 网络导入：拉订阅 → 派生 id → 写插件目录并初始化 → 写订阅记录。
    /// 下载/校验/写文件失败时不留坏记录、不留残留文件。
    pub async fn cat_import_url(
        &self,
        url: &str,
        name: Option<String>,
    ) -> Result<Value> {
        let (published_md5, js) = fetch_subscription(url).await?;
        let id = derive_plugin_id_from_url(url)?;
        let temp_name = name
            .as_ref()
            .filter(|n| !n.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| {
                url::Url::parse(url)
                    .ok()
                    .and_then(|u| u.host_str().map(str::to_string))
                    .unwrap_or_else(|| id.clone())
            });
        let info = self
            .cat_install(&id, &temp_name, name.as_deref(), Some(url.to_string()), &js, None, &[])
            .await?;
        let record = CatSourceRecord {
            id: id.clone(),
            name: info.meta.name.clone(),
            source_type: "url".into(),
            url: Some(url.to_string()),
            md5: published_md5.unwrap_or_else(|| md5_hex(&js)),
            enabled: info.enabled,
            custom_name: name.as_ref().is_some_and(|n| !n.trim().is_empty()),
            updated_at: crate::store::now() as i64,
        };
        {
            let mut store = self.cat_store.lock().unwrap();
            store.upsert(record.clone());
            store.save()?;
        }
        Ok(json!({"source": record, "plugin": info}))
    }

    /// ZIP 导入：读 ZIP → 解压校验 → 派生 id → 写插件目录并初始化 →
    /// 写订阅记录。失败时清掉新建的插件目录、不留记录。
    pub async fn cat_import_zip(
        &self,
        path: &str,
        name: Option<String>,
    ) -> Result<Value> {
        let bytes = std::fs::read(path).map_err(|_| PluginError::invalid("无法读取 ZIP 文件"))?;
        let extracted = extract_zip(&bytes)?;
        let stem = Path::new(path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        let id = derive_plugin_id_from_stem(stem)?;
        let temp_name = name
            .as_ref()
            .filter(|n| !n.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| stem.to_string());
        let info = self
            .cat_install(
                &id,
                &temp_name,
                name.as_deref(),
                None,
                &extracted.index_js,
                extracted.config.as_deref(),
                &extracted.files,
            )
            .await?;
        let record = CatSourceRecord {
            id: id.clone(),
            name: info.meta.name.clone(),
            source_type: "zip".into(),
            url: None,
            md5: extracted.index_js_md5.clone(),
            enabled: info.enabled,
            custom_name: name.as_ref().is_some_and(|n| !n.trim().is_empty()),
            updated_at: crate::store::now() as i64,
        };
        {
            let mut store = self.cat_store.lock().unwrap();
            store.upsert(record.clone());
            store.save()?;
        }
        Ok(json!({"source": record, "plugin": info}))
    }

    /// 检查更新（仅网络导入的源）：重新拉同一订阅 URL，md5 没变返回
    /// `{changed: false}`；变了覆盖 `plugin.js`、更新记录、重新初始化。
    /// 初始化失败时恢复旧版文件与旧记录，只报失败。
    pub async fn cat_check_update(&self, id: &str) -> Result<Value> {
        let record = self
            .cat_store
            .lock()
            .unwrap()
            .get(id)
            .ok_or_else(|| PluginError::invalid("猫源订阅不存在"))?;
        if record.source_type != "url" {
            return Err(PluginError::invalid(
                "ZIP 导入的源没有订阅地址，请重新导入 ZIP",
            ));
        }
        let url = record
            .url
            .clone()
            .ok_or_else(|| PluginError::invalid("该订阅没有订阅地址"))?;
        let (published_md5, js) = fetch_subscription(&url).await?;
        let new_md5 = published_md5.unwrap_or_else(|| md5_hex(&js));
        if new_md5 == record.md5 {
            return Ok(json!({"changed": false, "source": record}));
        }
        let _mutation = self.mutation.lock().await;
        let plugin = self.get(id).await?;
        let dir = self.root.join(id);
        let old_js = std::fs::read(dir.join("plugin.js"))?;
        let mut new_record = record.clone();
        new_record.md5 = new_md5;
        new_record.updated_at = crate::store::now() as i64;
        atomic_write(&dir.join("plugin.js"), &js)?;
        {
            let mut store = self.cat_store.lock().unwrap();
            store.upsert(new_record.clone());
            if let Err(e) = store.save() {
                let _ = atomic_write(&dir.join("plugin.js"), &old_js);
                return Err(e);
            }
        }
        let outcome = {
            let mut p = plugin.lock().await;
            if p.entry.enabled {
                match self.initialize(&mut p, true).await {
                    Ok(w) => {
                        if !new_record.custom_name {
                            if let Some(site_name) = Self::cat_first_site_name(&w) {
                                p.entry.meta.name = site_name.chars().take(60).collect();
                                new_record.name = p.entry.meta.name.clone();
                            }
                        }
                        Ok(Self::info(&p))
                    }
                    Err(e) => {
                        let _ = atomic_write(&dir.join("plugin.js"), &old_js);
                        let mut store = self.cat_store.lock().unwrap();
                        store.upsert(record.clone());
                        let _ = store.save();
                        p.error = Some(e.clone());
                        Err(e)
                    }
                }
            } else {
                Ok(Self::info(&p))
            }
        };
        match outcome {
            Ok(_info) => {
                {
                    let mut store = self.cat_store.lock().unwrap();
                    store.upsert(new_record.clone());
                    store.save()?;
                }
                self.save().await?;
                Ok(json!({"changed": true, "source": new_record}))
            }
            Err(e) => {
                self.save().await?;
                Err(e)
            }
        }
    }

    /// 猫源改名：改订阅记录的显示名并置 `custom_name`（检查更新/重新导入
    /// 不再拿首个站点名覆盖），同步改插件 entry 的显示名（Node源面板的
    /// 分组标题与搜索来源列表都用它）。记录先落盘，失败时插件不动。
    pub async fn cat_rename(&self, id: &str, name: &str) -> Result<Value> {
        let name: String = name.trim().chars().take(60).collect();
        if name.is_empty() {
            return Err(PluginError::invalid("源名称不能为空"));
        }
        let _mutation = self.mutation.lock().await;
        let record = self
            .cat_store
            .lock()
            .unwrap()
            .get(id)
            .ok_or_else(|| PluginError::invalid("猫源订阅不存在"))?;
        let plugin = self.get(id).await?;
        let mut new_record = record.clone();
        new_record.name = name.clone();
        new_record.custom_name = true;
        {
            let mut store = self.cat_store.lock().unwrap();
            store.upsert(new_record.clone());
            store.save()?;
        }
        {
            let mut p = plugin.lock().await;
            p.entry.meta.name = name;
        }
        self.save().await?;
        Ok(json!({"changed": true, "source": new_record}))
    }

    /// 重新导入 ZIP（仅 ZIP 导入的源，也可用于覆盖同一订阅）：覆盖插件
    /// 目录（新 ZIP 没有 `index.config.js` 时保留旧的配置文件，不删
    /// 用户凭证；有则覆盖）→ 更新记录 → 重新初始化。初始化失败时文件
    /// 已覆盖、插件留标错（与 `import_code` 的形态一致）。
    pub async fn cat_reimport_zip(&self, id: &str, path: &str) -> Result<Value> {
        let record = self
            .cat_store
            .lock()
            .unwrap()
            .get(id)
            .ok_or_else(|| PluginError::invalid("猫源订阅不存在"))?;
        let bytes = std::fs::read(path).map_err(|_| PluginError::invalid("无法读取 ZIP 文件"))?;
        let extracted = extract_zip(&bytes)?;
        let _mutation = self.mutation.lock().await;
        let plugin = self.get(id).await?;
        let dir = self.root.join(id);
        Self::write_cat_files(
            &dir,
            &extracted.index_js,
            extracted.config.as_deref(),
            &extracted.files,
        )?;
        let mut new_record = record.clone();
        new_record.md5 = extracted.index_js_md5.clone();
        new_record.updated_at = crate::store::now() as i64;
        {
            let mut store = self.cat_store.lock().unwrap();
            store.upsert(new_record.clone());
            store.save()?;
        }
        let info = {
            let mut p = plugin.lock().await;
            if p.entry.enabled {
                match self.initialize(&mut p, true).await {
                    Ok(w) => {
                        if !new_record.custom_name {
                            if let Some(site_name) = Self::cat_first_site_name(&w) {
                                p.entry.meta.name = site_name.chars().take(60).collect();
                                new_record.name = p.entry.meta.name.clone();
                            }
                        }
                    }
                    Err(e) => {
                        p.error = Some(e);
                    }
                }
            }
            Self::info(&p)
        };
        {
            let mut store = self.cat_store.lock().unwrap();
            store.upsert(new_record.clone());
            store.save()?;
        }
        self.save().await?;
        Ok(json!({"source": new_record, "plugin": info}))
    }

    /// 导入配置：把 `path` 的文件拷进插件目录为 `index.config.js`
    ///（覆盖旧的），再重新初始化使新配置生效。只要求文件存在、非空、
    /// ≤2 MiB；真正的配置错误在初始化时由 `/cat/load` 报。
    /// 不读配置内容（可能含凭证）。
    pub async fn cat_import_config(&self, id: &str, path: &str) -> Result<Value> {
        let _record = self
            .cat_store
            .lock()
            .unwrap()
            .get(id)
            .ok_or_else(|| PluginError::invalid("猫源订阅不存在"))?;
        let bytes = std::fs::read(path).map_err(|_| PluginError::invalid("无法读取配置文件"))?;
        if bytes.is_empty() || bytes.len() > 2 * 1024 * 1024 {
            return Err(PluginError::invalid("配置文件为空或过大（上限 2 MiB）"));
        }
        let _mutation = self.mutation.lock().await;
        let plugin = self.get(id).await?;
        let dir = self.root.join(id);
        atomic_write(&dir.join("index.config.js"), &bytes)?;
        let info = {
            let mut p = plugin.lock().await;
            if p.entry.enabled {
                if let Err(e) = self.initialize(&mut p, true).await {
                    p.error = Some(e);
                }
            }
            Self::info(&p)
        };
        self.save().await?;
        Ok(json!({"plugin": info}))
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
        // 分类诊断要标出是哪个分类：分类页会给每个分类预览各调一次
        // `catalog.list`，行首不带分类 id 就对不上分区。
        let cat_list_category = if method == "catalog.list" {
            params
                .json()
                .get("categoryId")
                .and_then(Value::as_str)
                .map(str::to_string)
        } else {
            None
        };
        let is_cat = matches!(w, RuntimeWorker::Cat(_));
        let cat_label =
            is_cat.then(|| cat_call_label(method, &params.json(), cat_list_category.as_deref()));
        let started = std::time::Instant::now();
        if let Some(label) = &cat_label {
            self.crumb(&format!("[crumb] cat {id} 调用 {label} 开始"));
        }
        let result = w
            .call_scoped(
                request_id.clone(),
                method.into(),
                params,
                meta.budget(method),
                user_auth && method == "auth.submit",
            )
            .await;
        if let Some(label) = &cat_label {
            match &result {
                Ok(_) => self.crumb(&format!(
                    "[crumb] cat {id} 调用 {label} 完成（{}ms）",
                    started.elapsed().as_millis()
                )),
                Err(e) => self.crumb(&format!("[crumb] cat {id} 调用 {label} 失败：{}", e.code)),
            }
        }
        let result = result.map_err(|mut e| {
            if matches!(e.code.as_str(), "AUTH_REQUIRED" | "AUTH_EXPIRED") {
                e.plugin_id = Some(id.into());
            }
            e
        })?;
        if method == "transformResource" {
            result.validate(8 * 1024 * 1024)?;
            return Ok(result);
        }
        // 猫源详情诊断（用户 2026-11-25 定：下个版本加详情日志，复现后发
        // 日志，定位网盘资源 0 集）：转换后、校验前记一笔播放列表摘要
        // （每列表：名、集数、最长分集 id 字数），只记数量与长度，不记
        // 地址原文（网盘分集 id 可能是分享链接，绝不记日志，见
        // cat-source.md §2.8）；校验丢了分集再记一笔 warn。
        let raw = result.json();
        let cat_detail_before =
            if method == "media.getDetail" && matches!(w, RuntimeWorker::Cat(_)) {
                let summary = crate::cat::convert::detail_summary(&raw);
                if !summary.is_empty() {
                    let parts: Vec<String> = summary
                        .iter()
                        .map(|(label, count, max_len)| {
                            format!("{label} {count}集 最长id {max_len}字节")
                        })
                        .collect();
                    logs.lock().unwrap().push(
                        "info",
                        &format!("[cat] detail 转换后：{}", parts.join("；")),
                        "cat",
                        None,
                    );
                }
                Some(summary)
            } else {
                None
            };
        // 猫源列表诊断（2026-11-26：分类分区“暂无内容”但无报错，列表调用
        // 成功却 0 条）：转换后、校验前记一笔条目摘要（总数、缺 id/标题数、
        // 封面形态计数），只记数量与形态，不记地址原文（封面可能是回环
        // `/pic?url=…` 签名地址，绝不记日志，见 cat-source.md §2.8）；
        // 校验丢了条目再记一笔 warn。`catalog.list` 的校验用静默丢弃的
        // `list()`，不记 warnings，所以这里单独记。
        // 只记源返回空的那笔：分类页会给每个分类预览各调一次
        // `catalog.list`，每笔都记会把日志刷爆——2026-11-26 一次打开就
        // 触发限速丢了 302 行，要找的 warn 行也可能在其中。返回条目
        // 正常、校验没丢的调用不用记，没行就是正常。
        let cat_list_before =
            if method == "catalog.list" && matches!(w, RuntimeWorker::Cat(_)) {
                let summary = crate::cat::convert::list_summary(&raw);
                if summary.total == 0 {
                    logs.lock().unwrap().push(
                        "info",
                        &format!(
                            "[cat] list [{}] 转换后：{summary}",
                            cat_list_category.as_deref().unwrap_or("?")
                        ),
                        "cat",
                        None,
                    );
                }
                Some(summary)
            } else {
                None
            };
        // 猫源分区诊断（与上面列表同模式）：`home.getSections`（推荐分区
        // / 信息流入口）的分区数与条目数；校验丢了分区或条目再记一笔
        // warn。分区校验（content.rs）虽记 warnings，但只看警告数不清
        // 丢了多少，这里补一笔总数对照。一次打开只调一次，不刷屏。
        let cat_sections_before =
            if method == "home.getSections" && matches!(w, RuntimeWorker::Cat(_)) {
                let summary = crate::cat::convert::sections_summary(&raw);
                logs.lock().unwrap().push(
                    "info",
                    &format!("[cat] sections 转换后：{summary}"),
                    "cat",
                    None,
                );
                Some(summary)
            } else {
                None
            };
        let mut warnings = Vec::new();
        let validated = model::validate_result_with_warnings(
            method,
            raw,
            meta.allow_insecure_http,
            &mut warnings,
        );
        for warning in warnings {
            crate::native::validation_log(&logs, "warn", &warning, &request_id);
        }
        if let Err(e) = &validated {
            crate::native::validation_log(&logs, "error", &e.message, &request_id);
        }
        if let (Some(before), Ok(validated_value)) = (&cat_detail_before, &validated) {
            let after = crate::cat::convert::detail_summary(validated_value);
            for (label, count_before, max_len) in before {
                let count_after = after
                    .iter()
                    .find(|(l, _, _)| l == label)
                    .map(|(_, n, _)| *n)
                    .unwrap_or(0);
                if count_after < *count_before {
                    logs.lock().unwrap().push(
                        "warn",
                        &format!(
                            "[cat] detail 校验丢分集：{label} {count_before}集→{count_after}集（最长id {max_len}字节，上限{}字节）",
                            model::EPISODE_ID_MAX
                        ),
                        "cat",
                        None,
                    );
                }
            }
        }
        if let (Some(before), Ok(validated_value)) = (&cat_list_before, &validated) {
            let after = crate::cat::convert::list_summary(validated_value);
            if after.total < before.total {
                // 列表校验上限是 100 条（model.rs 的 catalog.list 分支）：
                // 校验后正好剩 100 条时，丢的数里含截断，不全是坏条目，
                // 行尾注明，免得把“136 条→100 条”读成丢了 36 条坏数据。
                let capped = if after.total == 100 {
                    "（触顶 100 条上限）"
                } else {
                    ""
                };
                logs.lock().unwrap().push(
                    "warn",
                    &format!(
                        "[cat] list [{}] 校验丢条目：{} 条→{} 条{}（转换后封面：{}）",
                        cat_list_category.as_deref().unwrap_or("?"),
                        before.total,
                        after.total,
                        capped,
                        before.cover_part()
                    ),
                    "cat",
                    None,
                );
            }
        }
        if let (Some(before), Ok(validated_value)) = (&cat_sections_before, &validated) {
            let after = crate::cat::convert::sections_summary(validated_value);
            if after.sections < before.sections || after.total_items < before.total_items {
                logs.lock().unwrap().push(
                    "warn",
                    &format!(
                        "[cat] sections 校验丢内容：{} 个分区→{} 个，{} 条→{} 条",
                        before.sections, after.sections, before.total_items, after.total_items
                    ),
                    "cat",
                    None,
                );
            }
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

    /// mock Node 引导：ensure_started 直接返回给定的边车地址。
    use crate::node::NodeInfo;
    use futures_util::future::BoxFuture;
    struct MockNode {
        base: String,
    }

    impl NodeBootstrap for MockNode {
        fn ensure_started(&self) -> BoxFuture<'static, Result<NodeInfo>> {
            let base = self.base.clone();
            Box::pin(async move { Ok(NodeInfo { api_base: base }) })
        }
    }

    /// mock 猫源边车（/cat/load、/cat/unload）。猫源服务单独起监听，
    /// 关掉它即模拟“端口上没服务在听”（连接被拒）；/cat/load 每次都
    /// 返回当前猫源服务的地址，由测试在“复活”时换掉。
    struct MockCatSidecar {
        cat_base: Mutex<String>,
        loads: Mutex<u32>,
        unloads: Mutex<u32>,
    }

    /// mock 猫源服务：单独起监听，`shutdown()` 优雅停机（关监听 +
    /// 关空闲 keep-alive 连接），模拟“端口上没服务在听”。只 abort
    /// serve 任务不够：axum 把已建立的连接派生到独立任务，abort 后
    /// 池里的旧连接仍能被服务，客户端会拿到陈旧 200。
    struct MockCatServer {
        base: String,
        shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl MockCatServer {
        async fn shutdown(mut self) {
            if let Some(tx) = self.shutdown_tx.take() {
                let _ = tx.send(());
            }
            let _ = self.task.await;
        }
    }

    async fn serve_mock_cat_service() -> MockCatServer {
        use axum::http::header;
        use axum::routing::post;
        // 响应带 Connection: close：客户端不池化连接，每次都重新连接，
        // 服务停掉后下一次调用必定走到新连接、拿到连接被拒。
        let router = axum::Router::new()
            .route(
                "/spider/a/3/init",
                post(|| async {
                    ([(header::CONNECTION, "close")], axum::Json(json!({})))
                }),
            )
            .route(
                "/spider/a/3/home",
                post(|| async {
                    (
                        [(header::CONNECTION, "close")],
                        axum::Json(
                            json!({"code": 0, "data": {"class": [{"type_id": "hot", "type_name": "热门"}], "filters": {}}}),
                        ),
                    )
                }),
            )
            .route(
                "/spider/a/3/category",
                post(|| async {
                    (
                        [(header::CONNECTION, "close")],
                        axum::Json(json!({"code": 0, "data": {"page": 1, "pagecount": 1, "total": 1, "list": [{"vod_id": "v1", "vod_name": "影片一", "vod_pic": "http://x/1.jpg", "vod_remarks": "8.8"}]}})),
                    )
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap()
        });
        MockCatServer {
            base,
            shutdown_tx: Some(shutdown_tx),
            task,
        }
    }

    async fn serve_mock_cat_sidecar(
        state: Arc<MockCatSidecar>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use axum::routing::post;
        async fn load(
            axum::extract::State(state): axum::extract::State<Arc<MockCatSidecar>>,
            axum::Json(_body): axum::Json<Value>,
        ) -> axum::Json<Value> {
            *state.loads.lock().unwrap() += 1;
            let base = state.cat_base.lock().unwrap().clone();
            let port: u16 = base.rsplit(':').next().unwrap().parse().unwrap();
            axum::Json(json!({
                "success": true,
                "port": port,
                "baseUrl": base,
                "sites": [{"key": "a", "name": "A 站", "type": 3, "api": "/spider/a/3"}],
            }))
        }
        async fn unload(
            axum::extract::State(state): axum::extract::State<Arc<MockCatSidecar>>,
            axum::Json(_body): axum::Json<Value>,
        ) -> axum::Json<Value> {
            *state.unloads.lock().unwrap() += 1;
            axum::Json(json!({"success": true}))
        }
        let router = axum::Router::new()
            .route("/cat/load", post(load))
            .route("/cat/unload", post(unload))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (base, server)
    }

    fn write_cat_index(root: &std::path::Path) {
        let index = json!([
            {"meta": {"id": "cat.test", "name": "测试猫源", "protocol": 1, "version": "1.0.0", "runtime": "cat", "allowInsecureHttp": true}, "enabled": true, "source": null, "origin": null, "usable": true, "settings": {}, "endpoint": {}}
        ]);
        std::fs::write(root.join("index.json"), serde_json::to_vec(&index).unwrap()).unwrap();
        std::fs::create_dir_all(root.join("cat.test")).unwrap();
        std::fs::write(root.join("cat.test/plugin.js"), "module.exports = {start(){}, stop(){}};")
            .unwrap();
    }

    #[tokio::test]
    async fn cat_install_declares_allow_insecure_http() {
        // 猫源边车是回环 http：/play 与 /proxy 返回的播放地址就是
        // `http://127.0.0.1:<端口>/…`；没有这个声明，播放校验会把
        // 全部 http 流丢掉（2026-11-26 齐天圣瞳真机故障）。初始化
        // 在无 Node 引导时失败并记在 error 里，不影响本断言。
        let dir = tempfile::tempdir().unwrap();
        let manager = PluginManager::open(dir.path().into()).unwrap();
        let info = manager
            .cat_install(
                "cat.test",
                "测试猫源",
                None,
                None,
                b"module.exports = {start(){}, stop(){}};",
                None,
                &[],
            )
            .await
            .unwrap();
        assert!(info.meta.allow_insecure_http);
    }

    #[tokio::test]
    async fn cat_rename_updates_record_and_plugin_name() {
        // 改名同时改订阅记录与插件 entry 的显示名，并置 custom_name，
        // 此后检查更新/重新导入不再拿首个站点名覆盖；空名与不存在的
        // 订阅报错，什么都不改。
        let dir = tempfile::tempdir().unwrap();
        let manager = PluginManager::open(dir.path().into()).unwrap();
        manager
            .cat_install(
                "cat.test",
                "测试猫源",
                None,
                None,
                b"module.exports = {start(){}, stop(){}};",
                None,
                &[],
            )
            .await
            .unwrap();
        {
            let mut store = manager.cat_store.lock().unwrap();
            store.upsert(CatSourceRecord {
                id: "cat.test".into(),
                name: "测试猫源".into(),
                source_type: "zip".into(),
                url: None,
                md5: "x".into(),
                enabled: true,
                custom_name: false,
                updated_at: 0,
            });
            store.save().unwrap();
        }
        let out = manager.cat_rename("cat.test", "  新名字  ").await.unwrap();
        assert_eq!(out["source"]["name"], "新名字");
        assert_eq!(out["source"]["customName"], true);
        let record = manager.cat_store.lock().unwrap().get("cat.test").unwrap();
        assert_eq!(record.name, "新名字");
        assert!(record.custom_name);
        let plugin = manager.get("cat.test").await.unwrap();
        assert_eq!(plugin.lock().await.entry.meta.name, "新名字");
        assert!(manager.cat_rename("cat.test", "   ").await.is_err());
        assert!(manager.cat_rename("cat.missing", "名字").await.is_err());
    }

    #[tokio::test]
    async fn restore_migrates_old_cat_plugin_to_allow_insecure_http() {
        // v0.0.100 之前导入的猫源，index.json 里没有 allowInsecureHttp：
        // 恢复时要在内存里补上，否则老源点播会被播放校验把流全丢掉
        // （2026-11-26 齐天圣瞳真机故障）。非猫源插件不受影响。
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let index = json!([
            {"meta": {"id": "cat.old", "name": "老猫源", "protocol": 1, "version": "1.0.0", "runtime": "cat"}, "enabled": true, "source": null, "origin": null, "usable": true, "settings": {}, "endpoint": {}},
            {"meta": {"id": "py.old", "name": "老 Python 源", "protocol": 1, "version": "1.0.0", "runtime": "python"}, "enabled": true, "source": null, "origin": null, "usable": true, "settings": {}, "endpoint": {}},
        ]);
        std::fs::write(root.join("index.json"), serde_json::to_vec(&index).unwrap()).unwrap();
        std::fs::create_dir_all(root.join("cat.old")).unwrap();
        std::fs::write(
            root.join("cat.old/plugin.js"),
            "module.exports = {start(){}, stop(){}};",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("py.old")).unwrap();
        std::fs::write(
            root.join("py.old/plugin.py"),
            "class Spider(object):\n    def homeContent(self, filter):\n        return {\"class\": []}\n",
        )
        .unwrap();
        let manager = PluginManager::open(root.into()).unwrap();
        let info = manager.list().await;
        let cat = info.iter().find(|p| p.meta.id == "cat.old").unwrap();
        assert!(cat.meta.allow_insecure_http);
        let py = info.iter().find(|p| p.meta.id == "py.old").unwrap();
        assert!(!py.meta.allow_insecure_http);
    }

    #[tokio::test]
    async fn cat_worker_reloads_after_connection_failure() {
        // 猫源服务 #1 活着；边车 /cat/load 指过去。
        let cat_server1 = serve_mock_cat_service().await;
        let sidecar = Arc::new(MockCatSidecar {
            cat_base: Mutex::new(cat_server1.base.clone()),
            loads: Mutex::new(0),
            unloads: Mutex::new(0),
        });
        let (sidecar_base, sidecar_server) = serve_mock_cat_sidecar(sidecar.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_cat_index(root);
        let manager = PluginManager::open(root.into())
            .unwrap()
            .with_node_bootstrap(Arc::new(MockNode { base: sidecar_base }));
        // 第一次调用成功（加载服务 #1，分类出来）。
        let first = manager
            .call("cat.test", "catalog.getCategories", json!({}), "t1".to_string())
            .await
            .unwrap();
        assert_eq!(first[0]["id"].as_str(), Some("a::hot"));
        assert_eq!(first[0]["label"].as_str(), Some("热门"));
        assert_eq!(*sidecar.loads.lock().unwrap(), 1);
        // 关掉猫源服务 #1：再调报连接失败，worker 记 fault。
        cat_server1.shutdown().await;
        let err = manager
            .call("cat.test", "catalog.getCategories", json!({}), "t2".to_string())
            .await
            .unwrap_err();
        assert!(err.message.contains("猫源服务连接失败"), "{}", err.message);
        // 起猫源服务 #2（新端口），边车 /cat/load 改指过去；再调一次应
        // 重新加载并成功，而不是永远返回上面那个旧错。
        let cat_server2 = serve_mock_cat_service().await;
        *sidecar.cat_base.lock().unwrap() = cat_server2.base.clone();
        let third = manager
            .call("cat.test", "catalog.getCategories", json!({}), "t3".to_string())
            .await
            .unwrap();
        assert_eq!(third[0]["id"].as_str(), Some("a::hot"));
        assert_eq!(*sidecar.loads.lock().unwrap(), 2);
        // 重载前旧服务的卸载已完成（stop_and_wait 等到了卸载往返），
        // 不是 fire-and-forget。
        assert!(*sidecar.unloads.lock().unwrap() >= 1);
        // 故障已清：插件信息里不再带错。
        let info = manager.list().await;
        let cat = info.iter().find(|p| p.meta.id == "cat.test").unwrap();
        assert!(cat.error.is_none(), "{:?}", cat.error);
        sidecar_server.abort();
    }

    #[tokio::test]
    async fn cat_service_base_is_trusted_loopback() {
        // 2026-11-26 齐天圣瞳：猫源 /play 返回回环 /proxy 地址，媒体
        // 回环服务取流时 Network::target 在严格模式下拦回环，每条流都
        // 取不到、报“已尝试可用播放流”。加载后登记服务地址，取流豁免。
        let cat_server = serve_mock_cat_service().await;
        let sidecar = Arc::new(MockCatSidecar {
            cat_base: Mutex::new(cat_server.base.clone()),
            loads: Mutex::new(0),
            unloads: Mutex::new(0),
        });
        let (sidecar_base, sidecar_server) = serve_mock_cat_sidecar(sidecar.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_cat_index(root);
        let manager = PluginManager::open(root.into())
            .unwrap()
            .with_node_bootstrap(Arc::new(MockNode { base: sidecar_base }));
        let proxy_url = format!("{}/proxy/ali/src/redirect/x", cat_server.base);
        // 加载前：还不知道猫源服务地址，不放行。
        assert!(!manager.is_trusted_loopback_url(&proxy_url));
        let _ = manager
            .call("cat.test", "catalog.getCategories", json!({}), "t1".to_string())
            .await
            .unwrap();
        // 加载后：该服务的回环地址放行；别的端口不放行。
        assert!(manager.is_trusted_loopback_url(&proxy_url));
        assert!(!manager.is_trusted_loopback_url("http://127.0.0.1:1/proxy/x"));
        assert!(!manager.is_trusted_loopback_url("http://127.0.0.1:9999/proxy/x"));
        sidecar_server.abort();
    }

    #[test]
    fn cat_call_label_keeps_only_the_site_segment() {
        // 列表：分类 id 全记（站点::分类，都是短 id）。
        assert_eq!(
            cat_call_label(
                "catalog.list",
                &json!({"categoryId": "nodejs_douban::1"}),
                Some("nodejs_douban::1")
            ),
            "catalog.list nodejs_douban::1"
        );
        // 详情：只记 `::` 前的站点段，分集 id 后半段（可能含分享链接原文）绝不出现。
        assert_eq!(
            cat_call_label(
                "media.getDetail",
                &json!({"id": "nodejs_mypan::pan@@@115@@@0"}),
                None
            ),
            "media.getDetail nodejs_mypan"
        );
        // 点播：episodeId 同理。
        assert_eq!(
            cat_call_label(
                "media.resolvePlayback",
                &json!({"episodeId": "nodejs_douban::https://example.invalid/share/abc"}),
                None
            ),
            "media.resolvePlayback nodejs_douban"
        );
        // 其他方法只记方法名，不记参数（搜索词、URL 都不进日志）。
        assert_eq!(
            cat_call_label("media.search", &json!({"keyword": "齐天圣瞳"}), None),
            "media.search"
        );
        // 无 id 时不崩，只记方法。
        assert_eq!(
            cat_call_label("media.getDetail", &json!({}), None),
            "media.getDetail ?"
        );
    }

    #[tokio::test]
    async fn cat_crumb_sink_records_load_and_call() {
        // 面包屑接收端记下每一行：一次分类调用应留下加载开始/完成
        // 与调用开始/完成四行，崩后重开能从 panic 行前一行看到最后一步。
        let crumbs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_crumbs = crumbs.clone();
        let cat_server = serve_mock_cat_service().await;
        let sidecar = Arc::new(MockCatSidecar {
            cat_base: Mutex::new(cat_server.base.clone()),
            loads: Mutex::new(0),
            unloads: Mutex::new(0),
        });
        let (sidecar_base, sidecar_server) = serve_mock_cat_sidecar(sidecar.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_cat_index(root);
        let manager = PluginManager::open(root.into())
            .unwrap()
            .with_node_bootstrap(Arc::new(MockNode { base: sidecar_base }))
            .with_crumb_sink(Arc::new(move |line: &str| {
                sink_crumbs.lock().unwrap().push(line.to_string());
            }));
        let first = manager
            .call(
                "cat.test",
                "catalog.getCategories",
                json!({}),
                "t1".to_string(),
            )
            .await
            .unwrap();
        assert_eq!(first[0]["id"].as_str(), Some("a::hot"));
        let lines = crumbs.lock().unwrap().clone();
        assert!(
            lines.iter().any(|l| l == "[crumb] cat cat.test 加载开始"),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| l == "[crumb] cat cat.test 加载完成"),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l == "[crumb] cat cat.test 调用 catalog.getCategories 开始"),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("[crumb] cat cat.test 调用 catalog.getCategories 完成（")),
            "{lines:?}"
        );
        sidecar_server.abort();
    }
}
