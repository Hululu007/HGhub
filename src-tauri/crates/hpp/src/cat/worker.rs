//! 猫源 worker：把 HPP 方法调用翻译成对源 HTTP 服务的直接调用。
//!
//! 与 `PyWorker` 对外同一组接口，供 `PluginManager` 用 `RuntimeWorker`
//! 枚举统一持有：`methods / media / closed / fault / stop /
//! stop_and_wait / cancel / call / call_scoped`。
//!
//! 源的 HTTP 服务跑在 Node 边车加载的模块里（`host.js` 的 `/cat/load`
//! 已调 `start(config)` 并探活端口）；内容调用不经过边车中转，Rust
//! 侧直接调源的服务（回环地址，`no_proxy` 的 loopback 客户端，与
//! `NodeClient` 同款）。源服务地址固定，不换端口：连不上直接报错，
//! 不重探；`CatClient` 只在加载（spawn）与卸载（stop）时用。
use crate::{
    capabilities::MediaCapabilities,
    cat::{convert, read_capped_body},
    node::{CatClient, CatSite, NodeBootstrap},
    python::convert as py,
    wire::Transfer,
    PluginError, Result,
};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;

fn truncate_error(text: &str) -> String {
    const LIMIT: usize = 1000;
    let mut out: String = text.chars().take(LIMIT).collect();
    if text.chars().count() > LIMIT {
        out.push('…');
    }
    out
}

#[derive(Clone)]
pub struct CatWorker {
    plugin_id: String,
    /// 边车客户端可换：边车监听地址可能变，保留重探能力；
    /// 内容调用不走它，只在加载与卸载时用。
    client: Arc<Mutex<Arc<CatClient>>>,
    /// 调源 HTTP 服务用：`no_proxy()` + Chrome149，与 `NodeClient` 同款。
    http: wreq::Client,
    base_url: String,
    /// 仅 type 3 站点（普通站源；type 4 推送不进资源库）。
    sites: Vec<CatSite>,
    methods: Vec<String>,
    media: MediaCapabilities,
    fault: Arc<Mutex<Option<PluginError>>>,
    closed: Arc<AtomicBool>,
    tokens: Arc<Mutex<HashMap<String, CancellationToken>>>,
    /// 已调过 `/init` 的站点 key（每站点在 worker 生命周期内只调一次）。
    inited: Arc<Mutex<HashSet<String>>>,
}

impl CatWorker {
    /// 加载猫源并探测站点。`config_file` 是 `index.config.js` 的绝对路径
    ///（不存在时传 None，边车调 `start({})`）。
    pub async fn spawn(
        plugin_id: String,
        file: String,
        config_file: Option<String>,
        media: MediaCapabilities,
        node_bootstrap: &Arc<dyn NodeBootstrap>,
        budget: Duration,
    ) -> Result<Self> {
        let info = node_bootstrap.ensure_started().await?;
        let client = Arc::new(CatClient::new(info.api_base.clone())?);
        let loaded = client
            .load(&plugin_id, &file, config_file.as_deref(), budget)
            .await?;
        convert::check_required(&loaded.sites)?;
        let sites: Vec<CatSite> = loaded
            .sites
            .into_iter()
            .filter(|s| s.site_type == 3)
            .collect();
        let http = wreq::Client::builder()
            .emulation(wreq_util::Profile::Chrome149)
            .no_proxy()
            .build()
            .map_err(|_| PluginError::internal("创建猫源 HTTP 客户端失败"))?;
        Ok(Self {
            plugin_id,
            client: Arc::new(Mutex::new(client)),
            http,
            base_url: loaded.base_url.trim_end_matches('/').to_string(),
            sites,
            methods: convert::hpp_methods(),
            media,
            fault: Arc::new(Mutex::new(None)),
            closed: Arc::new(AtomicBool::new(false)),
            tokens: Arc::new(Mutex::new(HashMap::new())),
            inited: Arc::new(Mutex::new(HashSet::new())),
        })
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

    /// 供 manager 导入后取第一个站点名。
    pub fn sites(&self) -> &[CatSite] {
        &self.sites
    }

    pub fn stop(&self) {
        self.closed.store(true, Ordering::Relaxed);
        for t in self.tokens.lock().unwrap().values() {
            t.cancel();
        }
        // 同步方法里不能 await：后台尽力卸载边车里的源（调 stop() 关服务）。
        let client = self.current_client();
        let plugin_id = self.plugin_id.clone();
        tokio::spawn(async move {
            client.unload(&plugin_id, Duration::from_secs(1)).await;
        });
    }

    pub async fn stop_and_wait(&self) -> Result<()> {
        self.stop();
        // 再调一次 unload 并等待（1 秒超时、尽力而为）；重复卸载无害，
        // `/cat/unload` 是幂等的。
        self.current_client()
            .unload(&self.plugin_id, Duration::from_secs(1))
            .await;
        Ok(())
    }

    fn current_client(&self) -> Arc<CatClient> {
        self.client.lock().unwrap().clone()
    }

    /// 记一次调用故障：连不上说明源服务已死，下次初始化重新加载；
    /// 超时只换文案，不记故障。
    fn note_call_error(&self, mut e: PluginError) -> PluginError {
        if e.code == "INTERNAL" && e.message.contains("猫源服务连接失败") {
            *self.fault.lock().unwrap() = Some(e.clone());
        }
        if e.code == "TIMEOUT" {
            e = PluginError::timeout();
        }
        e
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
            return Err(PluginError::internal("猫源运行时已停止"));
        }
        params.validate(8 * 1024 * 1024)?;
        let args = params.json();
        let call = convert::dispatch(&method, &args, &self.sites).ok_or_else(|| {
            convert::decode_id_error(&method, &args, &self.sites)
                .unwrap_or_else(|| PluginError::new("UNSUPPORTED", "猫源不支持该方法"))
        })?;
        let cancel = CancellationToken::new();
        {
            let mut tokens = self.tokens.lock().unwrap();
            if tokens.contains_key(&id) {
                return Err(PluginError::invalid("requestId 重复"));
            }
            tokens.insert(id.clone(), cancel.clone());
        }
        let result = match &call {
            convert::CatCall::Single {
                site,
                endpoint,
                body,
            } => {
                self.ensure_init(site).await;
                self.call_source(site, endpoint, body.clone(), budget, &cancel)
                    .await
                    .and_then(|data| self.convert_single(&method, data, site, &args))
            }
            convert::CatCall::FanOut { endpoint, body } => {
                self.call_fanout(&method, endpoint, body.clone(), budget, &cancel)
                    .await
            }
        };
        self.tokens.lock().unwrap().remove(&id);
        result.map(Transfer::from_json)
    }

    /// 每站点在 worker 生命周期内第一次被用时调一次 `POST <api>/init`
    ///（5 秒超时、尽力而为、失败不阻塞）；之后不再重复调。
    async fn ensure_init(&self, site: &CatSite) {
        {
            let mut inited = self.inited.lock().unwrap();
            if !inited.insert(site.key.clone()) {
                return;
            }
        }
        let url = format!(
            "{}{}/init",
            self.base_url,
            site.api.trim_end_matches('/')
        );
        let _ = self
            .http
            .post(&url)
            .json(&json!({}))
            .timeout(Duration::from_secs(5))
            .send()
            .await;
    }

    /// 调一次源服务：POST `{base_url}{api}{endpoint}`，JSON body，
    /// 15 秒超时（不超调用预算），`no_proxy` 回环；响应体上限 8 MiB。
    /// 返回解包后的 `data`。非 2xx / 非 JSON / `{code != 0}` 转
    /// `PluginError`（code 与 message 透传，截断 1000 字）。
    async fn call_source(
        &self,
        site: &CatSite,
        endpoint: &str,
        body: Value,
        budget: Duration,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        let url = format!(
            "{}{}{}",
            self.base_url,
            site.api.trim_end_matches('/'),
            endpoint
        );
        let timeout = budget.min(Duration::from_secs(15));
        let http = self.http.clone();
        let fut = async move {
            let rsp = http
                .post(&url)
                .json(&body)
                .timeout(timeout)
                .send()
                .await
                .map_err(|e| {
                    if e.is_timeout() {
                        PluginError::timeout()
                    } else {
                        PluginError::new("INTERNAL", format!("猫源服务连接失败：{e}"))
                    }
                })?;
            let status = rsp.status();
            let bytes = read_capped_body(rsp, 8 * 1024 * 1024, "猫源响应体过大").await?;
            let v: Value = serde_json::from_slice(&bytes)
                .map_err(|_| PluginError::new("INTERNAL", "猫源返回非 JSON"))?;
            if !status.is_success() {
                return Err(Self::source_error(status, &v));
            }
            convert::unwrap_envelope(&v)
        };
        let result = tokio::select! {
            _ = cancel.cancelled() => Err(PluginError::cancelled()),
            r = fut => r,
        };
        result.map_err(|e| self.note_call_error(e))
    }

    /// 源的 HTTP 错误转 `PluginError`：HTTP 500 且 body 是 `{error}`
    /// 时用它的 error 文案；否则 code 与 message 透传。
    fn source_error(status: wreq::StatusCode, v: &Value) -> PluginError {
        if let Some(text) = v.get("error").and_then(Value::as_str) {
            return PluginError::new("UPSTREAM_ERROR", truncate_error(text));
        }
        if let Some(code) = v.get("code").and_then(Value::as_i64) {
            let message = v
                .get("message")
                .or_else(|| v.get("msg"))
                .and_then(Value::as_str)
                .unwrap_or("上游返回失败");
            return PluginError::new(
                "UPSTREAM_ERROR",
                format!("上游返回失败（code {code}）：{}", truncate_error(message)),
            );
        }
        PluginError::new(
            "UPSTREAM_ERROR",
            format!("猫源服务返回 HTTP {status}"),
        )
    }

    /// 单站点调用的结果转换。
    fn convert_single(
        &self,
        method: &str,
        data: Value,
        site: &CatSite,
        args: &Value,
    ) -> Result<Value> {
        match method {
            "catalog.getCategories" => {
                let prefixed = convert::prefix_categories(&data, std::slice::from_ref(site))?;
                py::categories(&prefixed)
            }
            "catalog.list" => {
                let prefixed = convert::prefix_page_items(&data, site)?;
                py::page(&prefixed)
            }
            "media.getDetail" => convert::rewrite_detail(&data, site),
            "media.resolvePlayback" => {
                let flag = args
                    .get("playlistId")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let flag = convert::decode_id(flag, &self.sites)
                    .map(|(_, raw)| raw)
                    .unwrap_or_else(|_| flag.to_string());
                convert::playback(&data, &flag)
            }
            _ => Err(PluginError::new("UNSUPPORTED", "猫源不支持该方法")),
        }
    }

    /// 扇出调用：对每个 type-3 站点各调一次，合并结果。单个站点失败
    /// 只跳过该站；全部失败时返回第一个站点的错误。
    async fn call_fanout(
        &self,
        method: &str,
        endpoint: &str,
        body: Value,
        budget: Duration,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        let mut first_err: Option<PluginError> = None;
        match method {
            "catalog.getCategories" => {
                let mut classes = vec![];
                for site in &self.sites {
                    self.ensure_init(site).await;
                    match self
                        .call_source(site, endpoint, body.clone(), budget, cancel)
                        .await
                    {
                        Ok(data) => {
                            if let Some(arr) = data.get("class").and_then(Value::as_array) {
                                for c in arr {
                                    let mut c = c.clone();
                                    if let Some(obj) = c.as_object_mut() {
                                        obj.insert(
                                            "_cat_site".into(),
                                            json!(site.key),
                                        );
                                    }
                                    classes.push(c);
                                }
                            }
                        }
                        Err(e) => {
                            if first_err.is_none() {
                                first_err = Some(e);
                            }
                        }
                    }
                }
                if classes.is_empty() {
                    if let Some(e) = first_err {
                        return Err(e);
                    }
                }
                let merged = json!({"class": classes});
                let prefixed = convert::prefix_categories(&merged, &self.sites)?;
                py::categories(&prefixed)
            }
            "home.getSections" | "feed.list" => {
                let layout = if method == "home.getSections" {
                    "grid"
                } else {
                    "row"
                };
                let mut sections = vec![];
                let mut total = 0u64;
                for site in &self.sites {
                    self.ensure_init(site).await;
                    match self
                        .call_source(site, endpoint, body.clone(), budget, cancel)
                        .await
                    {
                        Ok(data) => {
                            // 无 list 时为空分区（设计 §6.1）；条目 id 也要
                            // 加站点前缀，否则多站点时详情解不出站点。
                            let mut data = data;
                            if data.get("list").is_none() {
                                if let Some(obj) = data.as_object_mut() {
                                    obj.insert("list".into(), json!([]));
                                }
                            }
                            let prefixed = match convert::prefix_page_items(&data, site) {
                                Ok(v) => v,
                                Err(e) => {
                                    if first_err.is_none() {
                                        first_err = Some(e);
                                    }
                                    continue;
                                }
                            };
                            match py::sections(&prefixed, &site.key, layout) {
                                Ok(page) => {
                                    if let Some(items) =
                                        page.get("items").and_then(Value::as_array)
                                    {
                                        sections.extend(items.iter().cloned());
                                    }
                                    total += page.get("total").and_then(Value::as_u64).unwrap_or(0);
                                }
                                Err(e) => {
                                    if first_err.is_none() {
                                        first_err = Some(e);
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            if first_err.is_none() {
                                first_err = Some(e);
                            }
                        }
                    }
                }
                if sections.is_empty() {
                    if let Some(e) = first_err {
                        return Err(e);
                    }
                }
                Ok(json!({"items": sections, "next": null, "total": total}))
            }
            "search.search" => {
                let mut items = vec![];
                let mut total = 0u64;
                let mut next = Value::Null;
                for site in &self.sites {
                    self.ensure_init(site).await;
                    match self
                        .call_source(site, endpoint, body.clone(), budget, cancel)
                        .await
                    {
                        Ok(data) => {
                            let converted = convert::prefix_page_items(&data, site)
                                .and_then(|prefixed| py::page(&prefixed));
                            match converted {
                                Ok(page) => {
                                    if let Some(list) =
                                        page.get("items").and_then(Value::as_array)
                                    {
                                        items.extend(list.iter().cloned());
                                    }
                                    total += page.get("total").and_then(Value::as_u64).unwrap_or(0);
                                    if next.is_null() && !page.get("next").unwrap_or(&Value::Null).is_null() {
                                        next = page["next"].clone();
                                    }
                                }
                                Err(e) => {
                                    if first_err.is_none() {
                                        first_err = Some(e);
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            if first_err.is_none() {
                                first_err = Some(e);
                            }
                        }
                    }
                }
                if items.is_empty() {
                    if let Some(e) = first_err {
                        return Err(e);
                    }
                }
                Ok(json!({"items": items, "next": next, "total": total}))
            }
            _ => Err(PluginError::new("UNSUPPORTED", "猫源不支持该方法")),
        }
    }
}
