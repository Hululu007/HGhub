//! Node 引擎引导：iOS 上启动 NodeMobile 边车。
//!
//! nodejs-mobile v18.20.4（Node.js 18.20.4）以 `NodeMobile.xcframework`
//! 嵌进iOS 包（见 `scripts/sync-ios-node.mjs`）；Rust 侧用裸 FFI 调
//! `node_start`，不引 Swift 桥——framework 只暴露纯 C 入口
//! `int node_start(int argc, char *argv[])`，与 Python 边车调
//! `Py_Initialize` 是同一套做法。
//!
//! 引导只做一件事：启动一次引擎线程，跑起边车 `node-app/host.js`，
//! 之后所有插件 net 请求都走回环 HTTP（见 hpp 的 `node::client`）。
//!
//! 引擎线程永不退出：`host.js` 里是自愈式 serve 循环；Node 同进程
//! 不支持二次启动，绝不能调第二次 `node_start`。边车地址（默认
//! 55101，被占时落临时端口）由 `host.js` 写进 info 文件，引导轮询
//! 该文件 + `/health` 拿到实际地址。
//!
//! Node 18 已 EOL、nodejs-mobile 实际停滞（见
//! docs/developers/node-engine.md §3.4）：本版是 pin 死 v18.20.4
//! 的假设检验，不是长期运行时承诺。

use hghub_hpp::node::NodeBootstrap;
use std::{path::PathBuf, sync::Arc};

#[cfg(target_os = "ios")]
use hghub_hpp::node::NodeInfo;

#[cfg(target_os = "ios")]
use futures_util::future::BoxFuture;
#[cfg(target_os = "ios")]
use hghub_hpp::{node::client::NodeClient, PluginError, Result};
#[cfg(target_os = "ios")]
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::Duration,
};

/// iOS 上 NodeMobile.framework 的 C 入口（裸 FFI）。
///
/// 框架由 Xcode 侧 Embed 进包；staticlib 里的未定义符号在 Xcode 链接
/// `.app` 时解析（`build-ios-unsigned.sh` 传
/// `OTHER_LDFLAGS=-framework NodeMobile`）。非 iOS 目标整个模块
/// 不编译，不产生链接要求。
#[cfg(target_os = "ios")]
mod ffi {
    use std::ffi::{c_char, c_int};

    #[link(name = "NodeMobile", kind = "framework")]
    extern "C" {
        /// 在调用线程上跑起 Node 事件循环并阻塞到引擎退出；
        /// argv: ["node", "<bundle>/node-app/host.js"]。
        pub fn node_start(argc: c_int, argv: *mut *mut c_char) -> c_int;
    }
}

#[cfg(target_os = "ios")]
struct Inner {
    /// 边车地址文件：`<app_data_dir>/node-sidecar.json`。
    info_path: PathBuf,
    started: AtomicBool,
    /// 保证同一时间只有一个调用者在走启动流程。
    starting: tokio::sync::Mutex<()>,
    /// 引擎线程汇报的致命错误（node_start 返回时才有）。
    startup_error: Mutex<Option<String>>,
}

/// iOS 引导：保证边车已启动并返回入口地址。
#[cfg(target_os = "ios")]
pub struct IosNodeBootstrap {
    inner: Arc<Inner>,
}

#[cfg(target_os = "ios")]
impl IosNodeBootstrap {
    pub fn new(info_path: PathBuf) -> Self {
        Self {
            inner: Arc::new(Inner {
                info_path,
                started: AtomicBool::new(false),
                starting: tokio::sync::Mutex::new(()),
                startup_error: Mutex::new(None),
            }),
        }
    }

    /// 轮询 info 文件 + `/health`，拿到边车实际地址并校验引擎版本。
    async fn probe(inner: &Arc<Inner>) -> Result<NodeInfo> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(err) = inner.startup_error.lock().unwrap().clone() {
                return Err(PluginError::new(
                    "INTERNAL",
                    format!("Node 引擎启动失败：{err}"),
                ));
            }
            if let Ok(text) = std::fs::read_to_string(&inner.info_path) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                    if let Some(api_base) = v.get("apiBase").and_then(|b| b.as_str()) {
                        if let Ok(client) = NodeClient::new(api_base.to_string()) {
                            if let Ok(version) = client.health(Duration::from_secs(2)).await {
                                let _ = version;
                                return Ok(NodeInfo {
                                    api_base: api_base.to_string(),
                                });
                            }
                        }
                    }
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(PluginError::new(
                    "INTERNAL",
                    "Node 边车 30 秒内未就绪（未写出地址文件或健康检查失败）",
                ));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

#[cfg(target_os = "ios")]
impl NodeBootstrap for IosNodeBootstrap {
    fn ensure_started(&self) -> BoxFuture<'static, Result<NodeInfo>> {
        let inner = self.inner.clone();
        Box::pin(async move {
            let _guard = inner.starting.lock().await;
            if !inner.started.load(Ordering::SeqCst) {
                start_engine_thread(&inner)?;
                inner.started.store(true, Ordering::SeqCst);
            }
            Self::probe(&inner).await
        })
    }
}

/// 启动引擎线程：设环境变量 → `node_start` 跑边车 host.js。
///
/// 启动前先校验包内布局（host.js 在不在）：路径不对时不要等引擎
/// 启动失败，先报明错。调用线程要 2MB 栈（nodejs-mobile 官方要求
/// 运行时 1MB、推荐 2MB）。
#[cfg(target_os = "ios")]
fn start_engine_thread(inner: &Arc<Inner>) -> Result<()> {
    use std::ffi::CString;

    let exe = std::env::current_exe()
        .map_err(|e| PluginError::internal(format!("无法定位应用包路径：{e}")))?;
    let bundle = exe
        .parent()
        .ok_or_else(|| PluginError::internal("应用包路径没有父目录"))?
        .to_path_buf();
    let host_js = bundle.join("node-app").join("host.js");
    if !host_js.exists() {
        return Err(PluginError::internal(
            "Node 边车未打进应用包（缺 node-app/host.js），请检查 iOS 构建的 Node 打包步骤",
        ));
    }
    if let Some(parent) = inner.info_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| PluginError::internal(format!("无法创建 Node 边车地址文件目录：{e}")))?;
    }
    let info_path = inner.info_path.clone();
    let thread_inner = inner.clone();
    std::thread::Builder::new()
        .name("hg-node".to_string())
        .stack_size(2 * 1024 * 1024)
        .spawn(move || {
            // 环境变量必须在 node_start 之前设；只在首次启动时设一次。
            std::env::set_var("HG_NODE_INFO_PATH", &info_path);
            let argv0 = CString::new("node").unwrap();
            let argv1 = match CString::new(host_js.to_string_lossy().into_owned()) {
                Ok(path) => path,
                Err(_) => {
                    *thread_inner.startup_error.lock().unwrap() =
                        Some("边车路径含 NUL 字节".to_string());
                    return;
                }
            };
            let mut argv = [argv0.as_ptr() as *mut _, argv1.as_ptr() as *mut _];
            // node_start 在调用线程上跑事件循环，正常情况下永不返回；
            // 返回即引擎已死（Node 不能二次启动，只能报错，不尝试重启）。
            let rc = unsafe { ffi::node_start(argv.len() as _, argv.as_mut_ptr()) };
            *thread_inner.startup_error.lock().unwrap() = Some(format!(
                "Node 引擎已停止（node_start 返回 {rc}），请重启应用"
            ));
        })
        .map_err(|e| PluginError::internal(format!("无法启动 Node 引擎线程：{e}")))?;
    Ok(())
}

/// 构造平台引导：iOS 走真 Node 边车，其他平台走桩（UNSUPPORTED）。
pub fn node_bootstrap(info_path: PathBuf) -> Arc<dyn NodeBootstrap> {
    #[cfg(target_os = "ios")]
    {
        Arc::new(IosNodeBootstrap::new(info_path))
    }
    #[cfg(not(target_os = "ios"))]
    {
        let _ = info_path;
        Arc::new(hghub_hpp::node::UnavailableNode)
    }
}
