//! Node 猫源（CatVod/Node JS 源）宿主适配。
//!
//! 分工与 Python 蜘蛛同一范式（见 docs/developers/cat-source.md）：
//! - `convert`：猫源 HTTP 服务返回值 → HPP 形状的纯函数转换；
//! - `worker`：`CatWorker`，调源的 HTTP 服务（回环地址），对外与
//!   `PyWorker` 同一组接口；
//! - `subscriptions`：订阅记录与网络/ZIP 导入的纯逻辑。
//!
//! 凭证规则（设计 §2.8，硬性）：`index.config.js` 与 `ABC.db.json`
//! 只存本机插件目录；日志里绝不出现凭证值；`GET /config` 的原始
//! 响应不返回、不记日志——边车只回站点列表（见 node::CatClient）。
use crate::{PluginError, Result};

pub mod convert;
pub mod subscriptions;
pub mod worker;

/// 读完一个 HTTP 响应体并设上限：`content-length` 超限直接报错不读；
/// 无 `content-length` 时流式读，累计超限即报错，不把整包读进内存。
pub(crate) async fn read_capped_body(
    rsp: wreq::Response,
    limit: usize,
    over_msg: &str,
) -> Result<Vec<u8>> {
    if let Some(n) = rsp.content_length() {
        if n > limit as u64 {
            return Err(PluginError::new("UPSTREAM_ERROR", over_msg));
        }
    }
    let mut out = Vec::new();
    let mut stream = rsp.bytes_stream();
    {
        use futures_util::StreamExt;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk
                .map_err(|e| PluginError::new("UPSTREAM_ERROR", format!("读取响应失败：{e}")))?;
            if out.len() + chunk.len() > limit {
                return Err(PluginError::new("UPSTREAM_ERROR", over_msg));
            }
            out.extend_from_slice(&chunk);
        }
    }
    Ok(out)
}
