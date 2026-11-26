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
use futures_util::{stream, StreamExt};
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

    /// 测试用构造：直接给源服务地址与站点列表，不走边车加载。
    #[cfg(test)]
    pub fn for_test(plugin_id: String, base_url: String, sites: Vec<CatSite>) -> Result<Self> {
        let client = Arc::new(CatClient::new("http://127.0.0.1:1".into())?);
        let http = wreq::Client::builder()
            .emulation(wreq_util::Profile::Chrome149)
            .no_proxy()
            .build()
            .map_err(|_| PluginError::internal("创建猫源 HTTP 客户端失败"))?;
        Ok(Self {
            plugin_id,
            client: Arc::new(Mutex::new(client)),
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            sites,
            methods: convert::hpp_methods(),
            media: MediaCapabilities::default(),
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

    /// 供 manager 登记回环取流的豁免地址：源 HTTP 服务的 baseUrl。
    pub(crate) fn base_url(&self) -> &str {
        &self.base_url
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
            convert::CatCall::Shallow => {
                // 站点列表是加载时就已知的，不调源服务。
                let nodes: Vec<Value> = self
                    .sites
                    .iter()
                    .map(|s| json!({"id": s.key, "label": s.name, "children": []}))
                    .collect();
                Ok(Value::Array(nodes))
            }
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

    /// 并发扇出取数：对每个任务（站点 + 各自的 body）各调一次源服务
    ///（先 `ensure_init` 再 `call_source`），最多 6 路并行；结果按任务
    /// 顺序返回。单个站点失败只记在结果里，不中断其他站点。
    /// 之前是逐站串行：37 个站点、每站最长 5 秒 init + 15 秒调用，
    /// 一个慢站就能把整条调用拖到几十秒、前端骨架屏一直挂着。
    async fn fanout_fetch(
        &self,
        endpoint: &str,
        jobs: Vec<(CatSite, Value)>,
        budget: Duration,
        cancel: &CancellationToken,
    ) -> Vec<(CatSite, Result<Value>)> {
        let mut out: Vec<(usize, CatSite, Result<Value>)> = stream::iter(
            jobs.into_iter().enumerate().map(|(idx, (site, body))| {
                let worker = self.clone();
                let cancel = cancel.clone();
                async move {
                    worker.ensure_init(&site).await;
                    let r = worker
                        .call_source(&site, endpoint, body, budget, &cancel)
                        .await;
                    (idx, site, r)
                }
            }),
        )
        .buffer_unordered(6)
        .collect()
        .await;
        out.sort_by_key(|(idx, _, _)| *idx);
        out.into_iter().map(|(_, site, r)| (site, r)).collect()
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
                let jobs = self
                    .sites
                    .iter()
                    .map(|s| (s.clone(), body.clone()))
                    .collect();
                let mut classes = vec![];
                for (site, result) in self.fanout_fetch(endpoint, jobs, budget, cancel).await {
                    match result {
                        Ok(data) => {
                            if let Some(arr) = data.get("class").and_then(Value::as_array) {
                                for c in arr {
                                    let mut c = c.clone();
                                    if let Some(obj) = c.as_object_mut() {
                                        obj.insert("_cat_site".into(), json!(site.key));
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
                let nested = convert::nest_categories(&merged, &self.sites)?;
                if let Some(arr) = nested.as_array() {
                    if arr.len() == 1 {
                        // 只有一个站有分类：直接平铺该站的分类，不多一次点选。
                        return Ok(arr[0]["children"].clone());
                    }
                }
                Ok(nested)
            }
            "home.getSections" | "feed.list" => {
                let layout = if method == "home.getSections" {
                    "grid"
                } else {
                    "row"
                };
                let jobs = self
                    .sites
                    .iter()
                    .map(|s| (s.clone(), body.clone()))
                    .collect();
                let mut homes = self.fanout_fetch(endpoint, jobs, budget, cancel).await;
                // 回退：/home 只给分类不给列表的站点（设计 §5 的 /home
                // 形状本来就没有 list），取首个分类的 /category 列表当
                // 推荐内容；与 Python 蜘蛛 home.getSections 无
                // homeVideoContent 时退回 homeContent 的做法同款（协议
                // 06 §19.1）。有 list 的站点不走回退，避免两份推荐打架。
                let mut fallback_jobs: Vec<(CatSite, Value)> = vec![];
                let mut fallback_title: HashMap<String, String> = HashMap::new();
                for (site, result) in homes.iter() {
                    let Ok(data) = result else { continue };
                    let list_empty = data.get("list").is_none()
                        || data
                            .get("list")
                            .and_then(Value::as_array)
                            .is_some_and(Vec::is_empty);
                    if !list_empty {
                        continue;
                    }
                    let Some(class) = data
                        .get("class")
                        .and_then(Value::as_array)
                        .and_then(|a| a.first())
                    else {
                        continue;
                    };
                    let Some(tid) = class.get("type_id").and_then(Value::as_str) else {
                        continue;
                    };
                    let name = class
                        .get("type_name")
                        .and_then(Value::as_str)
                        .unwrap_or(tid)
                        .to_string();
                    let encoded = convert::encode_id(&site.key, tid);
                    let Some(convert::CatCall::Single { body, .. }) =
                        convert::dispatch("catalog.list", &json!({"categoryId": encoded}), &self.sites)
                    else {
                        continue;
                    };
                    fallback_jobs.push((site.clone(), body));
                    fallback_title.insert(site.key.clone(), name);
                }
                let mut used_fallback: HashSet<String> = HashSet::new();
                if !fallback_jobs.is_empty() {
                    let results = self
                        .fanout_fetch("/category", fallback_jobs, budget, cancel)
                        .await;
                    let by_key: HashMap<String, Result<Value>> = results
                        .into_iter()
                        .map(|(s, r)| (s.key, r))
                        .collect();
                    for (site, result) in homes.iter_mut() {
                        let Ok(data) = result.as_mut() else { continue };
                        let Some(cat) = by_key.get(&site.key) else { continue };
                        match cat {
                            Ok(cat) => {
                                if let Some(list) = cat
                                    .get("list")
                                    .and_then(Value::as_array)
                                    .filter(|a| !a.is_empty())
                                {
                                    if let Some(obj) = data.as_object_mut() {
                                        obj.insert("list".into(), json!(list));
                                    }
                                    used_fallback.insert(site.key.clone());
                                }
                            }
                            Err(e) => {
                                if first_err.is_none() {
                                    first_err = Some(e.clone());
                                }
                            }
                        }
                    }
                }
                let mut sections = vec![];
                let mut total = 0u64;
                for (site, result) in homes {
                    match result {
                        Ok(data) => {
                            // 无 list 时为空分区（设计 §6.1）；条目 id 也要
                            // 加站点前缀，否则多站点时详情解不出站点。
                            let mut data = data;
                            if data.get("list").is_none() {
                                if let Some(obj) = data.as_object_mut() {
                                    obj.insert("list".into(), json!([]));
                                }
                            }
                            let prefixed = match convert::prefix_page_items(&data, &site) {
                                Ok(v) => v,
                                Err(e) => {
                                    if first_err.is_none() {
                                        first_err = Some(e);
                                    }
                                    continue;
                                }
                            };
                            match py::sections(&prefixed, &site.key, layout) {
                                Ok(mut page) => {
                                    // 回退分区标上分类名（例：热门电影），
                                    // 不然全是“推荐”，分不出是哪个分类。
                                    if used_fallback.contains(&site.key) {
                                        if let Some(title) = fallback_title.get(&site.key) {
                                            if let Some(items) = page
                                                .get_mut("items")
                                                .and_then(Value::as_array_mut)
                                            {
                                                if let Some(sec) = items
                                                    .first_mut()
                                                    .and_then(Value::as_object_mut)
                                                {
                                                    sec.insert("title".into(), json!(title));
                                                }
                                            }
                                        }
                                    }
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
                let jobs = self
                    .sites
                    .iter()
                    .map(|s| (s.clone(), body.clone()))
                    .collect();
                let mut items = vec![];
                let mut total = 0u64;
                let mut next = Value::Null;
                for (site, result) in self.fanout_fetch(endpoint, jobs, budget, cancel).await {
                    match result {
                        Ok(data) => {
                            let converted = convert::prefix_page_items(&data, &site)
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        extract::{Json, Path, State},
        routing::post,
        Router,
    };
    use std::time::Instant;

    /// mock 猫源服务：记每次调用的起止时间，供并发断言用。
    struct MockState {
        calls: Mutex<Vec<(String, String, Instant, Instant)>>,
    }

    fn vod(id: &str, name: &str) -> Value {
        json!({"vod_id": id, "vod_name": name, "vod_pic": "http://x/1.jpg", "vod_remarks": "8.8"})
    }

    async fn mock_handler(
        State(state): State<Arc<MockState>>,
        Path((key, endpoint)): Path<(String, String)>,
        Json(_body): Json<Value>,
    ) -> Json<Value> {
        let start = Instant::now();
        // c/d 两站的 /home 各睡 2 秒：串行要 4 秒以上，并行约 2 秒。
        if endpoint == "home" && (key == "c" || key == "d") {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        let data = match (key.as_str(), endpoint.as_str()) {
            (_, "init") => return Json(json!({})),
            // a：/home 只有分类没有 list（豆瓣的形状），/category 给 2 条。
            ("a", "home") => json!({"class": [{"type_id": "hot_gaia", "type_name": "热门电影"}], "filters": {}}),
            ("a", "category") => {
                json!({"page": 1, "pagecount": 1, "total": 2, "list": [vod("a1", "影片一"), vod("a2", "影片二")]})
            }
            // b：/home 自带 list，不应再调 /category。
            ("b", "home") => json!({"class": [{"type_id": "bhot", "type_name": "B 热门"}], "list": [vod("b1", "B 影片")]}),
            ("b", "category") => {
                json!({"page": 1, "pagecount": 1, "total": 1, "list": [vod("bX", "不应出现")]})
            }
            ("c", "home") => json!({"class": [{"type_id": "chot", "type_name": "C 热门"}], "filters": {}}),
            ("c", "category") => {
                json!({"page": 1, "pagecount": 1, "total": 1, "list": [vod("c1", "C 影片")]})
            }
            ("d", "home") => json!({"class": [{"type_id": "dhot", "type_name": "D 热门"}], "filters": {}}),
            ("d", "category") => {
                json!({"page": 1, "pagecount": 1, "total": 1, "list": [vod("d1", "D 影片")]})
            }
            _ => json!({}),
        };
        let end = Instant::now();
        state
            .calls
            .lock()
            .unwrap()
            .push((key, endpoint, start, end));
        Json(json!({"code": 0, "data": data}))
    }

    fn site(key: &str, name: &str) -> CatSite {
        CatSite {
            key: key.to_string(),
            name: name.to_string(),
            site_type: 3,
            api: format!("/spider/{key}/3"),
        }
    }

    #[tokio::test]
    async fn home_sections_falls_back_to_first_category_and_fans_out_concurrently() {
        let state = Arc::new(MockState { calls: Mutex::new(vec![]) });
        let router = Router::new()
            .route("/spider/:key/3/:endpoint", post(mock_handler))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let worker = CatWorker::for_test(
            "cat.test".to_string(),
            base,
            vec![site("a", "A 站"), site("b", "B 站"), site("c", "C 站"), site("d", "D 站")],
        )
        .unwrap();
        let result = worker
            .call_scoped(
                "test-home-1".to_string(),
                "home.getSections".to_string(),
                Transfer::from_json(json!({})),
                Duration::from_secs(30),
                false,
            )
            .await
            .unwrap();
        server.abort();
        let page = result.json();
        let sections = page.get("items").and_then(Value::as_array).unwrap();
        assert_eq!(sections.len(), 4, "{page}");
        // a：回退首分类，2 条，标题是分类名。
        assert_eq!(sections[0].get("id").and_then(Value::as_str), Some("a"));
        assert_eq!(sections[0].get("title").and_then(Value::as_str), Some("热门电影"));
        assert_eq!(sections[0].get("items").and_then(Value::as_array).map(Vec::len), Some(2));
        // b：/home 自带 list，直接用，不调 /category。
        assert_eq!(sections[1].get("id").and_then(Value::as_str), Some("b"));
        assert!(sections[1].get("title").is_none());
        assert_eq!(sections[1].get("items").and_then(Value::as_array).map(Vec::len), Some(1));
        assert_eq!(
            sections[1]["items"][0].get("id").and_then(Value::as_str),
            Some("b::b1")
        );
        // c/d：回退，各 1 条。
        assert_eq!(sections[2].get("items").and_then(Value::as_array).map(Vec::len), Some(1));
        assert_eq!(sections[3].get("items").and_then(Value::as_array).map(Vec::len), Some(1));
        // b 的 /category 一次也没调。
        let calls = state.calls.lock().unwrap();
        assert!(
            !calls.iter().any(|(k, e, _, _)| k == "b" && e == "category"),
            "{calls:?}"
        );
        // c/d 的 /home 在时间上重叠：并行扇出，否则一个 2 秒睡完另一个才开始。
        let home_c = calls.iter().find(|(k, e, _, _)| k == "c" && e == "home").unwrap();
        let home_d = calls.iter().find(|(k, e, _, _)| k == "d" && e == "home").unwrap();
        assert!(
            home_c.2 < home_d.3 && home_d.2 < home_c.3,
            "c/d 的 /home 没有重叠：{calls:?}"
        );
    }
}
