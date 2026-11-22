//! Python 运行时引导：iOS 上启动 CPython 边车。
//!
//! BeeWare Python-Apple-support 3.12-b13（CPython 3.12.16）以
//! `Python.xcframework` 嵌进 iOS 包（见 `scripts/sync-ios-python.mjs`）；
//! Rust 侧用裸 FFI 调 C API，不引 PyO3——调用面只有初始化与跑一个
//! `.py` 文件，PyO3 的类型安全收益抵不过它的 iOS 交叉编译成本。
//!
//! 引导只做一件事：启动一次解释器线程，跑起边车 `main.py`，之后所有
//! 蜘蛛调用都走回环 HTTP（见 hpp 的 `python::worker`）。
//!
//! 解释器线程永不退出：`main.py` 里是自愈式 serve 循环；CPython 同进程
//! 不支持二次初始化，绝不能 `Py_Finalize` 再初始化。边车地址（API 与
//! 代理端口可能换成临时端口）由 `main.py` 写进 info 文件，引导轮询该
//! 文件 + `/health` 拿到实际地址。

use hghub_hpp::python::worker::PythonBootstrap;
use std::{path::PathBuf, sync::Arc};

#[cfg(target_os = "ios")]
use futures_util::future::BoxFuture;
#[cfg(target_os = "ios")]
use hghub_hpp::{python::client::SidecarClient, PluginError, Result};
#[cfg(target_os = "ios")]
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::Duration,
};

/// iOS 上 Python.framework 的 C API（裸 FFI）。
///
/// 框架由 Xcode 侧 Embed 进包；staticlib 里的未定义符号在 Xcode 链接
/// `.app` 时解析（`build-ios-unsigned.sh` 传 `OTHER_LDFLAGS=-framework Python`）。
/// 非 iOS 目标整个模块不编译，不产生链接要求。
#[cfg(target_os = "ios")]
mod ffi {
    use std::ffi::{c_char, c_int};

    #[link(name = "Python", kind = "framework")]
    extern "C" {
        pub fn Py_Initialize();
        pub fn Py_IsInitialized() -> c_int;
        pub fn PyRun_SimpleString(code: *const c_char) -> c_int;
    }
}

#[cfg(target_os = "ios")]
struct Inner {
    /// 边车地址文件：`<app_data_dir>/python-sidecar.json`。
    info_path: PathBuf,
    started: AtomicBool,
    /// 保证同一时间只有一个调用者在走启动流程。
    starting: tokio::sync::Mutex<()>,
    /// 解释器线程汇报的致命错误（main.py 退出时才有）。
    startup_error: Mutex<Option<String>>,
}

/// iOS 引导：保证边车已启动并返回入口地址。
#[cfg(target_os = "ios")]
pub struct IosPythonBootstrap {
    inner: Arc<Inner>,
}

#[cfg(target_os = "ios")]
impl IosPythonBootstrap {
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

    /// 轮询 info 文件 + `/health`，拿到边车实际地址。
    async fn probe(inner: &Arc<Inner>) -> Result<hghub_hpp::python::client::SidecarInfo> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(err) = inner.startup_error.lock().unwrap().clone() {
                return Err(PluginError::new(
                    "INTERNAL",
                    format!("Python 边车启动失败：{err}"),
                ));
            }
            if let Ok(text) = std::fs::read_to_string(&inner.info_path) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                    if let Some(api_base) = v.get("apiBase").and_then(|b| b.as_str()) {
                        if let Ok(client) = SidecarClient::new(api_base.to_string()) {
                            if let Ok(info) = client.health(Duration::from_secs(2)).await {
                                return Ok(info);
                            }
                        }
                    }
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(PluginError::new(
                    "INTERNAL",
                    "Python 边车 30 秒内未就绪（未写出地址文件或健康检查失败）",
                ));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

#[cfg(target_os = "ios")]
impl PythonBootstrap for IosPythonBootstrap {
    fn ensure_started(
        &self,
    ) -> BoxFuture<'static, Result<hghub_hpp::python::client::SidecarInfo>> {
        let inner = self.inner.clone();
        Box::pin(async move {
            let _guard = inner.starting.lock().await;
            if !inner.started.load(Ordering::SeqCst) {
                start_interpreter_thread(&inner)?;
                inner.started.store(true, Ordering::SeqCst);
            }
            Self::probe(&inner).await
        })
    }
}

/// 启动解释器线程：设环境变量 → `Py_Initialize` → 跑边车 main.py。
///
/// 启动前先校验包内布局（标准库与 main.py 在不在）：路径不对时
/// `Py_Initialize` 找不到 encodings 会直接 fatal abort 整个进程，
/// 必须在调用前报明错，而不是等崩溃。
#[cfg(target_os = "ios")]
fn start_interpreter_thread(inner: &Arc<Inner>) -> Result<()> {
    use std::ffi::CString;

    let exe = std::env::current_exe()
        .map_err(|e| PluginError::internal(format!("无法定位应用包路径：{e}")))?;
    let bundle = exe
        .parent()
        .ok_or_else(|| PluginError::internal("应用包路径没有父目录"))?
        .to_path_buf();
    let python_home = bundle.join("python");
    let stdlib = python_home.join("lib").join("python3.12");
    let main_py = bundle.join("app").join("main.py");
    if !stdlib.join("os.py").exists() {
        return Err(PluginError::internal(
            "Python 标准库未打进应用包（缺 python/lib/python3.12/os.py），请检查 iOS 构建的 Python 打包步骤",
        ));
    }
    if !main_py.exists() {
        return Err(PluginError::internal(
            "Python 边车未打进应用包（缺 app/main.py），请检查 iOS 构建的 Python 打包步骤",
        ));
    }
    if let Some(parent) = inner.info_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            PluginError::internal(format!("无法创建 Python 边车地址文件目录：{e}"))
        })?;
    }
    let python_path = [
        stdlib.clone(),
        stdlib.join("lib-dynload"),
        bundle.join("app"),
        bundle.join("app_packages"),
    ];
    let python_path_joined = std::env::join_paths(python_path.iter())
        .map_err(|e| PluginError::internal(format!("拼接 PYTHONPATH 失败：{e}")))?;
    // 路径用 JSON 编码进代码片段：含中文或空格的包路径也不会破坏语法。
    let main_py_json = serde_json::to_string(&main_py.to_string_lossy())
        .map_err(|e| PluginError::internal(format!("编码边车路径失败：{e}")))?;
    let snippet = format!(
        "import runpy, sys; sys.argv=['main.py']; runpy.run_path({main_py_json}, run_name='__main__')"
    );
    let info_path = inner.info_path.clone();
    let python_home = python_home.clone();
    let thread_inner = inner.clone();
    std::thread::Builder::new()
        .name("hg-python".to_string())
        .spawn(move || {
            // 环境变量必须在 Py_Initialize 之前设；只在首次启动时设一次。
            std::env::set_var("PYTHONHOME", &python_home);
            std::env::set_var("PYTHONPATH", &python_path_joined);
            std::env::set_var("HG_PY_INFO_PATH", &info_path);
            unsafe {
                ffi::Py_Initialize();
                if ffi::Py_IsInitialized() == 0 {
                    *thread_inner.startup_error.lock().unwrap() =
                        Some("Py_Initialize 失败（解释器未初始化）".to_string());
                    return;
                }
                let code = match CString::new(snippet) {
                    Ok(code) => code,
                    Err(_) => {
                        *thread_inner.startup_error.lock().unwrap() =
                            Some("边车启动代码含 NUL 字节".to_string());
                        return;
                    }
                };
                // main.py 里的自愈式 serve 循环正常情况下永不返回；
                // 返回即边车已死（CPython 不能二次初始化，只能报错）。
                let rc = ffi::PyRun_SimpleString(code.as_ptr());
                if rc != 0 {
                    *thread_inner.startup_error.lock().unwrap() = Some(format!(
                        "边车 main.py 异常退出（返回码 {rc}），请重启应用"
                    ));
                }
            }
        })
        .map_err(|e| PluginError::internal(format!("无法启动 Python 解释器线程：{e}")))?;
    Ok(())
}

/// 构造平台引导：iOS 走真 CPython 边车，其他平台走桩（UNSUPPORTED）。
pub fn python_bootstrap(info_path: PathBuf) -> Arc<dyn PythonBootstrap> {
    #[cfg(target_os = "ios")]
    {
        Arc::new(IosPythonBootstrap::new(info_path))
    }
    #[cfg(not(target_os = "ios"))]
    {
        let _ = info_path;
        Arc::new(hghub_hpp::python::worker::UnavailablePython)
    }
}
