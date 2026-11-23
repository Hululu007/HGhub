use super::*;
use crate::node::{NodeRequest, NodeResponse};
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
    headers: HeaderMap,
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
                .shared_hop(outgoing, follow, max, timeout, cancel)
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
        cancel: &CancellationToken,
    ) -> Result<Arc<BufferedResponse>> {
        if self.node_transport() {
            return self
                .send_api_hop(outgoing, follow, max, timeout, cancel)
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
    async fn send_api_hop(
        &self,
        outgoing: Outgoing,
        follow: bool,
        max: usize,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Arc<BufferedResponse>> {
        if cancel.is_cancelled() {
            return Err(PluginError::cancelled());
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
        // DNS 固定：每跳重新解析（wreq 路径靠 60 秒池重建，Node 路径
        // 更及时；行为差异见 node-engine.md §7）。
        let addrs = tokio::select! {
            _ = cancel.cancelled() => return Err(PluginError::cancelled()),
            value = network_policy::resolve_target(&target, self.strict()) => value.map_err(|e| PluginError::new("NETWORK", e))?,
        };
        let request = NodeRequest {
            url: target.to_string(),
            method: outgoing.method.to_string(),
            headers: outgoing.headers.clone(),
            body: outgoing.body.clone(),
            timeout,
            // 代理每跳重新解析（不缓存地址），设置改动立即生效。
            proxy_url: crate::proxy::effective_proxy_url(&crate::proxy::snapshot()),
            pinned_addrs: addrs.iter().map(|a| a.to_string()).collect(),
        };
        let response = self.node_request_with_refresh(request, cancel).await?;
        let mut headers = response.headers;
        let redacted = self.receive_cookies(
            &target,
            &mut headers,
            outgoing.generation.as_deref(),
            outgoing.cookies,
        )?;
        if matches!(response.status, 429 | 503) {
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
        match first {
            Ok(v) => Ok(v),
            Err(e) if Self::is_node_connect(&e) => {
                if self.refresh_node_client(cancel).await.is_ok() {
                    let client = self.node_client(cancel).await?;
                    tokio::select! {
                        _ = cancel.cancelled() => Err(PluginError::cancelled()),
                        r = client.request(request) => r,
                    }
                } else {
                    Err(e)
                }
            }
            Err(e) => Err(e),
        }
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
        cancel: &CancellationToken,
    ) -> Result<Arc<BufferedResponse>> {
        self.check_cooldown(outgoing.target.host_str().unwrap())?;
        if outgoing.method != Method::GET
            || outgoing.headers.contains_key("authorization")
            || outgoing.headers.contains_key("cookie")
        {
            return self.read_hop(outgoing, follow, max, timeout, cancel).await;
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
                    let work = network.read_hop(outgoing, follow, max, timeout, &cancel);
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
