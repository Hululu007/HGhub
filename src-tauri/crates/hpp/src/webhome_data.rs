//! 全 App 缓存清理（v0.0.84；v0.0.83 是“清理 WebHome 全部数据”）：
//! 设置页一键清掉全部运行时缓存，再重载 WebHome 页面。
//!
//! 顺序：
//! 1. 在 webhome 窗口里跑 JS，删三个 localStorage 键（监控快照、
//!    域名缓存、手动覆盖），取回每键清理前是否存在；
//! 2. `WebviewWindow::clear_all_browsing_data` 清 WebView 全部浏览数据
//!    （页面缓存、全部 localStorage/sessionStorage、Cookie、IndexedDB、
//!    Service Worker 缓存等）；
//! 3. 中间步骤（调用方传进来的 `clear_middle`）：全部插件的 Rust 请求
//!    Cookie 罐（内存 + 落盘的 cookies.json）、Network 响应缓存、
//!    `webhome:` 前缀的存储键；全部会话的媒体预加载缓存；插件目录下
//!    的 `__pycache__` 与 `*.pyc`；Python 边车已加载蜘蛛的运行时缓存
//!    （边车在运行时才清）；插件导入暂存目录里的文件。各类别尽力清理，
//!    某一类失败记入 `warnings` 并继续清下一类，不中断重载；
//! 4. `location.reload()` 重载 webhome 窗口；
//! 5. 最后清日志（调用方传进来的 `clear_logs`）：全部插件的运行日志
//!    ext.log 内存缓冲全清；App 侧的 debug.log 由调用方在返回后截断。
//!    日志放最后：本次清理动作自身的日志行也会随之消失，是预期的；
//!    清完后日志从空开始，下次出问题时新日志更干净、更好定位。
//!    清理失败（第 1/2/4 步）时日志不清，留着查错。
//!
//! 注意：
//! - 第 2 步是 fire-and-forget：wry 的 `clear_all_browsing_data` 调
//!   `WKWebsiteDataStore.removeData` 时 completion handler 是空的，
//!   调完立即返回、不等删完；重载可能跑在删除完成之前。但第 1 步的
//!   三个键是 JS 同步删掉并经回调确认的，所以重载后页面读到的域名
//!   列表不受影响；其余浏览数据的清除在后台完成。
//! - 第 2 步清的是 WebView 的共享数据存储（Tauri 文档原话：
//!   "clears the WKWebsiteDataStore of this webview"；未配自定义
//!   `data_store_identifier` 的窗口共享默认存储），主窗口的本地偏好
//!   也可能一并恢复默认。
//! - 不碰：App 设置（代理模式、自定义代理地址等）、导入的插件及插件
//!   文件、插件存储里非 `webhome:` 的键、下载好的视频、插件源文件
//!   （*.py、*.js、manifest）、HTML 源文件（localPage 的 *.html）。
//!   页面里的登录态会掉（Cookie 清了），用完要重登。
//! - 窗口不存在时不建窗口，返回 `WEBHOME_NOT_OPEN`。
//! - Node DoH 缓存（host.js 的 `dohCache`）60 秒自过期，不专门清。

use serde::Serialize;
use serde_json::Value;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 页面在 localStorage 里存域名数据的三个键（见 wo/app.js：
/// `wo_domain_cache`（:1799，`DOMAIN_CACHE` 持久化）、
/// `wo_monitor_snapshot`（`MONITOR_CACHE_KEY`，fetchMonitorData :596-637，
/// 1 小时缓存、过期仍用旧快照兜底）、`wo_site_domains`
/// （`SITE_DOMAIN_OVERRIDES` :263/:675，手动覆盖、同步时整站跳过）。
pub const DOMAIN_CACHE_KEYS: [&str; 3] =
    ["wo_domain_cache", "wo_monitor_snapshot", "wo_site_domains"];

/// WebHome 窗口不存在时返回的错误码（前端据此提示“先打开一次 WebHome”）。
pub const WEBHOME_NOT_OPEN: &str = "WEBHOME_NOT_OPEN";

/// 回调 10 秒不回即超时：脚本是同步的 localStorage 操作，正常几毫秒
/// 就回；窗口在清理中被关、或页面还在加载（wry 会把脚本排队、回调
/// 随脚本一起丢掉）时回调永远不来，不能无限等。
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(10);

/// 中间步骤（第 3 步）的清理结果：各类别尽力清理，某一类失败时记入
/// `warnings` 并继续清下一类，不中断重载。`py_running` 为 false 时
/// Python 边车没在运行——`_RUNTIME_CACHE` 是边车进程里的内存缓存，
/// 边车没跑就没有已加载的蜘蛛、也就没有可清的缓存，边车下次启动时
/// 本就是空的；`staging_files` 是插件导入暂存目录里清掉的文件数。
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct MiddleClearReport {
    pub plugins: usize,
    pub jar_cleared: usize,
    pub response_cache_dropped: usize,
    pub webhome_keys_deleted: usize,
    pub media_entries_dropped: usize,
    pub pycache_dirs: usize,
    pub pycache_files: usize,
    pub py_running: bool,
    pub py_spiders_total: usize,
    pub py_spiders_cleared: usize,
    pub staging_files: usize,
    pub warnings: Vec<String>,
}

/// 清理结果：`cleared` 是三个键里清理前存在、已删掉的；
/// `absent` 是本来就没有的；`jar_cleared` 是全部插件的 Rust 请求
/// Cookie 罐里清掉的总条数；其余字段见 `MiddleClearReport`；
/// `log_entries_dropped` 是最后一步清掉的插件运行日志行数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WebHomeClearReport {
    pub cleared: Vec<String>,
    pub absent: Vec<String>,
    pub jar_cleared: usize,
    pub plugins: usize,
    pub response_cache_dropped: usize,
    pub webhome_keys_deleted: usize,
    pub media_entries_dropped: usize,
    pub pycache_dirs: usize,
    pub pycache_files: usize,
    pub py_running: bool,
    pub py_spiders_total: usize,
    pub py_spiders_cleared: usize,
    pub staging_files: usize,
    pub log_entries_dropped: usize,
    pub warnings: Vec<String>,
}

/// WebHome 窗口的最小操作面：跑一段 JS 并把序列化结果交回回调、
/// 清全部浏览数据、重载页面。app crate 用本地包装
/// `WebHomeWindow(WebviewWindow)` 实现（trait 与窗口都不在本 crate 里，
/// 直接 impl 会撞孤儿规则）；测试用 mock。
pub trait WebHomeDataWindow: Send {
    fn eval_js(&self, js: String, callback: Box<dyn Fn(String) + Send>) -> Result<(), String>;
    fn clear_browsing_data(&self) -> Result<(), String>;
    fn reload_page(&self) -> Result<(), String>;
}

/// 删键脚本：逐键记清理前是否存在、删掉，返回
/// `{ok:true, existed:{<key>: bool}}`；localStorage 本身不可用时
/// 返回 `{ok:false, error, existed}`（页面自己也到处 try/catch 它）。
pub fn clear_js() -> String {
    const TEMPLATE: &str = r"(function(){var keys=[KEYS];var existed={};try{for(var i=0;i<keys.length;i++){var k=keys[i];var had=false;try{had=localStorage.getItem(k)!==null;}catch(e){}existed[k]=had;try{localStorage.removeItem(k);}catch(e){}}return {ok:true,existed:existed};}catch(e){return {ok:false,error:String((e&&e.message)||e),existed:existed};}})()";
    TEMPLATE.replace(
        "[KEYS]",
        &serde_json::json!(DOMAIN_CACHE_KEYS).to_string(),
    )
}

/// 清掉全部缓存并重载 WebHome 页面。`clear_middle` 是调用方传进来的
/// 中间步骤清理（全部插件的运行时缓存 + Python 边车运行时缓存 +
/// 导入暂存），永不失败、按类别把失败记入报告的 `warnings`；
/// `clear_logs` 是最后一步的日志清理（全部插件的 ext.log 内存缓冲），
/// 返回清掉的总行数。窗口不存在时返回 `WEBHOME_NOT_OPEN`，不建窗口、
/// `clear_middle` 与 `clear_logs` 都不跑。窗口步骤（删键、清浏览数据、
/// 重载）失败时按 v0.0.83 Fail-fast：不继续往下走、不重载、日志不清。
pub async fn clear_webhome_data<W, F, Fut, G, Fut2>(
    window: Option<&W>,
    clear_middle: F,
    clear_logs: G,
) -> Result<WebHomeClearReport, String>
where
    W: WebHomeDataWindow,
    F: FnOnce() -> Fut,
    Fut: Future<Output = MiddleClearReport> + Send,
    G: FnOnce() -> Fut2,
    Fut2: Future<Output = usize> + Send,
{
    clear_webhome_data_with_timeout(window, clear_middle, clear_logs, CALLBACK_TIMEOUT).await
}

pub async fn clear_webhome_data_with_timeout<W, F, Fut, G, Fut2>(
    window: Option<&W>,
    clear_middle: F,
    clear_logs: G,
    timeout: Duration,
) -> Result<WebHomeClearReport, String>
where
    W: WebHomeDataWindow,
    F: FnOnce() -> Fut,
    Fut: Future<Output = MiddleClearReport> + Send,
    G: FnOnce() -> Fut2,
    Fut2: Future<Output = usize> + Send,
{
    let Some(window) = window else {
        return Err(WEBHOME_NOT_OPEN.to_string());
    };
    // 1. JS 删三个键，回调确认每键清理前是否存在。
    let (tx, rx) = tokio::sync::oneshot::channel::<String>();
    let tx = Arc::new(Mutex::new(Some(tx)));
    let tx_cb = Arc::clone(&tx);
    window
        .eval_js(
            clear_js(),
            Box::new(move |result| {
                if let Ok(mut slot) = tx_cb.lock() {
                    if let Some(tx) = slot.take() {
                        let _ = tx.send(result);
                    }
                }
            }),
        )
        .map_err(|e| format!("清理脚本执行失败：{e}"))?;
    let raw = match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(raw)) => raw,
        Ok(Err(_)) => return Err("清理被中断，请重试".to_string()),
        Err(_) => {
            return Err("清理超时，请重新打开 WebHome、等页面加载完再试一次".to_string())
        }
    };
    let (cleared, absent) = parse_clear_result(&raw)?;
    // 2. 全清 WebView 浏览数据（fire-and-forget，见模块注释）。
    window
        .clear_browsing_data()
        .map_err(|e| format!("清除浏览数据失败：{e}"))?;
    // 3. 中间步骤：全部插件的运行时缓存 + Python 边车 + 导入暂存
    //    （按类别尽力清理，失败记入 warnings，不中断重载）。
    let middle = clear_middle().await;
    // 4. 重载页面（三个键已由 JS 确认删掉，内存里的旧域名列表随重载丢掉）。
    window
        .reload_page()
        .map_err(|e| format!("重新加载页面失败：{e}"))?;
    // 5. 最后清日志：本次清理动作自身的日志行也会随之消失，是预期的。
    let log_entries_dropped = clear_logs().await;
    Ok(WebHomeClearReport {
        cleared,
        absent,
        jar_cleared: middle.jar_cleared,
        plugins: middle.plugins,
        response_cache_dropped: middle.response_cache_dropped,
        webhome_keys_deleted: middle.webhome_keys_deleted,
        media_entries_dropped: middle.media_entries_dropped,
        pycache_dirs: middle.pycache_dirs,
        pycache_files: middle.pycache_files,
        py_running: middle.py_running,
        py_spiders_total: middle.py_spiders_total,
        py_spiders_cleared: middle.py_spiders_cleared,
        staging_files: middle.staging_files,
        log_entries_dropped,
        warnings: middle.warnings,
    })
}

/// 解析删键脚本的回调结果：非 JSON 或空串（脚本异常时 iOS 回调拿空串）
/// 报错；`ok:false` 时把脚本的 error 原样带回；否则按每键是否存在分
/// `cleared` / `absent`。
fn parse_clear_result(raw: &str) -> Result<(Vec<String>, Vec<String>), String> {
    let v: Value =
        serde_json::from_str(raw).map_err(|_| "清理结果无法解析，请重试".to_string())?;
    if v.get("ok").and_then(Value::as_bool) != Some(true) {
        let detail = v
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("未知错误");
        return Err(format!("清理失败：{detail}"));
    }
    let existed = v.get("existed").cloned().unwrap_or(Value::Null);
    let mut cleared = Vec::new();
    let mut absent = Vec::new();
    for key in DOMAIN_CACHE_KEYS {
        if existed.get(key).and_then(Value::as_bool) == Some(true) {
            cleared.push(key.to_string());
        } else {
            absent.push(key.to_string());
        }
    }
    Ok((cleared, absent))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{model::Meta, network::Network, node::UnavailableNode};
    use serde_json::json;
    use std::sync::{atomic::{AtomicBool, Ordering}, Mutex as StdMutex};

    struct MockWindow {
        eval_calls: StdMutex<Vec<String>>,
        callback_result: Option<String>,
        eval_error: Option<String>,
        clear_browsing_error: Option<String>,
        reload_error: Option<String>,
        reload_calls: StdMutex<usize>,
        order: Arc<StdMutex<Vec<&'static str>>>,
    }

    impl MockWindow {
        fn new(callback_result: Option<String>, order: Arc<StdMutex<Vec<&'static str>>>) -> Self {
            Self {
                eval_calls: StdMutex::new(Vec::new()),
                callback_result,
                eval_error: None,
                clear_browsing_error: None,
                reload_error: None,
                reload_calls: StdMutex::new(0),
                order,
            }
        }

        fn eval_calls(&self) -> Vec<String> {
            self.eval_calls.lock().unwrap().clone()
        }

        fn reload_calls(&self) -> usize {
            *self.reload_calls.lock().unwrap()
        }

        fn order(&self) -> Vec<&'static str> {
            self.order.lock().unwrap().clone()
        }
    }

    impl WebHomeDataWindow for MockWindow {
        fn eval_js(
            &self,
            js: String,
            callback: Box<dyn Fn(String) + Send>,
        ) -> Result<(), String> {
            self.order.lock().unwrap().push("eval_js");
            self.eval_calls.lock().unwrap().push(js);
            if let Some(e) = &self.eval_error {
                return Err(e.clone());
            }
            if let Some(r) = &self.callback_result {
                callback(r.clone());
            }
            Ok(())
        }

        fn clear_browsing_data(&self) -> Result<(), String> {
            self.order.lock().unwrap().push("clear_browsing_data");
            match &self.clear_browsing_error {
                Some(e) => Err(e.clone()),
                None => Ok(()),
            }
        }

        fn reload_page(&self) -> Result<(), String> {
            self.order.lock().unwrap().push("reload_page");
            *self.reload_calls.lock().unwrap() += 1;
            match &self.reload_error {
                Some(e) => Err(e.clone()),
                None => Ok(()),
            }
        }
    }

    fn ok_result() -> String {
        r#"{"ok":true,"existed":{"wo_domain_cache":true,"wo_monitor_snapshot":false,"wo_site_domains":true}}"#
            .to_string()
    }

    fn middle_ok(
        order: Arc<StdMutex<Vec<&'static str>>>,
        jar_cleared: usize,
    ) -> impl FnOnce() -> std::future::Ready<MiddleClearReport> {
        move || {
            order.lock().unwrap().push("middle");
            std::future::ready(MiddleClearReport {
                plugins: 1,
                jar_cleared,
                ..Default::default()
            })
        }
    }

    fn logs_ok(
        order: Arc<StdMutex<Vec<&'static str>>>,
        dropped: usize,
    ) -> impl FnOnce() -> std::future::Ready<usize> {
        move || {
            order.lock().unwrap().push("logs");
            std::future::ready(dropped)
        }
    }

    #[test]
    fn clear_js_deletes_exactly_the_three_keys() {
        let js = clear_js();
        for key in DOMAIN_CACHE_KEYS {
            assert!(js.contains(&format!("\"{key}\"")), "missing key {key}");
        }
        assert!(js.contains("localStorage.getItem(k)"));
        assert!(js.contains("localStorage.removeItem(k)"));
        assert!(
            !js.contains("localStorage.clear"),
            "must not wipe all localStorage from JS (full clear is the window call)"
        );
        assert!(!js.contains("sessionStorage"));
        assert!(!js.contains("cookie"));
        assert!(
            !js.contains("location.reload"),
            "reload is Rust-side, not in the script"
        );
    }

    #[test]
    fn clear_js_really_deletes_the_three_keys() {
        // rquickjs 真跑一次删键脚本：三个键都在、另有一个无关键；
        // 跑完三个键全删、无关键还在、existed 如实回报。
        use rquickjs::{Context, Runtime};
        let runtime = Runtime::new().unwrap();
        let ctx = Context::full(&runtime).unwrap();
        let (result, remaining): (String, String) = ctx.with(|ctx| {
            let _: rquickjs::Value = ctx
                .eval(
                    r#"globalThis.localStorage = (function(){var store={"wo_domain_cache":"{}","wo_monitor_snapshot":"{}","wo_site_domains":"a\nb","other_key":"keep"};return {getItem:function(k){return k in store?store[k]:null;},removeItem:function(k){delete store[k];},_store:store};})();"#,
                )
                .unwrap();
            let result: String = ctx
                .eval(format!("JSON.stringify({})", clear_js()))
                .unwrap();
            let remaining: String = ctx
                .eval("JSON.stringify(Object.keys(localStorage._store))")
                .unwrap();
            (result, remaining)
        });
        let v: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["existed"]["wo_domain_cache"], true);
        assert_eq!(v["existed"]["wo_monitor_snapshot"], true);
        assert_eq!(v["existed"]["wo_site_domains"], true);
        assert_eq!(remaining, r#"["other_key"]"#);
    }

    #[tokio::test]
    async fn no_window_returns_webhome_not_open_and_middle_and_logs_never_run() {
        let middle_called = Arc::new(AtomicBool::new(false));
        let logs_called = Arc::new(AtomicBool::new(false));
        let middle_called_cb = Arc::clone(&middle_called);
        let logs_called_cb = Arc::clone(&logs_called);
        let err = clear_webhome_data::<MockWindow, _, _, _, _>(
            None,
            move || {
                middle_called_cb.store(true, Ordering::SeqCst);
                std::future::ready(MiddleClearReport::default())
            },
            move || {
                logs_called_cb.store(true, Ordering::SeqCst);
                std::future::ready(0)
            },
        )
        .await
        .unwrap_err();
        assert_eq!(err, WEBHOME_NOT_OPEN);
        assert!(
            !middle_called.load(Ordering::SeqCst),
            "middle must not run without a window"
        );
        assert!(
            !logs_called.load(Ordering::SeqCst),
            "logs must not run without a window"
        );
    }

    #[tokio::test]
    async fn full_clear_runs_all_steps_in_order_and_reloads() {
        let order = Arc::new(StdMutex::new(Vec::new()));
        let window = MockWindow::new(Some(ok_result()), Arc::clone(&order));
        let report = clear_webhome_data(
            Some(&window),
            middle_ok(Arc::clone(&order), 3),
            logs_ok(Arc::clone(&order), 5),
        )
        .await
        .unwrap();
        assert_eq!(
            report,
            WebHomeClearReport {
                cleared: vec!["wo_domain_cache".to_string(), "wo_site_domains".to_string()],
                absent: vec!["wo_monitor_snapshot".to_string()],
                jar_cleared: 3,
                plugins: 1,
                response_cache_dropped: 0,
                webhome_keys_deleted: 0,
                media_entries_dropped: 0,
                pycache_dirs: 0,
                pycache_files: 0,
                py_running: false,
                py_spiders_total: 0,
                py_spiders_cleared: 0,
                staging_files: 0,
                log_entries_dropped: 5,
                warnings: Vec::new(),
            }
        );
        assert_eq!(window.eval_calls(), vec![clear_js()]);
        assert_eq!(
            window.order(),
            vec![
                "eval_js",
                "clear_browsing_data",
                "middle",
                "reload_page",
                "logs"
            ]
        );
        assert_eq!(window.reload_calls(), 1);
    }

    #[tokio::test]
    async fn middle_warnings_are_reported_and_reload_and_logs_still_run() {
        // 中间步骤按类别尽力清理：某一类失败只记 warnings，不中断重载与日志清理。
        let order = Arc::new(StdMutex::new(Vec::new()));
        let window = MockWindow::new(Some(ok_result()), Arc::clone(&order));
        let order_middle = Arc::clone(&order);
        let report = clear_webhome_data(
            Some(&window),
            move || {
                order_middle.lock().unwrap().push("middle");
                std::future::ready(MiddleClearReport {
                    plugins: 1,
                    jar_cleared: 2,
                    warnings: vec!["插件 x 的 Cookie 罐清理失败：boom".to_string()],
                    ..Default::default()
                })
            },
            logs_ok(Arc::clone(&order), 4),
        )
        .await
        .unwrap();
        assert_eq!(report.jar_cleared, 2);
        assert_eq!(
            report.warnings,
            vec!["插件 x 的 Cookie 罐清理失败：boom".to_string()]
        );
        assert_eq!(report.log_entries_dropped, 4);
        assert_eq!(
            window.order(),
            vec![
                "eval_js",
                "clear_browsing_data",
                "middle",
                "reload_page",
                "logs"
            ]
        );
        assert_eq!(window.reload_calls(), 1);
    }

    #[tokio::test]
    async fn eval_failure_propagates_without_further_steps() {
        let order = Arc::new(StdMutex::new(Vec::new()));
        let mut window = MockWindow::new(Some(ok_result()), Arc::clone(&order));
        window.eval_error = Some("eval boom".to_string());
        let err = clear_webhome_data(
            Some(&window),
            middle_ok(Arc::clone(&order), 3),
            logs_ok(Arc::clone(&order), 0),
        )
        .await
        .unwrap_err();
        assert!(err.contains("eval boom"), "{err}");
        assert_eq!(window.order(), vec!["eval_js"]);
        assert_eq!(window.reload_calls(), 0);
    }

    #[tokio::test]
    async fn bad_callback_json_errors_without_further_steps() {
        let order = Arc::new(StdMutex::new(Vec::new()));
        let window = MockWindow::new(Some("not json".to_string()), Arc::clone(&order));
        let err = clear_webhome_data(
            Some(&window),
            middle_ok(Arc::clone(&order), 3),
            logs_ok(Arc::clone(&order), 0),
        )
        .await
        .unwrap_err();
        assert!(err.contains("无法解析"), "{err}");
        assert_eq!(window.order(), vec!["eval_js"]);
        assert_eq!(window.reload_calls(), 0);
    }

    #[tokio::test]
    async fn callback_ok_false_errors_without_further_steps() {
        let order = Arc::new(StdMutex::new(Vec::new()));
        let window = MockWindow::new(
            Some(r#"{"ok":false,"error":"denied","existed":{}}"#.to_string()),
            Arc::clone(&order),
        );
        let err = clear_webhome_data(
            Some(&window),
            middle_ok(Arc::clone(&order), 3),
            logs_ok(Arc::clone(&order), 0),
        )
        .await
        .unwrap_err();
        assert!(err.contains("denied"), "{err}");
        assert_eq!(window.order(), vec!["eval_js"]);
        assert_eq!(window.reload_calls(), 0);
    }

    #[tokio::test]
    async fn callback_never_fires_times_out() {
        let order = Arc::new(StdMutex::new(Vec::new()));
        let window = MockWindow::new(None, Arc::clone(&order));
        let err = clear_webhome_data_with_timeout(
            Some(&window),
            middle_ok(Arc::clone(&order), 3),
            logs_ok(Arc::clone(&order), 0),
            Duration::from_millis(50),
        )
        .await
        .unwrap_err();
        assert!(err.contains("超时"), "{err}");
        assert_eq!(window.order(), vec!["eval_js"]);
        assert_eq!(window.reload_calls(), 0);
    }

    #[tokio::test]
    async fn browsing_data_failure_propagates_without_middle_logs_or_reload() {
        let order = Arc::new(StdMutex::new(Vec::new()));
        let mut window = MockWindow::new(Some(ok_result()), Arc::clone(&order));
        window.clear_browsing_error = Some("store boom".to_string());
        let err = clear_webhome_data(
            Some(&window),
            middle_ok(Arc::clone(&order), 3),
            logs_ok(Arc::clone(&order), 0),
        )
        .await
        .unwrap_err();
        assert!(err.contains("store boom"), "{err}");
        assert_eq!(window.order(), vec!["eval_js", "clear_browsing_data"]);
        assert_eq!(window.reload_calls(), 0);
    }

    #[tokio::test]
    async fn reload_failure_is_reported_and_logs_stay() {
        // 重载失败时日志不清（留着查错），logs 闭包不跑。
        let order = Arc::new(StdMutex::new(Vec::new()));
        let mut window = MockWindow::new(Some(ok_result()), Arc::clone(&order));
        window.reload_error = Some("reload boom".to_string());
        let err = clear_webhome_data(
            Some(&window),
            middle_ok(Arc::clone(&order), 3),
            logs_ok(Arc::clone(&order), 0),
        )
        .await
        .unwrap_err();
        assert!(err.contains("reload boom"), "{err}");
        assert_eq!(
            window.order(),
            vec!["eval_js", "clear_browsing_data", "middle", "reload_page"]
        );
    }

    #[test]
    fn clear_cookie_jar_empties_memory_and_disk() {
        // 罐子里 1 条普通 Cookie + 1 条 web 登录 Cookie（落盘文件直接写，
        // 与 Network::new 读的两种格式之一相同）；清完返回 2、落盘文件
        // 变成空数组、重建 Network 读回的也是空罐。
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("cookies.json"),
            json!({
                "ordinary": [{
                    "name": "sid", "value": "1", "domain": "example.com",
                    "path": "/", "hostOnly": false, "secure": false,
                    "httpOnly": false, "expiresAt": null
                }],
                "web_logins": {
                    "https://example.com": {
                        "generation": "g1",
                        "cookies": [{
                            "name": "sess", "value": "2", "domain": "example.com",
                            "path": "/", "hostOnly": false, "secure": false,
                            "httpOnly": false, "expiresAt": null
                        }]
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
        let meta = Meta::parse(
            json!({"id":"fixture.clear","name":"Clear","protocol":1,"version":"1.0.0","allowInsecureHttp":true}),
        )
        .unwrap();
        let network = Network::new(
            meta,
            dir.path().into(),
            Some(false),
            Default::default(),
            Arc::new(UnavailableNode),
        )
        .unwrap();
        let before: Value = network
            .cookies("get", json!({"url": "https://example.com/"}))
            .unwrap();
        assert_eq!(before.as_array().unwrap().len(), 1);
        assert_eq!(network.clear_cookie_jar().unwrap(), 2);
        let after: Value = network
            .cookies("get", json!({"url": "https://example.com/"}))
            .unwrap();
        assert_eq!(after.as_array().unwrap().len(), 0);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("cookies.json")).unwrap(),
            "[]"
        );
        let meta2 = Meta::parse(
            json!({"id":"fixture.clear","name":"Clear","protocol":1,"version":"1.0.0","allowInsecureHttp":true}),
        )
        .unwrap();
        let network2 = Network::new(
            meta2,
            dir.path().into(),
            Some(false),
            Default::default(),
            Arc::new(UnavailableNode),
        )
        .unwrap();
        let reloaded: Value = network2
            .cookies("get", json!({"url": "https://example.com/"}))
            .unwrap();
        assert_eq!(reloaded.as_array().unwrap().len(), 0);
    }
}
