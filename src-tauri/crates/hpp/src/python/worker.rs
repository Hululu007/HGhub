//! Python worker：把 HPP 方法调用翻译成边车 /call。
//!
//! 与 JS 的 `Worker`（runtime.rs）对外同一组接口，供 `PluginManager`
//! 用 `RuntimeWorker` 枚举统一持有：`methods / media / closed / fault /
//! stop / stop_and_wait / cancel / call / call_scoped`。
//!
//! 解释器本身不在 Rust 进程里：边车（CPython + Flask）由平台引导启动
//! 一次（`PythonBootstrap`），之后所有蜘蛛调用都走回环 HTTP。
//! 边车不可用（非 iOS 目标）时引导返回 `UNSUPPORTED`，初始化失败，
//! 插件保留文件、状态 failed、可重试。
use crate::{
    capabilities::MediaCapabilities,
    python::{
        client::{LoadInfo, SidecarClient, SidecarInfo},
        convert,
    },
    wire::Transfer,
    PluginError, Result,
};
use futures_util::future::BoxFuture;
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;

/// 平台引导：保证边车已启动并返回入口地址。
/// iOS 实现负责启动 CPython 解释器线程；非 iOS 目标返回 UNSUPPORTED。
pub trait PythonBootstrap: Send + Sync {
    fn ensure_started(&self) -> BoxFuture<'static, Result<SidecarInfo>>;
}

/// 非 iOS 目标的桩引导：编译通过，但任何 Python 插件初始化都报明错。
pub struct UnavailablePython;

impl PythonBootstrap for UnavailablePython {
    fn ensure_started(&self) -> BoxFuture<'static, Result<SidecarInfo>> {
        Box::pin(async {
            Err(PluginError::new(
                "UNSUPPORTED",
                "Python 运行时仅支持 iOS，当前平台不可用",
            ))
        })
    }
}

#[derive(Clone)]
pub struct PyWorker {
    plugin_id: String,
    /// 边车客户端可换：边车监听在自愈重启后可能换端口，调用时连不上
    /// 就重探地址换一个（见 call_with_refresh）。
    client: Arc<Mutex<Arc<SidecarClient>>>,
    /// 重探边车地址的引导（重读 info 文件 + 健康检查）。
    bootstrap: Arc<dyn PythonBootstrap>,
    methods: Vec<String>,
    /// 边车探测到的蜘蛛方法名（TVBox 名字），供调用时选来源方法。
    spider_methods: Vec<String>,
    media: MediaCapabilities,
    fault: Arc<Mutex<Option<PluginError>>>,
    closed: Arc<AtomicBool>,
    tokens: Arc<Mutex<HashMap<String, CancellationToken>>>,
}

impl PyWorker {
    /// 加载蜘蛛并探测方法面。`media_base` 是宿主回环地址（取数桥目标）。
    /// 成功时返回 worker 与边车代理入口地址（供回环取流豁免用）。
    pub async fn spawn(
        plugin_id: String,
        code: String,
        media_base: String,
        media: MediaCapabilities,
        bootstrap: &Arc<dyn PythonBootstrap>,
        budget: Duration,
    ) -> Result<(Self, String)> {
        let info = bootstrap.ensure_started().await?;
        let client = Arc::new(SidecarClient::new(info.api_base.clone())?);
        let health = client.health(Duration::from_secs(5)).await?;
        let loaded: LoadInfo = client
            .load(&plugin_id, &code, &media_base, budget)
            .await?;
        convert::check_required(&loaded.methods)?;
        let methods = convert::hpp_methods(&loaded.methods);
        // init(extend="")：只有蜘蛛自己实现了 init 才调；没实现的用基类
        // 空实现（类式，BaseSpider.init 是 pass）或直接跳过（函数式），
        // 不因此初始化失败。v1 无配置来源，传空串；以后 meta.settings
        // 接入时再把设置 JSON 串进来。init 抛异常即初始化失败。
        if loaded.methods.iter().any(|m| m == "init") {
            client
                .call(&plugin_id, "init", vec![Value::String(String::new())], budget)
                .await?;
        }
        Ok((
            Self {
                plugin_id,
                client: Arc::new(Mutex::new(client)),
                bootstrap: bootstrap.clone(),
                methods,
                spider_methods: loaded.methods,
                media,
                fault: Arc::new(Mutex::new(None)),
                closed: Arc::new(AtomicBool::new(false)),
                tokens: Arc::new(Mutex::new(HashMap::new())),
            },
            health.proxy_base,
        ))
    }

    pub fn methods(&self) -> &[String] {
        &self.methods
    }

    pub fn media(&self) -> &MediaCapabilities {
        &self.media
    }

    pub fn closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    pub fn fault(&self) -> Option<PluginError> {
        self.fault.lock().unwrap().clone()
    }

    pub fn stop(&self) {
        self.closed.store(true, Ordering::Relaxed);
        for t in self.tokens.lock().unwrap().values() {
            t.cancel();
        }
    }

    pub async fn stop_and_wait(&self) -> Result<()> {
        self.stop();
        // 尽力卸载边车里的蜘蛛：失败不报错，/load 同 key 会覆盖。
        self.current_client()
            .unload(&self.plugin_id, Duration::from_secs(1))
            .await;
        Ok(())
    }

    fn current_client(&self) -> Arc<SidecarClient> {
        self.client.lock().unwrap().clone()
    }

    /// 边车连不上（Connect 被拒）时返回 true：回环上只有“边车没在听”
    /// 这一种传输失败；/call 500 等蜘蛛错误走别的文案，不触发刷新。
    fn is_sidecar_connect(e: &PluginError) -> bool {
        e.code == "INTERNAL" && e.message.contains("边车连接失败")
    }

    /// 记一次调用故障：连不上说明解释器已死或地址已换，下次调用重新
    /// 初始化；超时只换文案，不记故障。
    fn note_call_error(&self, mut e: PluginError) -> PluginError {
        if Self::is_sidecar_connect(&e) {
            *self.fault.lock().unwrap() = Some(e.clone());
        }
        if e.code == "TIMEOUT" {
            e = PluginError::timeout();
        }
        e
    }

    /// 重新探测边车地址并换一个客户端（用调用预算兜底，超时就失败，
    /// 调用方报原来的错）。
    async fn refresh_client(&self, budget: Duration) -> Result<()> {
        let info = tokio::time::timeout(budget, self.bootstrap.ensure_started())
            .await
            .map_err(|_| PluginError::timeout())??;
        let fresh = SidecarClient::new(info.api_base)?;
        *self.client.lock().unwrap() = Arc::new(fresh);
        Ok(())
    }

    /// 调一次边车；连不上时重新探测地址并重试一次。
    ///
    /// 边车监听在自愈重启后可能换端口（main.py 重绑时 55001 被占就落
    /// 临时端口，info 文件会更新），而 worker 手里是旧地址的客户端，
    /// 直接重试只会再撞旧端口。刷新走 bootstrap.ensure_started（重读
    /// info 文件 + 健康检查）。请求从未到过边车（TCP 都没连上），方法
    /// 没有进入蜘蛛，重试不会重放已执行的方法。
    async fn call_with_refresh(
        &self,
        key: &str,
        method_name: &str,
        args: Vec<Value>,
        budget: Duration,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        let first = {
            let client = self.current_client();
            tokio::select! {
                _ = cancel.cancelled() => Err(PluginError::cancelled()),
                r = client.call(key, method_name, args.clone(), budget) => r,
            }
        };
        match first {
            Ok(v) => Ok(v),
            Err(e) if Self::is_sidecar_connect(&e) => {
                if self.refresh_client(budget).await.is_ok() {
                    let client = self.current_client();
                    let retry = tokio::select! {
                        _ = cancel.cancelled() => Err(PluginError::cancelled()),
                        r = client.call(key, method_name, args, budget) => r,
                    };
                    retry.map_err(|e| self.note_call_error(e))
                } else {
                    Err(self.note_call_error(e))
                }
            }
            Err(e) => Err(self.note_call_error(e)),
        }
    }

    pub fn cancel(&self, id: &str) {
        if let Some(t) = self.tokens.lock().unwrap().get(id) {
            t.cancel();
        }
    }

    pub async fn call(
        &self,
        id: String,
        method: String,
        params: Transfer,
        budget: Duration,
    ) -> Result<Transfer> {
        self.call_scoped(id, method, params, budget, false).await
    }

    pub async fn call_scoped(
        &self,
        id: String,
        method: String,
        params: Transfer,
        budget: Duration,
        _user_auth: bool,
    ) -> Result<Transfer> {
        if self.closed() {
            return Err(PluginError::internal("Python 运行时已停止"));
        }
        params.validate(8 * 1024 * 1024)?;
        let args = params.json();
        // home.getSections 的来源按蜘蛛实际有的方法选：有 homeVideoContent
        // 用它，否则退回 homeContent 的 list（两者不能同时调，避免两份推荐打架）。
        let (spider_method, spider_args, home_section) =
            if method == "home.getSections" {
                if self.spider_methods.iter().any(|m| m == "homeVideoContent") {
                    ("homeVideoContent", vec![], Some(("hot", "row")))
                } else {
                    ("homeContent", vec![], Some(("home", "grid")))
                }
            } else {
                let (m, a) = convert::dispatch(&method, &args).ok_or_else(|| {
                    PluginError::new("UNSUPPORTED", "Python 蜘蛛不支持该方法")
                })?;
                (m, a, None)
            };
        let cancel = CancellationToken::new();
        {
            let mut tokens = self.tokens.lock().unwrap();
            if tokens.contains_key(&id) {
                return Err(PluginError::invalid("requestId 重复"));
            }
            tokens.insert(id.clone(), cancel.clone());
        }
        let key = self.plugin_id.clone();
        let method_name = spider_method.to_string();
        let result = self
            .call_with_refresh(&key, &method_name, spider_args, budget, &cancel)
            .await;
        self.tokens.lock().unwrap().remove(&id);
        let raw = match result {
            Ok(v) => v,
            Err(e) => return Err(e),
        };
        let converted = if let Some((section_id, layout)) = home_section {
            convert::sections(&raw, section_id, layout)?
        } else {
            self.convert(&method, &raw, &args)?
        };
        Ok(Transfer::from_json(converted))
    }

    fn convert(&self, method: &str, raw: &Value, args: &Value) -> Result<Value> {
        match method {
            "catalog.getCategories" => convert::categories(raw),
            "feed.list" => convert::page(raw),
            "catalog.list" => convert::page(raw),
            "search.search" => convert::page(raw),
            "media.getDetail" => convert::detail(raw),
            "media.resolvePlayback" => {
                let flag = args
                    .get("playlistId")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                convert::playback(raw, flag)
            }
            _ => Err(PluginError::new("UNSUPPORTED", "Python 蜘蛛不支持该方法")),
        }
    }
}

/// JS Worker 与 Python Worker 的统一持有：对外同一组方法。
#[derive(Clone)]
pub enum RuntimeWorker {
    Js(crate::runtime::Worker),
    Py(PyWorker),
    Cat(crate::cat::worker::CatWorker),
}

impl RuntimeWorker {
    pub fn methods(&self) -> &[String] {
        match self {
            RuntimeWorker::Js(w) => &w.methods,
            RuntimeWorker::Py(w) => w.methods(),
            RuntimeWorker::Cat(w) => w.methods(),
        }
    }

    pub fn media(&self) -> &MediaCapabilities {
        match self {
            RuntimeWorker::Js(w) => &w.media,
            RuntimeWorker::Py(w) => w.media(),
            RuntimeWorker::Cat(w) => w.media(),
        }
    }

    pub fn closed(&self) -> bool {
        match self {
            RuntimeWorker::Js(w) => w.closed(),
            RuntimeWorker::Py(w) => w.closed(),
            RuntimeWorker::Cat(w) => w.closed(),
        }
    }

    pub fn fault(&self) -> Option<PluginError> {
        match self {
            RuntimeWorker::Js(w) => w.fault(),
            RuntimeWorker::Py(w) => w.fault(),
            RuntimeWorker::Cat(w) => w.fault(),
        }
    }

    pub fn stop(&self) {
        match self {
            RuntimeWorker::Js(w) => w.stop(),
            RuntimeWorker::Py(w) => w.stop(),
            RuntimeWorker::Cat(w) => w.stop(),
        }
    }

    pub async fn stop_and_wait(&self) -> Result<()> {
        match self {
            RuntimeWorker::Js(w) => w.stop_and_wait().await,
            RuntimeWorker::Py(w) => w.stop_and_wait().await,
            RuntimeWorker::Cat(w) => w.stop_and_wait().await,
        }
    }

    pub fn cancel(&self, id: &str) {
        match self {
            RuntimeWorker::Js(w) => w.cancel(id),
            RuntimeWorker::Py(w) => w.cancel(id),
            RuntimeWorker::Cat(w) => w.cancel(id),
        }
    }

    pub async fn call(
        &self,
        id: String,
        method: String,
        params: Transfer,
        budget: Duration,
    ) -> Result<Transfer> {
        match self {
            RuntimeWorker::Js(w) => w.call(id, method, params, budget).await,
            RuntimeWorker::Py(w) => w.call(id, method, params, budget).await,
            RuntimeWorker::Cat(w) => w.call(id, method, params, budget).await,
        }
    }

    pub async fn call_scoped(
        &self,
        id: String,
        method: String,
        params: Transfer,
        budget: Duration,
        user_auth: bool,
    ) -> Result<Transfer> {
        match self {
            RuntimeWorker::Js(w) => {
                w.call_scoped(id, method, params, budget, user_auth).await
            }
            RuntimeWorker::Py(w) => {
                w.call_scoped(id, method, params, budget, user_auth).await
            }
            RuntimeWorker::Cat(w) => {
                w.call_scoped(id, method, params, budget, user_auth).await
            }
        }
    }
}
