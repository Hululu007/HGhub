//! Node 引擎边车的 HTTP 客户端（Rust 侧）。
//!
//! 与 Python 边车（`python::client::SidecarClient`）同款做法：边车只
//! 监听回环地址，客户端强制直连（`no_proxy`），不经过系统代理。
//! 成功时 /request 返回上游的 `{status, headers, bodyBase64}`；失败
//! 时边车返回 `{error}` + HTTP 500，映射为 INTERNAL（异常文本截断
//! 后进错误消息）。上游的 3xx/4xx/5xx 是正常返回的数据，不是错误。
use crate::{PluginError, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde_json::{json, Value};
use std::time::Duration;
use wreq::header::{HeaderMap, HeaderName, HeaderValue};

/// 一次发往 Node 边车的请求（见 docs/developers/node-engine.md §5.2）。
#[derive(Debug, Clone)]
pub struct NodeRequest {
    pub url: String,
    pub method: String,
    pub headers: HeaderMap,
    pub body: Option<Vec<u8>>,
    /// Rust 侧已 clamp 到 1–30 秒的每跳超时。
    pub timeout: Duration,
    /// 本跳生效的代理地址；None 表示直连（无 HTTP 代理）。
    pub proxy_url: Option<String>,
    /// `resolve_target` 校验+固定后的地址列表（`"ip:port"`）；Node 路径
    /// 用系统解析器原顺序（v0.0.74，不做 IPv6 优先重排）。
    pub pinned_addrs: Vec<String>,
}

/// Node 边车返回的上游响应。
#[derive(Debug, Clone)]
pub struct NodeResponse {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

#[derive(Clone)]
pub struct NodeClient {
    base: String,
    // wreq::Client 没有 Debug，所以 NodeClient 不能再派生 Debug；
    // 调用方只拿它当 Arc<NodeClient> 用，不需要 Debug。
    client: wreq::Client,
}

fn truncate_error(text: &str) -> String {
    const LIMIT: usize = 1000;
    let mut out: String = text.chars().take(LIMIT).collect();
    if text.chars().count() > LIMIT {
        out.push('…');
    }
    out
}

impl NodeClient {
    pub fn new(api_base: String) -> Result<Self> {
        // 边车是回环 HTTP，模拟无实际作用；全项目统一用同一套客户端。
        let client = wreq::Client::builder()
            .emulation(wreq_util::Profile::Chrome149)
            .no_proxy()
            .build()
            .map_err(|e| PluginError::internal(format!("创建 Node 边车客户端失败：{e}")))?;
        Ok(Self {
            base: api_base.trim_end_matches('/').to_string(),
            client,
        })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    async fn post_json(
        &self,
        path: &str,
        body: Value,
        timeout: Duration,
    ) -> Result<wreq::Response> {
        let url = format!("{}{}", self.base, path);
        self.client
            .post(&url)
            .json(&body)
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    PluginError::timeout()
                } else {
                    PluginError::new("INTERNAL", format!("Node 边车连接失败：{e}"))
                }
            })
    }

    /// 健康检查，同时校验引擎版本（应为 Node 18，即 `v18` 开头）。
    pub async fn health(&self, timeout: Duration) -> Result<String> {
        let url = format!("{}/health", self.base);
        let rsp = self
            .client
            .get(&url)
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    PluginError::timeout()
                } else {
                    PluginError::new("INTERNAL", format!("Node 边车连接失败：{e}"))
                }
            })?;
        if !rsp.status().is_success() {
            return Err(PluginError::new(
                "INTERNAL",
                format!("Node 边车健康检查失败：HTTP {}", rsp.status()),
            ));
        }
        let v: Value = rsp
            .json()
            .await
            .map_err(|e| PluginError::new("INTERNAL", format!("Node 边车健康检查返回无效：{e}")))?;
        let version = v
            .get("node")
            .and_then(Value::as_str)
            .ok_or_else(|| PluginError::new("INTERNAL", "Node 边车健康检查缺少 node 版本"))?
            .to_string();
        if !version.starts_with("v18") {
            return Err(PluginError::new(
                "INTERNAL",
                format!("Node 引擎版本不对：{version}（期望 v18.20.4）"),
            ));
        }
        Ok(version)
    }

    /// 发一次上游请求。边车 500 时把 `{error}` 转成 INTERNAL；
    /// 超时报 TIMEOUT；连不上报 INTERNAL "Node 边车连接失败"。
    pub async fn request(&self, req: NodeRequest) -> Result<NodeResponse> {
        let timeout = req.timeout;
        let mut headers = serde_json::Map::new();
        for (k, v) in &req.headers {
            if let Ok(s) = v.to_str() {
                headers.insert(k.as_str().to_string(), json!(s));
            }
        }
        let rsp = self
            .post_json(
                "/request",
                json!({
                    "url": req.url,
                    "method": req.method,
                    "headers": headers,
                    "bodyBase64": req.body.as_ref().map(|b| BASE64.encode(b)),
                    "timeoutMs": timeout.as_millis() as u64,
                    "proxyUrl": req.proxy_url,
                    "pinnedAddrs": req.pinned_addrs,
                }),
                // 边车自身超时与请求超时一致；回环往返的余量由调用方的
                // fetch 超时兜底（超时不触发重探，见 network/api.rs）。
                timeout + Duration::from_secs(2),
            )
            .await?;
        let status = rsp.status();
        let v: Value = rsp.json().await.map_err(|e| {
            PluginError::new("INTERNAL", format!("Node 边车 /request 返回无效：{e}"))
        })?;
        if !status.is_success() {
            let message = v
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("Node 边车请求失败");
            let message = truncate_error(message);
            // host.js 的 500 带 code：上游超时/连接失败/TLS 失败按
            // NETWORK / TIMEOUT 映射（与 wreq 路径的错误码一致，
            // fetch 的 GET 重试只认 NETWORK / TIMEOUT）。
            return Err(match v.get("code").and_then(Value::as_str) {
                Some("TIMEOUT") => PluginError::timeout(),
                Some("NETWORK") => {
                    PluginError::new("NETWORK", format!("Node 边车请求失败：{message}"))
                }
                Some("UPSTREAM_ERROR") => {
                    PluginError::new("UPSTREAM_ERROR", format!("Node 边车请求失败：{message}"))
                }
                _ => PluginError::new("INTERNAL", format!("Node 边车请求失败：{message}")),
            });
        }
        let status = v
            .get("status")
            .and_then(Value::as_u64)
            .ok_or_else(|| PluginError::new("INTERNAL", "Node 边车返回缺少 status"))?
            as u16;
        let mut out_headers = HeaderMap::new();
        if let Some(map) = v.get("headers").and_then(Value::as_object) {
            for (k, val) in map {
                let name = HeaderName::from_bytes(k.as_bytes())
                    .map_err(|_| PluginError::new("INTERNAL", "Node 边车返回了无效响应头名"))?;
                match val {
                    Value::Array(items) => {
                        for item in items {
                            if let Some(s) = item.as_str() {
                                let value = HeaderValue::from_str(s).map_err(|_| {
                                    PluginError::new("INTERNAL", "Node 边车返回了无效响应头值")
                                })?;
                                out_headers.append(&name, value);
                            }
                        }
                    }
                    Value::String(s) => {
                        let value = HeaderValue::from_str(s).map_err(|_| {
                            PluginError::new("INTERNAL", "Node 边车返回了无效响应头值")
                        })?;
                        out_headers.insert(name, value);
                    }
                    _ => {}
                }
            }
        }
        let body = v
            .get("bodyBase64")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let body = BASE64
            .decode(body)
            .map_err(|_| PluginError::new("INTERNAL", "Node 边车返回的响应体不是合法 base64"))?;
        Ok(NodeResponse {
            status,
            headers: out_headers,
            body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        extract::Json,
        routing::{get, post},
        Router,
    };
    use std::sync::Arc;

    /// mock Node 边车：只实现 /health 与 /request 的协议面。
    struct MockNode {
        /// /request 被调次数。
        calls: std::sync::atomic::AtomicUsize,
    }

    async fn serve_mock() -> (String, Arc<MockNode>, tokio::task::JoinHandle<()>) {
        let state = Arc::new(MockNode {
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let router = Router::new()
            .route(
                "/health",
                get(|| async { Json(json!({"ok": true, "node": "v18.20.4"})) }),
            )
            .route(
                "/request",
                post({
                    let state = state.clone();
                    move |Json(body): Json<Value>| {
                        let state = state.clone();
                        async move {
                            state
                                .calls
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            if body["url"].as_str() == Some("https://boom.invalid/") {
                                return (
                                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                    Json(json!({"error": "connect ECONNREFUSED boom"})),
                                );
                            }
                            // 回显：上游状态/头/体按协议原样返回。
                            (
                                axum::http::StatusCode::OK,
                                Json(json!({
                                    "status": 200,
                                    "headers": {
                                        "content-type": "text/html; charset=utf-8",
                                        "set-cookie": ["a=1; Path=/", "b=2; Path=/"],
                                    },
                                    "bodyBase64": BASE64.encode("node-body"),
                                })),
                            )
                        }
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (base, state, server)
    }

    fn request(url: &str) -> NodeRequest {
        NodeRequest {
            url: url.to_string(),
            method: "GET".to_string(),
            headers: HeaderMap::new(),
            body: None,
            timeout: Duration::from_secs(5),
            proxy_url: None,
            pinned_addrs: vec![],
        }
    }

    #[tokio::test]
    async fn health_accepts_node_18_and_request_round_trips() {
        let (base, state, server) = serve_mock().await;
        let client = NodeClient::new(base).unwrap();
        assert_eq!(
            client.health(Duration::from_secs(2)).await.unwrap(),
            "v18.20.4"
        );
        let rsp = client
            .request(request("https://example.com/"))
            .await
            .unwrap();
        assert_eq!(rsp.status, 200);
        assert_eq!(rsp.body, b"node-body");
        assert_eq!(
            rsp.headers.get("content-type").unwrap().to_str().unwrap(),
            "text/html; charset=utf-8"
        );
        // set-cookie 保持数组：两条都要回来。
        let cookies: Vec<_> = rsp
            .headers
            .get_all("set-cookie")
            .iter()
            .map(|v| v.to_str().unwrap().to_string())
            .collect();
        assert_eq!(cookies, vec!["a=1; Path=/", "b=2; Path=/"]);
        assert_eq!(state.calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        server.abort();
    }

    #[tokio::test]
    async fn sidecar_500_maps_to_internal_with_message() {
        let (base, _state, server) = serve_mock().await;
        let client = NodeClient::new(base).unwrap();
        let err = client
            .request(request("https://boom.invalid/"))
            .await
            .unwrap_err();
        assert_eq!(err.code, "INTERNAL");
        assert!(err.message.contains("ECONNREFUSED"), "{}", err.message);
        server.abort();
    }

    #[tokio::test]
    async fn health_rejects_wrong_node_version() {
        let router = Router::new().route(
            "/health",
            get(|| async { Json(json!({"ok": true, "node": "v24.22.1"})) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = NodeClient::new(base).unwrap();
        let err = client.health(Duration::from_secs(2)).await.unwrap_err();
        assert_eq!(err.code, "INTERNAL");
        assert!(err.message.contains("v24.22.1"), "{}", err.message);
        server.abort();
    }

    #[tokio::test]
    async fn connect_failure_maps_to_internal() {
        // 127.0.0.1:1 永远连不上：报 INTERNAL "Node 边车连接失败"，
        // 调用方凭这句文案触发重探重试（见 network/api.rs）。
        let client = NodeClient::new("http://127.0.0.1:1".to_string()).unwrap();
        let err = client
            .request(request("https://example.com/"))
            .await
            .unwrap_err();
        assert_eq!(err.code, "INTERNAL");
        assert!(err.message.contains("Node 边车连接失败"), "{}", err.message);
    }
}
