//! Node 引擎边车 v0.0.70：插件 net 请求走真 Node 的 `https.request`。
//!
//! 架构与 Python 边车（`python` 模块）同一范式：Node 解释器跑在
//! 应用进程内的独立线程里，host.js 服务监听回环地址；Rust 侧只是
//! HTTP 客户端，不直接调 Node C API 以外的东西。引擎线程一旦启动
//! 永不退出（Node 同进程不支持二次启动）。
//!
//! 模块分工：
//! - `client`：边车 HTTP 客户端（`/health`、`/request`）；
//! - `cat_client`：猫源边车客户端（`/cat/load`、`/cat/unload`、`/cat/list`）；
//! - 本模块：`NodeBootstrap` 引导 trait 与 `UnavailableNode` 桩。
//!
//! Node 18 已 EOL、nodejs-mobile 实际停滞（见
//! docs/developers/node-engine.md §3.4）：本版是 pin 死 v18.20.4
//! 的假设检验，不是长期运行时承诺，不要把 Node 说成长期方案。
pub mod cat_client;
pub mod client;

use crate::{PluginError, Result};
pub use cat_client::{CatClient, CatLoadInfo, CatSite};
pub use client::{
    DiagnoseDomainSummary, DiagnoseOutcome, DiagnoseSummary, NodeClient, NodeRequest, NodeResponse,
};
use futures_util::future::BoxFuture;

/// 边车启动信息：API 入口地址。
#[derive(Debug, Clone)]
pub struct NodeInfo {
    pub api_base: String,
}

/// 平台引导：保证 Node 边车已启动并返回入口地址。
/// iOS 实现负责启动 Node 引擎线程；非 iOS 目标返回 UNSUPPORTED。
pub trait NodeBootstrap: Send + Sync {
    fn ensure_started(&self) -> BoxFuture<'static, Result<NodeInfo>>;
    /// 该平台有没有 Node 引导。默认 true；`UnavailableNode` 覆盖
    /// 为 false——传输分发（`network::api::read_hop`）凭它决定走
    /// Node 还是走 wreq，不用在每个调用点重复 cfg 判断。
    fn is_available(&self) -> bool {
        true
    }
}

/// 非 iOS 目标的桩引导：编译通过，但任何 Node 传输都不会走到它
/// （`is_available` 为 false，`read_hop` 直接走 wreq）。
pub struct UnavailableNode;

impl NodeBootstrap for UnavailableNode {
    fn ensure_started(&self) -> BoxFuture<'static, Result<NodeInfo>> {
        Box::pin(async {
            Err(PluginError::new(
                "UNSUPPORTED",
                "Node 引擎仅支持 iOS，当前平台不可用",
            ))
        })
    }

    fn is_available(&self) -> bool {
        false
    }
}
