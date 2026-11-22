//! Python 边车的 HTTP 客户端（Rust 侧）。
//!
//! 边车只监听回环地址，客户端强制直连（`no_proxy`），不经过系统代理。
//! 成功时 /call 直接返回蜘蛛方法的 JSON 结果；失败时边车返回
//! `{error}` + HTTP 500，映射为 INTERNAL（异常文本截断后进错误消息）。
use crate::{PluginError, Result};
use serde_json::{json, Value};
use std::time::Duration;

/// 边车启动信息：API 入口与代理入口地址。
#[derive(Debug, Clone)]
pub struct SidecarInfo {
    pub api_base: String,
    pub proxy_base: String,
}

/// /load 返回的探测产物。
#[derive(Debug, Clone)]
pub struct LoadInfo {
    /// 边车探测到的蜘蛛方法名（TVBox 名字）。
    pub methods: Vec<String>,
    /// "class" | "function"。
    pub style: String,
}

#[derive(Debug, Clone)]
pub struct SidecarClient {
    base: String,
    client: reqwest::Client,
}

fn truncate_error(text: &str) -> String {
    const LIMIT: usize = 1000;
    let mut out: String = text.chars().take(LIMIT).collect();
    if text.chars().count() > LIMIT {
        out.push('…');
    }
    out
}

impl SidecarClient {
    pub fn new(api_base: String) -> Result<Self> {
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .map_err(|e| PluginError::internal(format!("创建 Python 边车客户端失败：{e}")))?;
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
    ) -> Result<reqwest::Response> {
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
                    PluginError::new("INTERNAL", format!("Python 边车连接失败：{e}"))
                }
            })
    }

    /// 健康检查，同时拿到代理入口的实际地址（9978 被占时会换临时端口）。
    pub async fn health(&self, timeout: Duration) -> Result<SidecarInfo> {
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
                    PluginError::new("INTERNAL", format!("Python 边车连接失败：{e}"))
                }
            })?;
        if !rsp.status().is_success() {
            return Err(PluginError::new(
                "INTERNAL",
                format!("Python 边车健康检查失败：HTTP {}", rsp.status()),
            ));
        }
        let v: Value = rsp.json().await.map_err(|e| {
            PluginError::new("INTERNAL", format!("Python 边车健康检查返回无效：{e}"))
        })?;
        let proxy_base = v
            .get("proxyBase")
            .and_then(Value::as_str)
            .unwrap_or("http://127.0.0.1:9978")
            .to_string();
        Ok(SidecarInfo {
            api_base: self.base.clone(),
            proxy_base,
        })
    }

    /// 加载蜘蛛源码。`media_base` 是宿主回环地址，供蜘蛛的取数桥/日志桥用。
    pub async fn load(
        &self,
        key: &str,
        code: &str,
        media_base: &str,
        timeout: Duration,
    ) -> Result<LoadInfo> {
        let rsp = self
            .post_json(
                "/load",
                json!({"key": key, "code": code, "mediaBase": media_base}),
                timeout,
            )
            .await?;
        let status = rsp.status();
        let v: Value = rsp.json().await.map_err(|e| {
            PluginError::new("INTERNAL", format!("Python 边车 /load 返回无效：{e}"))
        })?;
        if !status.is_success() || v.get("success") != Some(&Value::Bool(true)) {
            let message = v
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("蜘蛛加载失败");
            return Err(PluginError::new(
                "INTERNAL",
                format!("Python 蜘蛛加载失败：{}", truncate_error(message)),
            ));
        }
        let methods = v
            .get("methods")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let style = v
            .get("style")
            .and_then(Value::as_str)
            .unwrap_or("class")
            .to_string();
        Ok(LoadInfo { methods, style })
    }

    /// 调用蜘蛛方法。成功返回结果 JSON；边车 500 时把 `{error}` 转成 INTERNAL。
    pub async fn call(
        &self,
        key: &str,
        method: &str,
        args: Vec<Value>,
        timeout: Duration,
    ) -> Result<Value> {
        let rsp = self
            .post_json(
                "/call",
                json!({"key": key, "method": method, "args": args}),
                timeout,
            )
            .await?;
        let status = rsp.status();
        let v: Value = rsp.json().await.map_err(|e| {
            PluginError::new("INTERNAL", format!("Python 边车 /call 返回无效：{e}"))
        })?;
        if !status.is_success() {
            let message = v
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("蜘蛛调用失败");
            return Err(PluginError::new(
                "INTERNAL",
                format!("Python 蜘蛛调用失败：{}", truncate_error(message)),
            ));
        }
        Ok(v)
    }

    /// 卸载蜘蛛（尽力而为：失败不报错，/load 同 key 会覆盖）。
    pub async fn unload(&self, key: &str, timeout: Duration) {
        let _ = self
            .post_json("/unload", json!({"key": key}), timeout)
            .await;
    }
}
