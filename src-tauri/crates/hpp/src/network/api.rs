use super::*;
use crate::node::{DiagnoseDomainSummary, DiagnoseSummary, NodeRequest, NodeResponse};
use futures_util::StreamExt;
use tokio::sync::{watch, OwnedSemaphorePermit, Semaphore};
use wreq::StatusCode;

#[derive(Clone, Copy)]
pub enum Queue {
    Api,
    Media,
}

struct Permits {
    _plugin: OwnedSemaphorePermit,
    _host: OwnedSemaphorePermit,
}

pub(super) struct Outgoing {
    target: Url,
    method: Method,
    /// `pub(super)`：测试要读组装好的 Cookie 头（加宽桥的并入结果），
    /// 字段本身只在 `network` 模块树内可见。
    pub(super) headers: HeaderMap,
    body: Option<Vec<u8>>,
    cookies: bool,
    generation: Option<String>,
}

pub(super) struct BufferedResponse {
    pub status: StatusCode,
    pub url: Url,
    pub headers: HeaderMap,
    pub bytes: Vec<u8>,
    pub redacted: bool,
}

pub(super) struct Flight {
    pub(super) result: watch::Sender<Option<Result<Arc<BufferedResponse>>>>,
}

impl Network {
    pub(super) fn outgoing(
        &self,
        target: Url,
        method: Method,
        mut headers: HeaderMap,
        body: Option<Vec<u8>>,
        cookies: bool,
    ) -> Result<Outgoing> {
        let (generation, cookie) =
            self.request_cookies(&target, cookies && !headers.contains_key("cookie"));
        if !cookie.is_empty() {
            headers.insert(
                "cookie",
                HeaderValue::from_str(&cookie).map_err(|_| PluginError::invalid("Cookie 无效"))?,
            );
        }
        // v0.0.77：WebView clearance 拿到的通行证（只在内存里），按目标
        // 主机匹配后并进 Cookie 头——已有同名 Cookie 的不重复带；合并
        // 失败（非法字符）时只放弃附加通行证，不让整个请求失败。
        if cookies {
            let existing = headers.get("cookie").and_then(|v| v.to_str().ok());
            let extra = self.clearance_cookies(&target, existing);
            if !extra.is_empty() {
                let merged = match existing {
                    Some(cur) if !cur.is_empty() => format!("{cur}; {extra}"),
                    _ => extra,
                };
                if let Ok(value) = HeaderValue::from_str(&merged) {
                    headers.insert("cookie", value);
                }
            }
        }
        // v0.0.87 加宽桥：首页落的会话 Cookie 按目标 origin 取出来并
        // 进当次请求的 Cookie 头——只附当次、不进罐子；已有同名 Cookie
        //（罐子或通行证的）的不重复带，目标域名的优先；合并失败（非法
        // 字符）时只放弃附加，不让整个请求失败。
        if cookies {
            let existing = headers.get("cookie").and_then(|v| v.to_str().ok());
            let present: Vec<&str> = existing
                .map(|v| {
                    v.split(';')
                        .filter_map(|p| p.trim().split('=').next())
                        .collect()
                })
                .unwrap_or_default();
            let extra: Vec<String> = self
                .home_cookies_for(&target)
                .into_iter()
                .filter(|(n, _)| !present.iter().any(|p| p.eq_ignore_ascii_case(n)))
                .map(|(n, v)| format!("{n}={v}"))
                .collect();
            if !extra.is_empty() {
                let extra = extra.join("; ");
                let merged = match existing {
                    Some(cur) if !cur.is_empty() => format!("{cur}; {extra}"),
                    _ => extra,
                };
                if let Ok(value) = HeaderValue::from_str(&merged) {
                    headers.insert("cookie", value);
                }
            }
        }
        // 页面给的 UA 原样发出（哪怕是裸 "Mozilla/5.0"）：插件 net 请求
        // 走 Node 式极简画像（HTTP/1.1、无浏览器头），UA 与画像必须
        // 自洽，不能一边发裸 UA 一边补 Chrome 指纹。
        headers
            .entry("user-agent")
            .or_insert(HeaderValue::from_static("HGhub/HPP-dev"));
        Ok(Outgoing {
            target,
            method,
            headers,
            body,
            cookies,
            generation,
        })
    }

    fn check_cooldown(&self, host: &str) -> Result<()> {
        if let Some(until) = self.cooldown.lock().unwrap().get(host).copied() {
            let remaining = until.saturating_duration_since(Instant::now());
            if !remaining.is_zero() {
                let mut e = PluginError::new("RATE_LIMITED", "该主机正在冷却");
                e.retry_after_sec = Some(remaining.as_secs().max(1));
                return Err(e);
            }
        }
        Ok(())
    }

    async fn permits(&self, host: &str, cancel: &CancellationToken) -> Result<Permits> {
        let gate = {
            let mut hosts = self.hosts.lock().unwrap();
            hosts.retain(|_, gate| gate.strong_count() > 0);
            if let Some(gate) = hosts.get(host).and_then(Weak::upgrade) {
                gate
            } else {
                let gate = Arc::new(Semaphore::new(4));
                hosts.insert(host.to_string(), Arc::downgrade(&gate));
                gate
            }
        };
        // A saturated host must not occupy all plugin slots while waiting.
        let host = tokio::select! {
            _ = cancel.cancelled() => return Err(PluginError::cancelled()),
            value = gate.acquire_owned() => value.map_err(|_| PluginError::internal("网络队列已关闭"))?,
        };
        let plugin = tokio::select! {
            _ = cancel.cancelled() => return Err(PluginError::cancelled()),
            value = self.gate.clone().acquire_owned() => value.map_err(|_| PluginError::internal("网络队列已关闭"))?,
        };
        Ok(Permits {
            _plugin: plugin,
            _host: host,
        })
    }

    pub(super) async fn send(
        &self,
        outgoing: Outgoing,
        queue: Queue,
        cancel: &CancellationToken,
    ) -> Result<Response> {
        if cancel.is_cancelled() {
            return Err(PluginError::cancelled());
        }
        let host = outgoing.target.host_str().unwrap();
        self.check_cooldown(host)?;
        let permits = if matches!(queue, Queue::Api) {
            let permits = self.permits(host, cancel).await?;
            self.rate_limit(cancel).await?;
            Some(Arc::new(permits))
        } else {
            None
        };
        self.check_cooldown(host)?;
        let client = self.client(&outgoing.target, cancel).await?;
        let mut request = client
            // wreq 的 IntoUri 不接受 url::Url（只接受 Uri/str/String），传 String。
            .request(outgoing.method, outgoing.target.to_string())
            .headers(outgoing.headers);
        if let Some(body) = outgoing.body {
            request = request.body(body);
        }
        let mut response = tokio::select! {
            _ = cancel.cancelled() => return Err(PluginError::cancelled()),
            value = request.send() => value.map_err(|e| PluginError::new("NETWORK", crate::proxy::connection_error(&e)))?,
        };
        if self.receive_cookies(
            &outgoing.target,
            response.headers_mut(),
            outgoing.generation.as_deref(),
            outgoing.cookies,
        )? {
            response.extensions_mut().insert(RedactedSetCookie);
        }
        if matches!(response.status().as_u16(), 429 | 503) {
            if let Some(sec) = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
            {
                self.cooldown.lock().unwrap().insert(
                    host.to_string(),
                    Instant::now() + Duration::from_secs(sec.min(600)),
                );
            }
        }
        // API callers retain Response through chunk reads. Media streams use their
        // own permit, retained by the loopback response body.
        if let Some(permits) = permits {
            response.extensions_mut().insert(permits);
        }
        Ok(response)
    }

    pub(super) fn redirect(
        &self,
        target: &Url,
        status: StatusCode,
        headers: &HeaderMap,
        follow: bool,
        hop: usize,
    ) -> Result<Option<Url>> {
        if !follow || !matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308) {
            return Ok(None);
        }
        if hop == 5 {
            return Err(PluginError::new("NETWORK", "重定向次数超过上限"));
        }
        let location = headers
            .get("location")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| PluginError::new("NETWORK", "重定向缺少 Location"))?;
        let next = target
            .join(location)
            .map_err(|_| PluginError::new("NETWORK", "重定向地址无效"))?;
        if target.scheme() == "https" && next.scheme() != "https" {
            return Err(PluginError::new("NETWORK", "禁止 HTTPS 降级"));
        }
        self.target(next.as_str()).map(Some)
    }

    pub(super) async fn buffered(
        self: &Arc<Self>,
        mut target: Url,
        method: Method,
        mut headers: HeaderMap,
        body: Option<Vec<u8>>,
        cookies: bool,
        follow: bool,
        max: usize,
        timeout: Duration,
        // v0.0.82：探针本次运行的代理决议（对话框当前选中）；None =
        // 用已保存的设置（所有真实请求）。只影响 Node Api 路径的
        // 代理选择，不碰全局设置；wreq 回落路径仍用已保存的设置。
        proxy_override: Option<(crate::proxy::Mode, Option<String>)>,
        cancel: &CancellationToken,
    ) -> Result<Arc<BufferedResponse>> {
        let initial = target.origin();
        for hop in 0..=5 {
            target.set_fragment(None);
            if target.origin() != initial {
                headers.remove("authorization");
                headers.remove("cookie");
            }
            let outgoing = self.outgoing(
                target.clone(),
                method.clone(),
                headers.clone(),
                body.clone(),
                cookies,
            )?;
            let response = self
                .shared_hop(outgoing, follow, max, timeout, proxy_override.clone(), cancel)
                .await?;
            if let Some(next) =
                self.redirect(&target, response.status, &response.headers, follow, hop)?
            {
                target = next;
            } else {
                return Ok(response);
            }
        }
        unreachable!("redirect rejects a sixth hop")
    }

    /// 传输分发（v0.0.70）：插件 net 请求（Queue::Api）在 iOS 上走
    /// Node 边车，其他情况走现有 wreq 路径。媒体取流不经过这里
    /// （`Network::request` 直接调 `send()`），`send()` 本身不动。
    async fn read_hop(
        &self,
        outgoing: Outgoing,
        follow: bool,
        max: usize,
        timeout: Duration,
        // v0.0.82：探针本次运行的代理决议（见 buffered）；只在 Node
        // Api 路径生效，wreq 回落路径仍用已保存的设置（见下）。
        proxy_override: Option<(crate::proxy::Mode, Option<String>)>,
        cancel: &CancellationToken,
    ) -> Result<Arc<BufferedResponse>> {
        if self.node_transport() {
            return self
                .send_api_hop(outgoing, follow, max, timeout, proxy_override, cancel)
                .await;
        }
        self.read_hop_wreq(outgoing, follow, max, cancel).await
    }

    /// Node 传输的一跳：调度（冷却/许可/限速）与 Cookie（接收、
    /// `429/503` 冷却）与 `send()` 一致，能复用的方法都复用；
    /// `429/503` 冷却写入是与 `send()` 对应的小段重复（读的是 Node
    /// 返回的头，不是 wreq 响应头）。
    ///
    /// iOS 上不静默回落 wreq：引擎起不来、边车连不上、/request 500
    /// 都直接报明错——v0.0.70 是假设检验，静默回落会让真机结果
    /// 无法判断到底是哪条路过的。非 iOS 本来就没有 Node（见 read_hop）。
    ///
    /// v0.0.78：窄回落与 5xx 诊断（见 `node_hop_with_fallback`）——
    /// 只在跟随系统模式；5xx 不再当正常数据返回（wreq 路径仍当数据）。
    ///
    /// v0.0.81：失效镜像重定向（见 `rewrite_dead_mirror`）——请求
    /// 目标 host 是已死的 .cf 镜像时，在函数入口就改写到活的 .xyz
    /// 镜像，再走冷却/许可/预检；解析与诊断后缀里出现的是改写后
    /// 的 host，SNI/Host 随 URL 一起变（Node 只从 URL 取 hostname）。
    async fn send_api_hop(
        &self,
        outgoing: Outgoing,
        follow: bool,
        max: usize,
        timeout: Duration,
        // v0.0.82：探针本次运行的代理决议（见 buffered）。
        proxy_override: Option<(crate::proxy::Mode, Option<String>)>,
        cancel: &CancellationToken,
    ) -> Result<Arc<BufferedResponse>> {
        if cancel.is_cancelled() {
            return Err(PluginError::cancelled());
        }
        let mut outgoing = outgoing;
        // v0.0.81：失效镜像重定向——在冷却/许可/预检之前改写（见
        // rewrite_dead_mirror 的证据注释）。请求头不动：outgoing()
        // 已按原 host 附好 Cookie/通行证，.cf 与 .xyz 是同一站，
        // 附过去无害；.xyz 不挑战，不需要通行证。
        if let Some(redirected) = rewrite_dead_mirror(&outgoing.target) {
            self.log_mirror_redirect(&outgoing.target, &redirected);
            outgoing.target = redirected;
        }
        let host = outgoing.target.host_str().unwrap();
        self.check_cooldown(host)?;
        // 许可保持到响应体读完或取消（协议 03 §3）：Node 一次返回
        // 完整 body，许可在函数返回时释放，与 wreq 分支“先取许可、
        // 读完再放”的效果一致。
        let _permits = self.permits(host, cancel).await?;
        self.rate_limit(cancel).await?;
        self.check_cooldown(host)?;
        let target = outgoing.target.clone();
        // v0.0.76：决定性实验——不再把解析结果钉给 Node，与 Arcane
        // 完全一致（Node 默认 lookup + Node 18 默认 autoSelectFamily）。
        // Rust 侧的解析只保留两件事：① 预检——DNS 失败/不合规地址仍
        // 在这里快速失败（协议 03 §2 的校验意图）；② 诊断——解析到的
        // 地址附在 Node 报错末尾（见 with_resolved_addrs），让真机测试
        // 在所有失败路径上都能看到实际解析到了什么。
        // 代价是 TOCTOU 缺口：Node 在连接时重新解析，校验与连接之间
        // DNS 答案再变就拦不住——Arcane 同样有这个缺口（它根本不校验）；
        // 若实验证明 pinning 不是原因、要恢复固定，再把列表传回去。
        // 查到的证据（见 node-engine.md §7）：tokio 的 lookup_host 走
        // std getaddrinfo（系统解析器），并没有绕过 Karing 自建 DNS；
        // 真机 pinned 列表曾是 24 个 IPv4（v0.0.72 的 [Object] 个数），
        // 不是单个 FakeIP——说明当时系统解析器回的就是真实边缘地址。
        let addrs = tokio::select! {
            _ = cancel.cancelled() => return Err(PluginError::cancelled()),
            value = network_policy::resolve_target_keep_order(&target, self.strict()) => value.map_err(|e| PluginError::new("NETWORK", e))?,
        };
        let started = Instant::now();
        let response = self
            .node_hop_with_fallback(&outgoing, timeout, &addrs, started, proxy_override, cancel)
            .await?;
        let mut headers = response.headers;
        let redacted = self.receive_cookies(
            &target,
            &mut headers,
            outgoing.generation.as_deref(),
            outgoing.cookies,
        )?;
        self.note_rate_limit(host, response.status, &headers);
        // 重定向跳不读体（与 wreq 分支一致）：body 已由 Node 取回，
        // 这里只跳过 maxBytes 校验，buffered() 按 Location 跟进。
        let is_redirect = follow && matches!(response.status, 301 | 302 | 303 | 307 | 308);
        if !is_redirect && response.body.len() > max {
            return Err(PluginError::new("UPSTREAM_ERROR", "响应体超过 maxBytes"));
        }
        Ok(Arc::new(BufferedResponse {
            status: StatusCode::from_u16(response.status)
                .map_err(|_| PluginError::new("UPSTREAM_ERROR", "上游返回无效状态码"))?,
            url: target,
            headers,
            bytes: response.body,
            redacted,
        }))
    }

    /// 边车连不上（TCP 没连上、请求没进 Node）时返回 true：回环上
    /// 只有“边车没在听”这一种传输失败；/request 500 等边车错误
    /// 走别的文案，不触发刷新。
    fn is_node_connect(e: &PluginError) -> bool {
        e.code == "INTERNAL" && e.message.contains("Node 边车连接失败")
    }

    /// 拿缓存的 Node 客户端；没有就重探引导（重读 info 文件 +
    /// `/health`，边车重绑换了端口时拿到新地址）。
    async fn node_client(&self, cancel: &CancellationToken) -> Result<Arc<NodeClient>> {
        if let Some(client) = self.node_client.lock().unwrap().clone() {
            return Ok(client);
        }
        let bootstrap = self.node_bootstrap.lock().unwrap().clone();
        let info = tokio::select! {
            _ = cancel.cancelled() => return Err(PluginError::cancelled()),
            info = bootstrap.ensure_started() => info?,
        };
        let client = Arc::new(NodeClient::new(info.api_base)?);
        *self.node_client.lock().unwrap() = Some(client.clone());
        Ok(client)
    }

    /// 清掉旧客户端，下次 `node_client()` 重探引导拿新地址。
    async fn refresh_node_client(&self, cancel: &CancellationToken) -> Result<()> {
        *self.node_client.lock().unwrap() = None;
        self.node_client(cancel).await?;
        Ok(())
    }

    /// 调一次 Node 边车；连不上时重新探测地址并重试一次。
    /// 请求从未到过边车（TCP 都没连上），重试不会重放已执行的请求。
    /// 超时不触发重探。
    async fn node_request_with_refresh(
        &self,
        request: NodeRequest,
        cancel: &CancellationToken,
    ) -> Result<NodeResponse> {
        let first = {
            let client = self.node_client(cancel).await?;
            tokio::select! {
                _ = cancel.cancelled() => return Err(PluginError::cancelled()),
                r = client.request(request.clone()) => r,
            }
        };
        // v0.0.80：边车 DoH 使用情况写进 ext.log（成功/失败各记一行）。
        self.log_doh_usage(&first);
        match first {
            Ok(v) => Ok(v),
            Err(e) if Self::is_node_connect(&e) => {
                if self.refresh_node_client(cancel).await.is_ok() {
                    let client = self.node_client(cancel).await?;
                    let second = tokio::select! {
                        _ = cancel.cancelled() => Err(PluginError::cancelled()),
                        r = client.request(request) => r,
                    };
                    self.log_doh_usage(&second);
                    second
                } else {
                    Err(e)
                }
            }
            Err(e) => Err(e),
        }
    }

    /// Node 一跳 + 窄回落与 5xx 诊断（v0.0.78）：只在跟随系统模式、
    /// 只在 Api 的 Node 路径（即本函数）。首跳经代理快速失败（< 5 秒）
    /// 且是上游 5xx 或代理握手失败时，同一 URL/头/方法直连重试一次；
    /// 成功正常返回；失败返回首跳原始错误（附两次尝试结果）。自定义
    /// 代理、直连模式、慢失败不回落——首跳永远先走代理（用户 2026-11-23
    /// 的规矩：不许默认直连；回落本身也是定性仪器——重试成功坐实代理
    /// 路径，重试也 502 则与代理无关）。
    ///
    /// 5xx 不再当正常数据返回：转 `UPSTREAM_ERROR`（原始状态 + 代理
    /// 描述 + 解析地址 + 两次结果 + Cookie 名字，`details.status`
    /// 保留供 `fetch()` 的重试判定）。wreq 路径（非 iOS）仍把 5xx 当
    /// 数据返回——契约只改 Node 路径（见 node-engine.md §7）。
    /// `429/503` 的 Retry-After 冷却只在返回错误时写：重试成功说明
    /// 直连能通，不应再冷却该主机。
    async fn node_hop_with_fallback(
        &self,
        outgoing: &Outgoing,
        timeout: Duration,
        addrs: &[std::net::SocketAddr],
        started: Instant,
        // v0.0.82：探针本次运行的代理决议（见 buffered）；Some 时
        // 本跳就用它，不读已保存的设置——探针用对话框当前选中的
        // 模式跑，但绝不改已保存的设置（真实请求不受影响）。
        proxy_override: Option<(crate::proxy::Mode, Option<String>)>,
        cancel: &CancellationToken,
    ) -> Result<NodeResponse> {
        let (mode, proxy_url) = proxy_override.unwrap_or_else(|| self.proxy_decision());
        let proxy_desc = describe_proxy(mode, &proxy_url);
        let cookie_note = cookie_note(&outgoing.headers);
        self.log_proxy_decision(&outgoing.target, &proxy_desc, &cookie_note);
        let host = outgoing.target.host_str().unwrap_or("");
        let request = NodeRequest {
            url: outgoing.target.to_string(),
            method: outgoing.method.to_string(),
            headers: outgoing.headers.clone(),
            body: outgoing.body.clone(),
            timeout,
            // 代理每跳重新解析（不缓存地址），设置改动立即生效。
            proxy_url: proxy_url.clone(),
            pinned_addrs: vec![],
        };
        let eligible = fallback_eligible(mode, &proxy_url);
        let first = self
            .node_request_with_refresh(request.clone(), cancel)
            .await;
        match first {
            Ok(response) if (500..600).contains(&response.status) => {
                // v0.0.87：5xx 也记响应行——状态与 Set-Cookie、体标记
                // 都在响应里，错误转走后就看不见了。
                self.log_response(&response, "");
                let outcome = if eligible && started.elapsed() < Duration::from_secs(5) {
                    match self.direct_retry(&request, started, cancel).await {
                        Ok(retry) if retry.status < 500 => {
                            self.log_response(&retry, "直连重试");
                            return Ok(retry);
                        }
                        Ok(retry) => {
                            self.log_response(&retry, "直连重试");
                            format!("同样 {}", retry.status)
                        }
                        Err(e) => format!("失败：{}", short_error(&e)),
                    }
                } else {
                    no_retry_reason(mode, &proxy_url, started.elapsed())
                };
                if response.status == 503 {
                    self.note_rate_limit(host, response.status, &response.headers);
                }
                Err(upstream_5xx_error(
                    response.status,
                    &proxy_desc,
                    addrs,
                    &outcome,
                    &cookie_note,
                ))
            }
            Ok(response) => {
                self.log_response(&response, "");
                Ok(response)
            }
            Err(e)
                if eligible
                    && started.elapsed() < Duration::from_secs(5)
                    && is_proxy_handshake_error(&e, &proxy_url) =>
            {
                match self.direct_retry(&request, started, cancel).await {
                    Ok(retry) if (500..600).contains(&retry.status) => {
                        // 直连重试也拿到 503 时，Retry-After 冷却仍要落
                        // （与 5xx 分支一致）。
                        self.log_response(&retry, "直连重试");
                        if retry.status == 503 {
                            self.note_rate_limit(host, retry.status, &retry.headers);
                        }
                        let mut first_err = e;
                        first_err.message = format!(
                            "{}（代理: {proxy_desc}；直连重试: 同样 {}；Cookie: {cookie_note}）",
                            first_err.message, retry.status
                        );
                        Err(with_resolved_addrs(first_err, addrs))
                    }
                    Ok(retry) => {
                        self.log_response(&retry, "直连重试");
                        Ok(retry)
                    }
                    Err(retry_err) => {
                        let mut first_err = e;
                        first_err.message = format!(
                            "{}（代理: {proxy_desc}；直连重试: 失败：{}；Cookie: {cookie_note}）",
                            first_err.message,
                            short_error(&retry_err),
                        );
                        Err(with_resolved_addrs(first_err, addrs))
                    }
                }
            }
            Err(e) => Err(with_resolved_addrs(e, addrs)),
        }
    }

    /// 直连重试一次：同一请求，`proxy_url` 置空；超时用首跳剩余预算
    /// （外层 `fetch_once` 的总超时仍兜底——重试不会把单次请求拖
    /// 超过用户设的超时）。
    async fn direct_retry(
        &self,
        request: &NodeRequest,
        started: Instant,
        cancel: &CancellationToken,
    ) -> Result<NodeResponse> {
        let mut retry = request.clone();
        retry.proxy_url = None;
        retry.timeout = request.timeout.saturating_sub(started.elapsed());
        self.node_request_with_refresh(retry, cancel).await
    }

    /// `429/503` 的 Retry-After 冷却（协议 03 §3）：响应与转错误时
    /// 都写——`send_api_hop` 的收尾与 5xx 错误路径共用。
    /// `pub(crate)`：冷却写入的保持测试在 node_tests.rs 里调它。
    pub(crate) fn note_rate_limit(&self, host: &str, status: u16, headers: &HeaderMap) {
        if !matches!(status, 429 | 503) {
            return;
        }
        if let Some(sec) = headers
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
        {
            self.cooldown.lock().unwrap().insert(
                host.to_string(),
                Instant::now() + Duration::from_secs(sec.min(600)),
            );
        }
    }

    /// 每次 net.request 的代理决议写进 ext.log（模式 + host:port +
    /// 认证有/无；不记密码、不记 Cookie 值）：下次真机测试不用猜
    /// 跟随系统到底解析出了什么代理。没绑日志面板时（测试用的
    /// Network）不记。
    fn log_proxy_decision(&self, target: &Url, proxy_desc: &str, cookie_note: &str) {
        let Some(logs) = self.logs.lock().unwrap().clone() else {
            return;
        };
        logs.lock().unwrap().push(
            "info",
            &format!(
                "net.request 代理：{proxy_desc}；目标：{}；Cookie: {cookie_note}",
                target.host_str().unwrap_or("")
            ),
            "net",
            None,
        );
    }

    /// Cookie 桥接写进 ext.log（v0.0.86）：每次 WebHome 引擎请求前
    /// 桥了几个 Cookie、名字是什么（只报名字，不报值）。0 个也记
    /// 一行——桥跑了、窗口里没有该 URL 的 Cookie，与桥没跑是两回
    /// 事，真机验收靠这行区分。没绑日志面板时（测试用的 Network）
    /// 不记。
    pub(crate) fn log_cookie_bridge(&self, names: &[String]) {
        let Some(logs) = self.logs.lock().unwrap().clone() else {
            return;
        };
        logs.lock().unwrap().push(
            "info",
            &format!(
                "net.request Cookie 桥接：{} 个（名字：{}）",
                names.len(),
                names.join(", ")
            ),
            "net",
            None,
        );
    }

    /// 加宽桥写进 ext.log（v0.0.87）：首页的 Cookie 桥了几个、名字
    /// 是什么、来源是哪个域（只报名字与域名，不报值）。0 个也记一
    /// 行——桥跑了、WebView 里首页没有 Cookie，与桥没跑是两回事。
    /// 没绑日志面板时（测试用的 Network）不记。
    pub(crate) fn log_home_cookie_bridge(&self, names: &[String], source_host: &str) {
        let Some(logs) = self.logs.lock().unwrap().clone() else {
            return;
        };
        logs.lock().unwrap().push(
            "info",
            &format!(
                "net.request Cookie 桥接（首页 {source_host}）：{} 个（名字：{}）",
                names.len(),
                names.join(", ")
            ),
            "net",
            None,
        );
    }

    /// 响应里 Set-Cookie 的名字（v0.0.87）：只报名字，不报值；没有时
    /// 为空，由调用方写“无”。
    fn set_cookie_names(headers: &HeaderMap) -> Vec<String> {
        headers
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .filter_map(|s| s.split(';').next())
            .filter_map(|pair| pair.split('=').next())
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .collect()
    }

    /// 每次 Node 引擎请求的响应写进 ext.log（v0.0.87）：HTTP 状态 +
    /// 收到的 Set-Cookie 名字（有就报名字、无就报“无”）+ 响应体标记
    ///（是否含 module-item、是否含验证页文本、是否含 just-a-moment）。
    /// 只记布尔标记，绝不记完整响应体；只报 Cookie 名字，不报值。
    /// `note` 为空是首跳、`"直连重试"` 是直连重试那跳。没绑日志面板
    /// 时（测试用的 Network）不记。
    pub(crate) fn log_response(&self, response: &NodeResponse, note: &str) {
        let Some(logs) = self.logs.lock().unwrap().clone() else {
            return;
        };
        let (has_list, has_verify, has_challenge) = super::response_markers(&response.body);
        let set_cookies = Self::set_cookie_names(&response.headers);
        let set_cookies = if set_cookies.is_empty() {
            "无".to_string()
        } else {
            set_cookies.join(", ")
        };
        let note = if note.is_empty() {
            String::new()
        } else {
            format!("（{note}）")
        };
        logs.lock().unwrap().push(
            "info",
            &format!(
                "net.request 响应{note}：HTTP {}；Set-Cookie：{set_cookies}；内容：module-item={}，验证页={}，just-a-moment={}",
                response.status,
                if has_list { "是" } else { "否" },
                if has_verify { "是" } else { "否" },
                if has_challenge { "是" } else { "否" },
            ),
            "net",
            None,
        );
    }

    /// 失效镜像重定向写进 ext.log（v0.0.81）：from → to；每次改写记
    /// 一行（requestId `"net"`）。没绑日志面板时（测试用的 Network）
    /// 不记。
    fn log_mirror_redirect(&self, from: &Url, to: &Url) {
        let Some(logs) = self.logs.lock().unwrap().clone() else {
            return;
        };
        logs.lock().unwrap().push(
            "info",
            &format!(
                "net.request 失效镜像重定向：{} → {}",
                from.host_str().unwrap_or(""),
                to.host_str().unwrap_or("")
            ),
            "net",
            None,
        );
    }

    /// 边车 DoH 使用情况写进 ext.log（v0.0.80）：主机名、补充几个
    /// 地址、成功/失败。`doh` 来自边车响应/错误的 doh 字段（见
    /// node/client.rs）；没查过 DoH 时不记。没绑日志面板时不记。
    fn log_doh_usage(&self, result: &Result<NodeResponse>) {
        let doh = match result {
            Ok(v) => v.doh.as_deref().map(|d| (d, true)),
            Err(e) => e
                .details
                .as_ref()
                .and_then(|d| d.get("doh"))
                .and_then(Value::as_str)
                .map(|d| (d, false)),
        };
        let Some((doh, ok)) = doh else {
            return;
        };
        let Some(logs) = self.logs.lock().unwrap().clone() else {
            return;
        };
        logs.lock().unwrap().push(
            "info",
            &format!(
                "net.request DoH：{doh}；结果：{}",
                if ok { "成功" } else { "失败" }
            ),
            "net",
            None,
        );
    }

    /// 失效镜像重定向（v0.0.81）：玩偶站的两个 .cf 镜像已死——探针
    /// （2026-11-24 09:28，直连）证明 `woggpan.xxooo.cf` /
    /// `wogg.xxooo.cf` 的唯一系统解析地址 TLS 全被断（198.20.0.94 /
    /// 198.20.0.21，141/147ms），公开 DNS 对它俩查无记录（DoH 0 个）；
    /// 而同站的 .xyz / .net 镜像 7/4 条边缘全通、HTTP 200、非挑战页。
    /// 09:30 分类仍失败在 198.20.0.19（.cf 段的地址，.xyz/.net 从不
    /// 解析到 198.20.0.x）——分类请求打到了死镜像，DoH 对 .cf 无能
    /// 为力。4 个域名是同一 MacCMS 站点的镜像，内容相同。Node Api
    /// 请求路径上把死镜像 upfront 改写到活的 .xyz 镜像，不先试原域
    /// 名（.cf 之死是确定性的，没必要每次浪费一次失败请求；若 .cf
    /// 日后复活，重定向也无害——.xyz 同样能出内容）。探针不重写
    /// （探针就是要测原始域名，它不走 `send_api_hop`）。
    const DEAD_MIRROR_REDIRECTS: [(&'static str, &'static str); 2] = [
        ("woggpan.xxooo.cf", "woggpan.888484.xyz"),
        ("wogg.xxooo.cf", "woggpan.888484.xyz"),
    ];

    /// 网络诊断探针的目标域名（v0.0.80）：当前 WebHome 失败的玩偶站
    /// 4 个域名。只用于诊断（net.diagnose），不进任何请求逻辑——
    /// 探针要对照的正是这 4 个域名的系统解析与 DoH 答案。
    const DIAGNOSE_DOMAINS: [&'static str; 4] = [
        "woggpan.888484.xyz",
        "www.wogg.net",
        "woggpan.xxooo.cf",
        "wogg.xxooo.cf",
    ];

    /// 出口 IP 查询地址（v0.0.80）：cloudflare trace 优先（顺带记
    /// colo），不可用时边车换下一个。
    const DIAGNOSE_EGRESS_URLS: [&'static str; 2] = [
        "https://www.cloudflare.com/cdn-cgi/trace",
        "https://api.ipify.org",
    ];

    /// 真实分类请求检查的目标（v0.0.82）：① .cf 分类 URL——走正常
    /// net.request 路径，检验失效镜像改写是否生效；② .xyz 同款作
    /// 对照。用电影分类第 1 页（与页面 `_getInner` 竞速时打的 URL 同形）。
    const DIAGNOSE_CATEGORY_URLS: [&str; 2] = [
        "https://woggpan.xxooo.cf/vodshow/1--------1---.html",
        "https://woggpan.888484.xyz/vodshow/1--------1---.html",
    ];

    /// 真实分类请求的超时（v0.0.82）：页面分类请求默认 16 秒
    ///（`req()`/`_getInner` 的 timeoutSec 默认值）；Rust 侧
    /// `fetch_once` 默认 15 秒、上限 30 秒，16 秒落在常规预算内。
    const DIAGNOSE_CATEGORY_TIMEOUT: Duration = Duration::from_secs(16);

    /// 真实分类请求的最大响应体（v0.0.82）：与 `fetch_once` 默认的
    /// 2 MiB 一致；分类页远小于此。
    const DIAGNOSE_CATEGORY_MAX_BYTES: usize = 2 * 1024 * 1024;

    /// 分类页列表内容标记（v0.0.82）：与页面 `_getInner` 判定“列表页
    /// 无有效内容”用的同一组标记（MacCMS 列表页）；命中任一即有内容。
    const DIAGNOSE_CATEGORY_MARKERS: [&str; 6] = [
        "module-item",
        "module-poster-item",
        "stui-vodlist",
        "myui-vodlist",
        "list-item",
        "data-date",
    ];

    /// 网络诊断探针（v0.0.80；v0.0.82：用对话框当前选中的模式跑、
    /// 加真实分类请求检查、报告头带版本与改写状态、顶部加结论行）：
    /// 走 Node 边车，与真实请求同一条路、同一代理决议（本次运行
    /// 的模式）。只读：不改 net.request、回落、clearance、代理决议，
    /// 不碰 Cookie 罐，不存持久化结果。报告返回给设置页展示，同时
    /// 写进 ext.log（requestId "net"）。
    ///
    /// 真实分类请求检查：两条分类 URL 各发一次正常 net.request
    ///（`buffered()`，与插件 net.request 同一条路：改写、冷却、
    /// 许可、限速、Cookie、重定向跟随全在；只代理决议用本次运行
    /// 的模式）。Node 边车不可用时与真实请求一样回落到 wreq——
    /// 那时代理决议用已保存的设置（override 只进 Node 路径）。
    /// 最坏耗时是 Node 探针 60 秒 + 两次分类请求各 16 秒。
    pub async fn diagnose(
        self: &Arc<Self>,
        cancel: &CancellationToken,
        mode: &str,
        url: &str,
        version: &str,
    ) -> Result<String> {
        let (proxy_mode, proxy_url) = self.probe_proxy_decision(mode, url)?;
        let proxy_desc = describe_proxy(proxy_mode, &proxy_url);
        let client = self.node_client(cancel).await?;
        let outcome = client
            .diagnose(json!({
                "domains": Self::DIAGNOSE_DOMAINS,
                "proxyUrl": proxy_url,
                "proxyDesc": proxy_desc,
                "egressUrls": Self::DIAGNOSE_EGRESS_URLS,
            }))
            .await?;
        let version = version.trim();
        let version = if version.is_empty() { "未知" } else { version };
        let mut checks = Vec::with_capacity(Self::DIAGNOSE_CATEGORY_URLS.len());
        let mut check_lines: Vec<String> = Vec::new();
        for (i, url) in Self::DIAGNOSE_CATEGORY_URLS.iter().enumerate() {
            let (detail, result) = self
                .diagnose_category_check(url, (proxy_mode, proxy_url.clone()), cancel)
                .await;
            check_lines.push(format!(
                "{} {url}\n   {detail}",
                if i == 0 { "①" } else { "②" }
            ));
            checks.push(result);
        }
        let conclusion = Self::diagnose_conclusion(&outcome.summary, &checks);
        let rewrite_desc = if Self::DEAD_MIRROR_REDIRECTS.is_empty() {
            "无".to_string()
        } else {
            Self::DEAD_MIRROR_REDIRECTS
                .iter()
                .map(|(from, to)| format!("{from} → {to}"))
                .collect::<Vec<_>>()
                .join("；")
        };
        let report = format!(
            "结论：{conclusion}\n版本：{version}（失效镜像改写：{rewrite_desc}）\n{}\n真实分类请求（走正常 net.request 路径；超时 16 秒）：\n{}",
            outcome.report,
            check_lines.join("\n"),
        );
        if let Some(logs) = self.logs.lock().unwrap().clone() {
            logs.lock().unwrap().push("info", &report, "net", None);
        }
        Ok(report)
    }

    /// 探针本次运行的代理决议（v0.0.82）：对话框当前选中的模式，
    /// 不是已保存的设置。mode/url 都空 = 老调用（无参），沿用已保
    /// 存的设置；否则按传入模式解析（与 proxy.test 同款
    /// `Config::for_test`），自定义地址无效时直接报错——不静默换
    /// 成已保存的设置跑，静默换模式正是本版要修的误导。
    fn probe_proxy_decision(
        &self,
        mode: &str,
        url: &str,
    ) -> Result<(crate::proxy::Mode, Option<String>)> {
        if mode.is_empty() && url.is_empty() {
            return Ok(self.proxy_decision());
        }
        let config =
            crate::proxy::Config::for_test(mode, url).map_err(PluginError::invalid)?;
        Ok((config.mode, crate::proxy::effective_proxy_url(&config)))
    }

    /// 一次真实分类请求检查（v0.0.82）：返回（详情行，结构化结果）。
    /// 详情行只含本次实测的事实；失败时错误消息原样写（Node 的
    /// 已试/DoH/解析后缀全在里面）。
    async fn diagnose_category_check(
        self: &Arc<Self>,
        url: &str,
        proxy: (crate::proxy::Mode, Option<String>),
        cancel: &CancellationToken,
    ) -> (String, CategoryCheckResult) {
        let target = match self.target(url) {
            Ok(u) => u,
            Err(e) => {
                return (
                    format!("改写：无；解析：—；失败：地址无效（{e}）"),
                    CategoryCheckResult {
                        rewritten_to: None,
                        status: None,
                        has_content: false,
                        challenge: false,
                        error: Some(format!("地址无效（{e}）")),
                    },
                );
            }
        };
        let rewritten = rewrite_dead_mirror(&target);
        let rewritten_to = rewritten
            .as_ref()
            .and_then(|u| u.host_str())
            .map(str::to_string);
        let rewrite_text = match &rewritten_to {
            Some(to) => format!("改写：{} → {to}", target.host_str().unwrap_or("")),
            None => "改写：无".to_string(),
        };
        // 解析地址（展示用；失败时以错误后缀里那组为准——同一解析器、
        // 先后脚查的两组，正常情况下应一致，不一致本身也是诊断信息）。
        let effective = rewritten.unwrap_or_else(|| target.clone());
        let addrs = match network_policy::resolve_target_keep_order(&effective, self.strict())
            .await
        {
            Ok(list) => {
                let mut ips: Vec<String> =
                    list.iter().map(|a| a.ip().to_string()).collect();
                ips.sort();
                ips.dedup();
                if ips.is_empty() {
                    "无记录".to_string()
                } else {
                    ips.join(", ")
                }
            }
            Err(e) => format!("解析失败（{e}）"),
        };
        // 与页面 req() 同款头：裸 UA + Referer 就是 URL 本身。
        let mut headers = HeaderMap::new();
        headers.insert("user-agent", HeaderValue::from_static("Mozilla/5.0"));
        if let Ok(v) = HeaderValue::from_str(url) {
            headers.insert("referer", v);
        }
        let result = self
            .buffered(
                target,
                Method::GET,
                headers,
                None,
                true,
                true,
                Self::DIAGNOSE_CATEGORY_MAX_BYTES,
                Self::DIAGNOSE_CATEGORY_TIMEOUT,
                Some(proxy),
                cancel,
            )
            .await;
        match result {
            Ok(resp) => {
                let body = String::from_utf8_lossy(&resp.bytes);
                let lower = body.to_ascii_lowercase();
                let challenge = ["just a moment", "cf-browser", "access denied"]
                    .iter()
                    .any(|m| lower.contains(m));
                let marker = Self::DIAGNOSE_CATEGORY_MARKERS
                    .iter()
                    .find(|m| body.contains(**m))
                    .map(|s| s.to_string());
                let status = resp.status.as_u16();
                let mut detail = format!(
                    "{rewrite_text}；解析：{addrs}；HTTP {status}；{}",
                    if challenge {
                        "挑战页".to_string()
                    } else if let Some(m) = &marker {
                        format!("含列表内容（{m}）")
                    } else {
                        "无列表内容标记".to_string()
                    }
                );
                if resp.url.as_str() != url {
                    detail.push_str(&format!("；最终地址：{}", resp.url));
                }
                (
                    detail,
                    CategoryCheckResult {
                        rewritten_to,
                        status: Some(status),
                        has_content: marker.is_some(),
                        challenge,
                        error: None,
                    },
                )
            }
            Err(e) => (
                format!("{rewrite_text}；解析：{addrs}；失败：{}", e.message),
                CategoryCheckResult {
                    rewritten_to,
                    status: None,
                    has_content: false,
                    challenge: false,
                    error: Some(e.message.clone()),
                },
            ),
        }
    }

    /// 结论行（v0.0.82）：纯函数，只陈述本次测到的事实。域名小结
    /// 来自 Node /diagnose，真实分类结果来自 `diagnose_category_check`。
    pub(super) fn diagnose_conclusion(
        summary: &DiagnoseSummary,
        checks: &[CategoryCheckResult],
    ) -> String {
        let mut clauses: Vec<String> = summary
            .domains
            .iter()
            .map(Self::diagnose_domain_verdict)
            .collect();
        if summary.domains.is_empty() {
            clauses.push("无域名检查结果".to_string());
        }
        let mut real = String::new();
        if let Some(first) = checks.first() {
            let via = match &first.rewritten_to {
                Some(to) => format!("经改写（→{to}）后"),
                None => "（无改写）".to_string(),
            };
            real.push_str(&format!(
                "真实分类请求：.cf {via}{}",
                first.outcome_text()
            ));
        }
        if let Some(second) = checks.get(1) {
            real.push_str(&format!("；对照 .xyz {}", second.outcome_text()));
        }
        if real.is_empty() {
            real.push_str("真实分类请求：未执行");
        }
        clauses.push(real);
        clauses.join("；")
    }

    /// 单个域名的结论分句（v0.0.82）：只用小结里的实测计数。
    pub(super) fn diagnose_domain_verdict(d: &DiagnoseDomainSummary) -> String {
        if d.edges == 0 {
            return format!("{} 无可用地址", d.host);
        }
        if d.http_ok == d.edges {
            return format!("{} 全通（{}/{} 边缘）", d.host, d.http_ok, d.edges);
        }
        if d.http_ok > 0 {
            return format!("{} 部分通（{}/{} 边缘）", d.host, d.http_ok, d.edges);
        }
        if d.tls_ok > 0 {
            return format!("{} TLS 通但 HTTP 未成功", d.host);
        }
        if d.tls_fail == d.edges {
            let mut s = format!("{} TLS 全被断", d.host);
            if d.doh_state == "empty" {
                s.push_str("（DoH 无记录）");
            } else if d.doh_state == "failed" {
                s.push_str("（DoH 查询失败）");
            }
            return s;
        }
        format!("{} 无一边缘成功", d.host)
    }

    async fn read_hop_wreq(
        &self,
        outgoing: Outgoing,
        follow: bool,
        max: usize,
        cancel: &CancellationToken,
    ) -> Result<Arc<BufferedResponse>> {
        // wreq 没有 Response::chunk()，也拿不到 response.url()：先把本跳
        // 的目标地址记下来，再把响应体转成流逐块读（bytes_stream 会消费
        // response，所以状态/头/标记要在转流之前快照）。重定向由调用方
        // buffered() 按 target 逐跳跟进，这里存的 url 就是本跳的目标地址。
        let target = outgoing.target.clone();
        let mut response = self.send(outgoing, Queue::Api, cancel).await?;
        // wreq 的 bytes_stream 会消费 response 并丢掉 extensions 里挂的
        // Api 并发许可；协议 03 §3 要求许可保持到响应体读完或取消，
        // 不能在收到响应头时提前释放——所以先把许可取出来，在本函数内
        // 保持到返回再释放（与旧 chunk() 读法保持 response 存活
        // 到读完的效果一致）。重定向分支不读体，许可随本函数返回释放，
        // 与旧行为一致。
        let _permits = response.extensions_mut().remove::<Arc<Permits>>();
        let mut bytes = Vec::new();
        if !follow || !matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
            let status = response.status();
            let headers = response.headers().clone();
            let redacted = response
                .extensions()
                .get::<RedactedSetCookie>()
                .is_some();
            let mut stream = Box::pin(response.bytes_stream());
            while let Some(chunk) = tokio::select! {
                _ = cancel.cancelled() => return Err(PluginError::cancelled()),
                value = stream.next() => value.transpose().map_err(|_| PluginError::new("NETWORK", "响应读取失败"))?,
            } {
                if bytes.len() + chunk.len() > max {
                    return Err(PluginError::new("UPSTREAM_ERROR", "响应体超过 maxBytes"));
                }
                bytes.extend_from_slice(&chunk);
            }
            return Ok(Arc::new(BufferedResponse {
                status,
                url: target,
                headers,
                bytes,
                redacted,
            }));
        }
        Ok(Arc::new(BufferedResponse {
            status: response.status(),
            url: target,
            headers: response.headers().clone(),
            bytes,
            redacted: response.extensions().get::<RedactedSetCookie>().is_some(),
        }))
    }

    async fn shared_hop(
        self: &Arc<Self>,
        outgoing: Outgoing,
        follow: bool,
        max: usize,
        timeout: Duration,
        // v0.0.82：探针本次运行的代理决议（见 buffered）；进合并键，
        // 不同模式的同 URL 请求不共用一条在途结果。
        proxy_override: Option<(crate::proxy::Mode, Option<String>)>,
        cancel: &CancellationToken,
    ) -> Result<Arc<BufferedResponse>> {
        self.check_cooldown(outgoing.target.host_str().unwrap())?;
        if outgoing.method != Method::GET
            || outgoing.headers.contains_key("authorization")
            || outgoing.headers.contains_key("cookie")
        {
            return self
                .read_hop(outgoing, follow, max, timeout, proxy_override, cancel)
                .await;
        }
        let key = json!([
            crate::proxy::generation(),
            self.strict(),
            outgoing.target.as_str(),
            outgoing
                .headers
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_bytes()))
                .collect::<std::collections::BTreeMap<_, _>>(),
            outgoing.cookies,
            outgoing.generation,
            follow,
            max,
            // 代理决议不同 = 走的路不同，不能合并（见上）。
            proxy_override
                .as_ref()
                .map(|(m, u)| format!("{}|{}", m.as_str(), u.as_deref().unwrap_or(""))),
        ])
        .to_string();
        let mut receiver = {
            let mut pending = self.inflight.lock().unwrap();
            if let Some(flight) = pending.get(&key).filter(|f| !f.result.is_closed()) {
                flight.result.subscribe()
            } else {
                let (result, receiver) = watch::channel(None);
                let flight = Arc::new(Flight { result });
                pending.insert(key.clone(), flight.clone());
                let network = self.clone();
                tokio::spawn(async move {
                    let cancel = CancellationToken::new();
                    let work =
                        network.read_hop(outgoing, follow, max, timeout, proxy_override, &cancel);
                    tokio::pin!(work);
                    let result = loop {
                        tokio::select! {
                            biased;
                            _ = flight.result.closed() => {
                                let mut pending = network.inflight.lock().unwrap();
                                if flight.result.is_closed() {
                                    if pending.get(&key).is_some_and(|current| Arc::ptr_eq(current, &flight)) {
                                        pending.remove(&key);
                                    }
                                    break Err(PluginError::cancelled());
                                }
                            },
                            value = &mut work => break value,
                        }
                    };
                    cancel.cancel();
                    let mut pending = network.inflight.lock().unwrap();
                    if pending
                        .get(&key)
                        .is_some_and(|current| Arc::ptr_eq(current, &flight))
                    {
                        pending.remove(&key);
                    }
                    flight.result.send_replace(Some(result));
                });
                receiver
            }
        };
        loop {
            if cancel.is_cancelled() {
                return Err(PluginError::cancelled());
            }
            if let Some(result) = receiver.borrow().clone() {
                return result;
            }
            tokio::select! {
                _ = cancel.cancelled() => return Err(PluginError::cancelled()),
                changed = receiver.changed() => changed.map_err(|_| PluginError::internal("网络合并请求缺少结果"))?,
            }
        }
    }
}

/// 一次真实分类请求检查的结构化结果（v0.0.82）：结论行用；详情
/// 行由 `diagnose_category_check` 直接拼。
pub(super) struct CategoryCheckResult {
    /// 改写后的 host（None = 无改写）。
    pub(super) rewritten_to: Option<String>,
    /// 成功时 HTTP 状态。
    pub(super) status: Option<u16>,
    /// 成功且 body 含列表内容标记。
    pub(super) has_content: bool,
    /// 成功但 body 是挑战页。
    pub(super) challenge: bool,
    /// 失败时的完整错误消息（含已试/DoH/解析后缀）。
    pub(super) error: Option<String>,
}

impl CategoryCheckResult {
    /// 结论行里的结果分句（v0.0.82）：只陈述本次测到的事实。
    pub(super) fn outcome_text(&self) -> String {
        if let Some(e) = &self.error {
            return format!("失败（{}）", diagnose_short_reason(e));
        }
        let status = self.status.unwrap_or(0);
        if self.challenge {
            return format!("成功但返回挑战页（HTTP {status}）");
        }
        if self.has_content {
            return format!("成功（HTTP {status}，含列表内容）");
        }
        format!("成功但无列表内容（HTTP {status}）")
    }
}

/// 错误消息缩成结论行用的短原因（v0.0.82）：只按消息里的原话归类，
/// 完整消息在报告的检查行里原样写。
fn diagnose_short_reason(message: &str) -> String {
    if message.contains("before secure TLS") {
        "TLS 被断".to_string()
    } else if message.contains("超时")
        || message.contains("timed out")
        || message.contains("Timeout")
    {
        "超时".to_string()
    } else if message.contains("冷却") || message.contains("RATE_LIMITED") {
        "被限流/冷却".to_string()
    } else if message.contains("ENOTFOUND")
        || message.contains("getaddrinfo")
        || message.contains("解析失败")
        || message.contains("DNS")
    {
        "DNS 失败".to_string()
    } else if message.contains("502") || message.contains("503") || message.contains("504") {
        "上游错误".to_string()
    } else {
        "失败".to_string()
    }
}

/// 代理决议的对外描述（报错与日志共用）：模式 + `scheme://host[:port]`；
/// 认证只写有/无，绝不写值；解析失败时不回显原始地址。
fn describe_proxy(mode: crate::proxy::Mode, proxy_url: &Option<String>) -> String {
    use crate::proxy::Mode;
    match (mode, proxy_url) {
        (Mode::Off, _) => "直连".to_string(),
        (Mode::Custom, None) => "自定义（无代理）".to_string(),
        (Mode::System, None) => "跟随系统（无代理）".to_string(),
        (Mode::System, Some(url)) => match redact_proxy_url(url) {
            Some((addr, authed)) => {
                format!("跟随系统 {addr}{}", if authed { "（有认证）" } else { "" })
            }
            None => "跟随系统（代理地址无效）".to_string(),
        },
        (Mode::Custom, Some(url)) => match redact_proxy_url(url) {
            Some((addr, authed)) => {
                format!("自定义 {addr}{}", if authed { "（有认证）" } else { "" })
            }
            None => "自定义（代理地址无效）".to_string(),
        },
    }
}

/// 代理 URL → `scheme://host[:port]`（去 userinfo）+ 是否有认证。
/// IPv6 host 加方括号。
fn redact_proxy_url(url: &str) -> Option<(String, bool)> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    let port = parsed.port().map(|p| format!(":{p}")).unwrap_or_default();
    Some((
        format!("{}://{host}{port}", parsed.scheme()),
        !parsed.username().is_empty(),
    ))
}

/// 死镜像改写（v0.0.81，纯函数）：host 在 `DEAD_MIRROR_REDIRECTS`
/// 里时返回改写后的 URL——只换 host，scheme/端口/路径/查询/片段
/// 都不动；不在表里时回 `None`。`Url` 解析时已把 host 小写规范化，
/// 这里直接精确匹配。
fn rewrite_dead_mirror(target: &Url) -> Option<Url> {
    let host = target.host_str()?;
    let to = Network::DEAD_MIRROR_REDIRECTS
        .iter()
        .find(|(from, _)| *from == host)?
        .1;
    let mut rewritten = target.clone();
    rewritten.set_host(Some(to)).ok()?;
    Some(rewritten)
}

/// 窄回落只在跟随系统且实际解析出代理时才有意义：自定义是用户显式
/// 路由，不擅自绕过；直连/无代理时没有可回落的对象。
fn fallback_eligible(mode: crate::proxy::Mode, proxy_url: &Option<String>) -> bool {
    mode == crate::proxy::Mode::System && proxy_url.is_some()
}

/// 是不是代理层握手失败（host.js 的代理错误文案都含“代理”；SOCKS
/// 连不上代理时文案是裸 `connect ECONNREFUSED <代理 host>:<port>`，
/// 用代理 host 认）。隧道建好后的上游失败（TLS 被断、DNS 失败、
/// 上游 ECONNREFUSED）不含这两项，不触发回落。
fn is_proxy_handshake_error(e: &PluginError, proxy_url: &Option<String>) -> bool {
    if e.code != "NETWORK" {
        return false;
    }
    if e.message.contains("代理") {
        return true;
    }
    let Some(url) = proxy_url else {
        return false;
    };
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    e.message.contains(host)
}

/// Cookie 头里的名字（只报名字，不报值）：诊断用，排除罐子 Cookie
/// 嫌疑。名字按 `;` 分段、取每段第一个 `=` 之前。
fn cookie_names(headers: &HeaderMap) -> Option<Vec<String>> {
    let value = headers.get("cookie")?.to_str().ok()?;
    let names: Vec<String> = value
        .split(';')
        .filter_map(|part| {
            let name = part.trim().split('=').next().unwrap_or("").trim();
            (!name.is_empty()).then(|| name.to_string())
        })
        .collect();
    Some(names)
}

/// 附带的 Cookie 只报名字（值绝不记）：`已附带（名字：a, b）` /
/// `未附带`。
fn cookie_note(headers: &HeaderMap) -> String {
    match cookie_names(headers) {
        Some(names) if !names.is_empty() => {
            format!("已附带（名字：{}）", names.join(", "))
        }
        _ => "未附带".to_string(),
    }
}

/// 错误消息的简写（前 120 字）：重试失败的原因此处只做定性，不
/// 展开全文（消息里可能带长 URL）。
fn short_error(e: &PluginError) -> String {
    const LIMIT: usize = 120;
    let mut out: String = e.message.chars().take(LIMIT).collect();
    if e.message.chars().count() > LIMIT {
        out.push('…');
    }
    out
}

/// 没做直连重试的原因（结构性原因优先于耗时原因）。
fn no_retry_reason(
    mode: crate::proxy::Mode,
    proxy_url: &Option<String>,
    elapsed: Duration,
) -> String {
    use crate::proxy::Mode;
    match (mode, proxy_url) {
        (Mode::Custom, _) => "未尝试（自定义代理）".to_string(),
        (Mode::Off, _) => "未尝试（已直连）".to_string(),
        (_, None) => "未尝试（无代理）".to_string(),
        (_, Some(_)) => format!("未尝试（首跳耗时 {:.1} 秒）", elapsed.as_secs_f64()),
    }
}

/// 5xx 的诊断错误：原始状态 + 代理描述 + 解析地址 + 两次尝试结果 +
/// Cookie 名字（不报值）；`details.status` 保留，供 `fetch()` 的
/// 重试判定（`details.status >= 500` 才算失败，见 network.rs）。
fn upstream_5xx_error(
    status: u16,
    proxy_desc: &str,
    addrs: &[std::net::SocketAddr],
    retry_outcome: &str,
    cookie_note: &str,
) -> PluginError {
    let mut parts = vec![format!("代理: {proxy_desc}")];
    if !addrs.is_empty() {
        let list = addrs
            .iter()
            .map(|a| a.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        parts.push(format!("解析: {list}"));
    }
    parts.push(format!("直连重试: {retry_outcome}"));
    parts.push(format!("Cookie: {cookie_note}"));
    let mut e = PluginError::new(
        "UPSTREAM_ERROR",
        format!("上游返回 HTTP {status}（{}）", parts.join("；")),
    );
    e.details = Some(json!({"status": status}));
    e
}

/// Node 请求失败时，把 Rust 侧预检解析到的地址附在报错末尾
/// （`（解析: <ip1>, <ip2>；共 <n> 个地址）`），让真机测试在所有失败路径
/// （直连/代理、NETWORK/TIMEOUT/INTERNAL）上都能看到实际解析到了
/// 什么地址——v0.0.75 的 `pinned:` 只在直连 NETWORK 错误上出现，
/// 且拦截页是 HTTP 200、没有宿主错误，本来就带不上任何诊断。
/// 取消不附加（不是失败）。
/// v0.0.79：带上解析总数——直连与跟随系统两种模式都要能从这个诊断
/// 里直接读出结论（2026-11-24 06:25/06:33 两次真机报错都只有 1 个
/// 地址 198.20.0.19:443；后缀列的是全部解析地址，不只第一个——见
/// network_policy.rs resolve_target_ordered 全量 collect——所以 1
/// 个就是只回了 1 个，轮换无处可换；不写总数，用户数不出这个结论）。
fn with_resolved_addrs(mut e: PluginError, addrs: &[std::net::SocketAddr]) -> PluginError {
    if addrs.is_empty() || e.code == "CANCELLED" {
        return e;
    }
    let list = addrs
        .iter()
        .map(|a| a.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    e.message = format!("{}（解析: {list}；共 {} 个地址）", e.message, addrs.len());
    e
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dead_mirror_redirect_rewrites_cf_hosts_only() {
        // v0.0.81：两个 .cf 镜像改写到活的 .xyz 镜像，只换 host，
        // 路径/查询/端口都不动；非列表内 host 不动。
        assert_eq!(
            rewrite_dead_mirror(
                &Url::parse("https://woggpan.xxooo.cf/vodshow/1--------1---.html").unwrap()
            )
            .unwrap()
            .as_str(),
            "https://woggpan.888484.xyz/vodshow/1--------1---.html"
        );
        assert_eq!(
            rewrite_dead_mirror(
                &Url::parse("https://wogg.xxooo.cf:8443/a/b?x=1&y=2").unwrap()
            )
            .unwrap()
            .as_str(),
            "https://woggpan.888484.xyz:8443/a/b?x=1&y=2"
        );
        assert!(rewrite_dead_mirror(
            &Url::parse("https://www.wogg.net/vodshow/2--------1---.html").unwrap()
        )
        .is_none());
        assert!(rewrite_dead_mirror(
            &Url::parse("https://woggpan.888484.xyz/vodshow/1--------1---.html").unwrap()
        )
        .is_none());
    }

    #[test]
    fn resolved_addrs_suffix_goes_on_all_node_errors_except_cancelled() {
        // v0.0.76：Node 请求失败时报错末尾带上预检解析到的地址。
        let addrs: Vec<std::net::SocketAddr> = vec![
            "104.18.32.0:443".parse().unwrap(),
            "198.18.0.71:443".parse().unwrap(),
        ];
        let e = with_resolved_addrs(PluginError::new("NETWORK", "上游失败"), &addrs);
        assert_eq!(
            e.message,
            "上游失败（解析: 104.18.32.0:443, 198.18.0.71:443；共 2 个地址）"
        );
        assert_eq!(e.code, "NETWORK");
        // 空列表不附加；取消不附加。
        let e = with_resolved_addrs(PluginError::new("TIMEOUT", "超时"), &[]);
        assert_eq!(e.message, "超时");
        let e = with_resolved_addrs(PluginError::cancelled(), &addrs);
        assert_eq!(e.message, "调用已取消");
    }

    #[test]
    fn proxy_description_redacts_auth_and_covers_all_modes() {
        // v0.0.78：代理描述只含模式 + host:port，认证只写有/无。
        use crate::proxy::Mode;
        let sys = Some("http://127.0.0.1:7890".to_string());
        assert_eq!(
            describe_proxy(Mode::System, &sys),
            "跟随系统 http://127.0.0.1:7890"
        );
        assert_eq!(
            describe_proxy(
                Mode::System,
                &Some("http://user:secret@127.0.0.1:7890".to_string())
            ),
            "跟随系统 http://127.0.0.1:7890（有认证）"
        );
        assert_eq!(
            describe_proxy(Mode::Custom, &Some("socks5h://127.0.0.1:1080".to_string())),
            "自定义 socks5h://127.0.0.1:1080"
        );
        assert_eq!(
            describe_proxy(Mode::System, &Some("http://user:pw@[::1]:8080".to_string())),
            "跟随系统 http://[::1]:8080（有认证）"
        );
        assert_eq!(describe_proxy(Mode::System, &None), "跟随系统（无代理）");
        assert_eq!(describe_proxy(Mode::Custom, &None), "自定义（无代理）");
        assert_eq!(describe_proxy(Mode::Off, &sys), "直连");
        assert_eq!(
            describe_proxy(Mode::System, &Some("not a url".to_string())),
            "跟随系统（代理地址无效）"
        );
        // 密码绝不出现在描述里。
        assert!(!describe_proxy(
            Mode::System,
            &Some("http://user:secret@127.0.0.1:7890".to_string())
        )
        .contains("secret"));
    }

    #[test]
    fn fallback_only_for_system_with_a_proxy() {
        // v0.0.78：窄回落只在跟随系统且实际解析出代理时。
        use crate::proxy::Mode;
        let proxy = Some("http://127.0.0.1:7890".to_string());
        assert!(fallback_eligible(Mode::System, &proxy));
        assert!(!fallback_eligible(Mode::Custom, &proxy));
        assert!(!fallback_eligible(Mode::Off, &proxy));
        assert!(!fallback_eligible(Mode::System, &None));
        assert!(!fallback_eligible(Mode::Custom, &None));
    }

    #[test]
    fn proxy_handshake_detection_matches_host_js_messages() {
        // v0.0.78：host.js 的代理错误文案（见 node-sidecar/host.js）。
        let proxy = Some("http://127.0.0.1:7890".to_string());
        let socks = Some("socks5://127.0.0.1:1080".to_string());
        let cases = [
            ("代理拒绝 CONNECT（HTTP 502）", &proxy),
            (
                "无法连接代理目标服务器：connect ECONNREFUSED 1.2.3.4:443",
                &proxy,
            ),
            ("SOCKS5 代理拒绝连接（REP 0x05）", &socks),
            ("SOCKS5 代理认证失败", &socks),
            ("SOCKS5 代理返回无效版本", &socks),
            ("SOCKS4 代理拒绝连接（状态 0x5b）", &socks),
            // SOCKS 连不上代理：文案是裸 ECONNREFUSED + 代理地址。
            ("connect ECONNREFUSED 127.0.0.1:1080", &socks),
        ];
        for (msg, p) in cases {
            let e = PluginError::new("NETWORK", format!("Node 边车请求失败：{msg}"));
            assert!(is_proxy_handshake_error(&e, p), "应识别：{msg}");
        }
        // 隧道建好后的失败不触发回落。
        let upstream = [
            "read ECONNRESET",
            "connect ECONNREFUSED 93.184.216.34:443",
            "getaddrinfo ENOTFOUND example.com",
            "socket hang up",
        ];
        for msg in upstream {
            let e = PluginError::new("NETWORK", format!("Node 边车请求失败：{msg}"));
            assert!(!is_proxy_handshake_error(&e, &proxy), "不应识别：{msg}");
        }
        // 非 NETWORK 码不识别。
        let e = PluginError::new("TIMEOUT", "Node 边车请求失败：代理拒绝 CONNECT（HTTP 502）");
        assert!(!is_proxy_handshake_error(&e, &proxy));
    }

    #[test]
    fn cookie_note_reports_names_only() {
        // v0.0.78：Cookie 只报名字，值绝不出现。
        let mut headers = HeaderMap::new();
        headers.insert(
            "cookie",
            HeaderValue::from_static("cf_clearance=abc123; session=xyz; theme=dark"),
        );
        assert_eq!(
            cookie_note(&headers),
            "已附带（名字：cf_clearance, session, theme）"
        );
        assert!(!cookie_note(&headers).contains("abc123"));
        assert!(!cookie_note(&headers).contains("xyz"));
        let empty = HeaderMap::new();
        assert_eq!(cookie_note(&empty), "未附带");
    }

    #[test]
    fn upstream_5xx_error_carries_diagnosis_and_details_status() {
        // v0.0.78：5xx 转 UPSTREAM_ERROR，消息含状态+代理+解析+两次
        // 结果+Cookie 名字；details.status 保留供 fetch() 重试判定。
        let addrs: Vec<std::net::SocketAddr> = vec!["104.18.32.0:443".parse().unwrap()];
        let e = upstream_5xx_error(
            502,
            "跟随系统 http://127.0.0.1:7890",
            &addrs,
            "同样 502",
            "已附带（名字：cf_clearance）",
        );
        assert_eq!(e.code, "UPSTREAM_ERROR");
        assert_eq!(
            e.message,
            "上游返回 HTTP 502（代理: 跟随系统 http://127.0.0.1:7890；解析: 104.18.32.0:443；直连重试: 同样 502；Cookie: 已附带（名字：cf_clearance））"
        );
        assert_eq!(e.details.as_ref().unwrap()["status"], 502);
        // 无解析地址时不写解析段。
        let e = upstream_5xx_error(503, "直连", &[], "未尝试（已直连）", "未附带");
        assert_eq!(
            e.message,
            "上游返回 HTTP 503（代理: 直连；直连重试: 未尝试（已直连）；Cookie: 未附带）"
        );
    }

    #[test]
    fn no_retry_reasons_cover_every_mode() {
        // v0.0.78：未回落的原因按模式区分；慢首跳报耗时。
        use crate::proxy::Mode;
        let proxy = Some("http://127.0.0.1:7890".to_string());
        assert_eq!(
            no_retry_reason(Mode::Custom, &proxy, Duration::from_secs(1)),
            "未尝试（自定义代理）"
        );
        assert_eq!(
            no_retry_reason(Mode::Off, &None, Duration::from_secs(1)),
            "未尝试（已直连）"
        );
        assert_eq!(
            no_retry_reason(Mode::System, &None, Duration::from_secs(1)),
            "未尝试（无代理）"
        );
        assert_eq!(
            no_retry_reason(Mode::System, &proxy, Duration::from_millis(6200)),
            "未尝试（首跳耗时 6.2 秒）"
        );
    }

    #[test]
    fn short_error_truncates_long_messages() {
        let e = PluginError::new("NETWORK", "x".repeat(200));
        let s = short_error(&e);
        assert_eq!(s.chars().count(), 121);
        assert!(s.ends_with('…'));
        let e = PluginError::new("NETWORK", "短消息");
        assert_eq!(short_error(&e), "短消息");
    }
}
