//! 猫源边车端点（`/cat/load`、`/cat/unload`、`/cat/list`）的 Rust 侧客户端。
//!
//! 与 `NodeClient` 同款做法：边车只监听回环地址，客户端强制直连
//! （`no_proxy`），不经过系统代理。内容调用不经过边车（不设
//! `/cat/call`）：Rust 侧直接调源自己的 HTTP 服务（见
//! docs/developers/cat-source.md §5.1）。
use crate::{PluginError, Result};
use serde_json::{json, Value};
use std::time::Duration;

fn truncate_error(text: &str) -> String {
    const LIMIT: usize = 1000;
    let mut out: String = text.chars().take(LIMIT).collect();
    if text.chars().count() > LIMIT {
        out.push('…');
    }
    out
}

/// 猫源 `GET /config` 返回的站点（`video.sites[]` 里的一项）：
/// 边车只保留这四个字段（见 docs/developers/cat-source.md §5.2），
/// 原始响应不返回、不记日志。
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default)]
pub struct CatSite {
    pub key: String,
    pub name: String,
    #[serde(rename = "type")]
    pub site_type: i64,
    pub api: String,
}

/// `/cat/load` 成功时边车返回的加载信息。
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default)]
pub struct CatLoadInfo {
    pub port: u16,
    #[serde(rename = "baseUrl")]
    pub base_url: String,
    pub sites: Vec<CatSite>,
}

#[derive(Clone)]
pub struct CatClient {
    base: String,
    // wreq::Client 没有 Debug，所以 CatClient 不能再派生 Debug；
    // 调用方只拿它当 Arc<CatClient> 用，不需要 Debug。
    client: wreq::Client,
}

impl CatClient {
    pub fn new(api_base: String) -> Result<Self> {
        // 边车是回环 HTTP，模拟无实际作用；全项目统一用同一套客户端。
        let client = wreq::Client::builder()
            .emulation(wreq_util::Profile::Chrome149)
            .no_proxy()
            .build()
            .map_err(|e| PluginError::internal(format!("创建猫源边车客户端失败：{e}")))?;
        Ok(Self {
            base: api_base.trim_end_matches('/').to_string(),
            client,
        })
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
                    PluginError::new("INTERNAL", format!("猫源边车连接失败：{e}"))
                }
            })
    }

    /// 加载一个猫源：边车 require 模块、调 `start(config)`、探活端口、
    /// 回 `port`/`baseUrl`/`sites`。非 2xx 或 `success != true` 时报
    /// INTERNAL“猫源加载失败：…”（边车 error 截断 1000 字）。
    pub async fn load(
        &self,
        key: &str,
        file: &str,
        config_file: Option<&str>,
        timeout: Duration,
    ) -> Result<CatLoadInfo> {
        let rsp = self
            .post_json(
                "/cat/load",
                json!({ "key": key, "file": file, "configFile": config_file }),
                timeout,
            )
            .await?;
        let status = rsp.status();
        let v: Value = rsp.json().await.map_err(|e| {
            PluginError::new("INTERNAL", format!("猫源边车 /cat/load 返回无效：{e}"))
        })?;
        if !status.is_success() || v.get("success").and_then(Value::as_bool) != Some(true) {
            let message = v
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("猫源加载失败");
            return Err(PluginError::new(
                "INTERNAL",
                format!("猫源加载失败：{}", truncate_error(message)),
            ));
        }
        let port = v
            .get("port")
            .and_then(Value::as_u64)
            .ok_or_else(|| PluginError::new("INTERNAL", "猫源边车 /cat/load 返回缺少 port"))?
            as u16;
        let base_url = v
            .get("baseUrl")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                PluginError::new("INTERNAL", "猫源边车 /cat/load 返回缺少 baseUrl")
            })?
            .to_string();
        let sites: Vec<CatSite> = serde_json::from_value(
            v.get("sites")
                .cloned()
                .ok_or_else(|| PluginError::new("INTERNAL", "猫源边车 /cat/load 返回缺少 sites"))?,
        )
        .map_err(|_| PluginError::new("INTERNAL", "猫源边车 /cat/load 返回的 sites 格式无效"))?;
        Ok(CatLoadInfo {
            port,
            base_url,
            sites,
        })
    }

    /// 卸载一个猫源：尽力而为，不返回错误（key 不存在、stop 抛错、
    /// 边车连不上都静默成功）。
    pub async fn unload(&self, key: &str, timeout: Duration) {
        let _ = self
            .post_json("/cat/unload", json!({ "key": key }), timeout)
            .await;
    }

    /// 已加载的猫源 key 列表。
    pub async fn list(&self, timeout: Duration) -> Result<Vec<String>> {
        let url = format!("{}/cat/list", self.base);
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
                    PluginError::new("INTERNAL", format!("猫源边车连接失败：{e}"))
                }
            })?;
        let status = rsp.status();
        let v: Value = rsp.json().await.map_err(|e| {
            PluginError::new("INTERNAL", format!("猫源边车 /cat/list 返回无效：{e}"))
        })?;
        if !status.is_success() {
            let message = v
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("猫源列表获取失败");
            return Err(PluginError::new(
                "INTERNAL",
                format!("猫源列表获取失败：{}", truncate_error(message)),
            ));
        }
        let sources = v
            .get("sources")
            .and_then(Value::as_array)
            .ok_or_else(|| PluginError::new("INTERNAL", "猫源边车 /cat/list 返回缺少 sources"))?
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        Ok(sources)
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

    /// mock 猫源边车：只实现 /cat/load、/cat/unload、/cat/list 的协议面。
    async fn serve_mock() -> (String, tokio::task::JoinHandle<()>) {
        let router = Router::new()
            .route(
                "/cat/load",
                post(|Json(body): Json<Value>| async move {
                    if body["file"].as_str() == Some("/no/such/plugin.js") {
                        return (
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({"success": false, "error": "猫源文件不存在"})),
                        );
                    }
                    (
                        axum::http::StatusCode::OK,
                        Json(json!({
                            "success": true,
                            "port": 9988,
                            "baseUrl": "http://127.0.0.1:9988",
                            "sites": [{"key": "a", "name": "站点 A", "type": 3, "api": "/spider/a/3"}],
                        })),
                    )
                }),
            )
            .route(
                "/cat/unload",
                post(|Json(body): Json<Value>| async move {
                    if body["key"].as_str() == Some("boom") {
                        return (
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({"success": false, "error": "stop 失败"})),
                        );
                    }
                    (
                        axum::http::StatusCode::OK,
                        Json(json!({"success": true})),
                    )
                }),
            )
            .route(
                "/cat/list",
                get(|| async { Json(json!({"sources": ["k1", "k2"]})) }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (base, server)
    }

    #[tokio::test]
    async fn load_returns_port_base_url_and_sites() {
        let (base, server) = serve_mock().await;
        let client = CatClient::new(base).unwrap();
        let info = client
            .load(
                "k1",
                "/plugins/k1/plugin.js",
                Some("/plugins/k1/index.config.js"),
                Duration::from_secs(2),
            )
            .await
            .unwrap();
        assert_eq!(info.port, 9988);
        assert_eq!(info.base_url, "http://127.0.0.1:9988");
        assert_eq!(info.sites.len(), 1);
        assert_eq!(info.sites[0].key, "a");
        assert_eq!(info.sites[0].name, "站点 A");
        assert_eq!(info.sites[0].site_type, 3);
        assert_eq!(info.sites[0].api, "/spider/a/3");
        // configFile 为 None 时传 null，同样应成功。
        let info = client
            .load("k1", "/plugins/k1/plugin.js", None, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(info.port, 9988);
        server.abort();
    }

    #[tokio::test]
    async fn load_failure_maps_to_internal() {
        let (base, server) = serve_mock().await;
        let client = CatClient::new(base).unwrap();
        let err = client
            .load("nope", "/no/such/plugin.js", None, Duration::from_secs(2))
            .await
            .unwrap_err();
        assert_eq!(err.code, "INTERNAL");
        assert!(err.message.contains("猫源加载失败"), "{}", err.message);
        assert!(err.message.contains("猫源文件不存在"), "{}", err.message);
        server.abort();
    }

    #[tokio::test]
    async fn list_returns_sources_and_unload_is_best_effort() {
        let (base, server) = serve_mock().await;
        let client = CatClient::new(base).unwrap();
        assert_eq!(
            client.list(Duration::from_secs(2)).await.unwrap(),
            vec!["k1".to_string(), "k2".to_string()]
        );
        // 200 与 500 都不报错：unload 尽力而为。
        client.unload("k1", Duration::from_secs(2)).await;
        client.unload("boom", Duration::from_secs(2)).await;
        server.abort();
    }

    #[tokio::test]
    async fn connect_failure_maps_to_internal() {
        // 127.0.0.1:1 永远连不上：报 INTERNAL "猫源边车连接失败"。
        let client = CatClient::new("http://127.0.0.1:1".to_string()).unwrap();
        let err = client
            .load("k1", "/plugins/k1/plugin.js", None, Duration::from_secs(2))
            .await
            .unwrap_err();
        assert_eq!(err.code, "INTERNAL");
        assert!(err.message.contains("猫源边车连接失败"), "{}", err.message);
    }
}
