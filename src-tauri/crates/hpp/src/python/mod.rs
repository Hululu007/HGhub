//! Python 运行时 v1：TVBox Python 蜘蛛宿主。
//!
//! 架构是 HTTP/JSON 边车（与 ant-video 实证过的路线一致）：CPython
//! 解释器跑在应用进程内的独立线程里，Flask 服务监听回环地址；
//! Rust 侧只是 HTTP 客户端，不直接调 Python C API，不写 FFI 绑定。
//! 解释器线程一旦启动永不退出（CPython 同进程不支持二次初始化）。
//!
//! 模块分工：
//! - `scan`：.py 源码的静态扫描（内容识别、方法面探测），不执行代码；
//! - `convert`：TVBox 返回值 → HPP 形状的纯函数转换；
//! - `client`：边车 HTTP 客户端；
//! - `worker`：`PyWorker` + `RuntimeWorker` 枚举（与 JS Worker 同接口）。
pub mod client;
pub mod convert;
pub mod scan;
pub mod worker;

pub use client::{LoadInfo, SidecarClient, SidecarInfo};
pub use scan::{is_python, looks_like_python, scan_source, SpiderFace};
pub use worker::{PyWorker, PythonBootstrap, RuntimeWorker, UnavailablePython};
