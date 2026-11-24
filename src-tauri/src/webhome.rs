//! WebHome 宿主面（08 §10）：为声明了 meta.webhome 的插件开一个独立 WebView
//! 窗口，加载其首页并注入 window.fm 桥（见 webhome_sdk.js）。窗口一次只开
//! 一个，绑定一个插件；页面只能操作绑定它的那个插件。窗口常驻不销毁：
//! × 只是隐藏，重开直接显示，换源时导航——iOS 上窗口销毁后从窗口表注销
//! 的时机由系统把握，关了重建会撞 label 已存在。
use crate::{
    browser,
    clearance::{self, Clearance},
    plugins::PluginHost,
};
use hghub_hpp::{
    manager::PluginManager,
    webhome::{self, Outcome},
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent};
use url::Url;

const LABEL: &str = "webhome";
const SDK: &str = include_str!("webhome_sdk.js");

pub struct WebHomeState {
    session: Mutex<Option<WebHomeSession>>,
    /// 主窗口推送的直链播放状态，供 player.status 读取。
    player_state: Mutex<Value>,
    /// open_webhome 串行化：两次打开不能交错（建窗与导航、写 session
    /// 之间不能插进另一次打开），否则窗口与 session 会对不上。
    open_lock: tokio::sync::Mutex<()>,
    /// clearance 串行化：同一时间只跑一趟 WebView 挑战（两趟并行会
    /// 抢同一个隐藏 iframe、顶层导航也会交错）；等锁的后来者拿到后
    /// 照常跑一趟——WebView 的 Cookie 存储是权威来源，通行证已在
    /// 的话第一轮 poll 就拿到，不会白等 30 秒。
    clearance_lock: tokio::sync::Mutex<()>,
}

struct WebHomeSession {
    plugin_id: String,
}

impl Default for WebHomeState {
    fn default() -> Self {
        Self {
            session: Mutex::new(None),
            player_state: Mutex::new(json!({
                "playing": false,
                "positionSec": 0,
                "durationSec": 0,
            })),
            open_lock: tokio::sync::Mutex::new(()),
            clearance_lock: tokio::sync::Mutex::new(()),
        }
    }
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// 诊断日志（v0.0.89）：WebHome 关键操作记进 App 自己的 debug.log（落盘，
/// 闪退后还在），供复现后回看最后几行定位崩点。只记 method + 插件 id +
/// 简短标记，不记响应体、完整 URL、密钥/Cookie；写失败不影响调用本身。
fn debug_log(app: &AppHandle, msg: &str) {
    if let Ok(dir) = app.path().app_data_dir() {
        let _ = crate::logging::append_debug(&dir, msg);
    }
}

/// webhome_call 的简短参数标记：net.request 与播放类方法只记目标 host
///（不记完整 URL），其他方法不记参数。
fn call_marker(method: &str, payload: &Value) -> String {
    let url = match method {
        "req" | "net.request" => payload["url"].as_str(),
        m if m.starts_with("player.") => payload["url"].as_str(),
        _ => None,
    };
    match url {
        Some(u) => match u.parse::<Url>() {
            Ok(parsed) => format!(" host={}", parsed.host_str().unwrap_or("?")),
            Err(_) => " host=?".to_string(),
        },
        None => String::new(),
    }
}

/// 错误文案进日志前截断（前 120 字）并去掉完整 URL：遇到 "://" 就截到
/// scheme 开头。诊断后缀里的 IP 地址与状态码不受影响。
fn redact_error(e: &str) -> String {
    let mut s: String = e.chars().take(120).collect();
    if let Some(pos) = s.find("://") {
        let cut = s[..pos]
            .rfind(|c: char| !c.is_ascii_alphanumeric() && c != '+' && c != '-' && c != '.')
            .map(|i| i + 1)
            .unwrap_or(0);
        s.truncate(cut);
        s.push('…');
    }
    s
}

fn outcome_kind(outcome: &Outcome) -> &'static str {
    match outcome {
        Outcome::Value(_) => "Value",
        Outcome::Play { .. } => "Play",
        Outcome::OpenVod(_) => "OpenVod",
        Outcome::OpenInAppBrowser { .. } => "OpenInAppBrowser",
        Outcome::Toast(_) => "Toast",
        Outcome::PlayerControl(..) => "PlayerControl",
        Outcome::OpenWebhome(_) => "OpenWebhome",
        Outcome::OpenLive => "OpenLive",
        Outcome::OpenKeep => "OpenKeep",
        Outcome::OpenSetting => "OpenSetting",
        Outcome::SetToolbar(_) => "SetToolbar",
    }
}

#[tauri::command]
pub async fn open_webhome(
    app: AppHandle,
    host: State<'_, PluginHost>,
    state: State<'_, WebHomeState>,
    plugin_id: String,
) -> Result<Value, String> {
    open_webhome_inner(&app, &host, &state, plugin_id).await
}

/// 打开 / 换源的共用实现：主窗口的 open_webhome 命令与 WebHome 窗口里
/// 小房子面板的 app.openWebhome 都走这里，不复制第二份逻辑。
async fn open_webhome_inner(
    app: &AppHandle,
    host: &PluginHost,
    state: &WebHomeState,
    plugin_id: String,
) -> Result<Value, String> {
    // 整个打开过程串行化：两次打开不能交错（建窗与导航、写 session
    // 之间不能插进另一次打开），否则窗口与 session 会对不上。
    let _open_guard = state.open_lock.lock().await;
    let manager = host.manager(app).await.map_err(err)?;
    let meta = manager.meta(&plugin_id).await.map_err(err)?;
    // 远程用 homePage；本地页面用当前回环地址（端口每次启动都变，现算）。
    let home_page = hghub_hpp::media::MediaSessions::webhome_page_url(&manager, &plugin_id)
        .await
        .map_err(err)?;
    if let Some(window) = app.get_webview_window(LABEL) {
        let same = state
            .session
            .lock()
            .map_err(err)?
            .as_ref()
            .is_some_and(|s| s.plugin_id == plugin_id);
        if same {
            // 窗口一直活着（× 只是隐藏）：直接显示，页面状态保留。
            window.show().map_err(err)?;
            window.set_focus().map_err(err)?;
            debug_log(app, &format!("[webhome] 窗口显示：{plugin_id}（已开同源）"));
            return Ok(json!({ "url": home_page }));
        }
        // 换源：让活着的 webview 导航到新首页，桥在导航后重新注入
        //（wry 把初始化脚本挂成 AtDocumentStart 的 WKUserScript，每
        // 次加载都跑），再把 session 更新成新插件。
        window
            .navigate(home_page.parse().map_err(err)?)
            .map_err(err)?;
        *state.session.lock().map_err(err)? = Some(WebHomeSession { plugin_id: plugin_id.clone() });
        window.show().map_err(err)?;
        window.set_focus().map_err(err)?;
        debug_log(app, &format!("[webhome] 窗口导航：{plugin_id}"));
        return Ok(json!({ "url": home_page }));
    }
    // 窗口不存在才建新窗。建窗仍撞 label 说明有窗口存在但前面没认
    // 出来，聚焦它只会重现“打开了却什么都没出现”的静默失败；直接报出来。
    let window = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::External(home_page.parse().map_err(err)?))
        .title(meta.name.clone())
        .initialization_script(SDK)
        .build()
        .map_err(err)?;
    // iOS：WKWebView 默认不开启边缘滑动前进/后退。wo 页面详情用
    // history.pushState，系统手势后退正好触发 history.back() 关详情。
    // 只开系统级手势，不在 JS 里拦截手势，避免与页面内横滑冲突。
    #[cfg(target_os = "ios")]
    enable_back_forward_gestures(&window, app);
    // 页面按 fmpause/fmresume 感知前后台（04 §14）。
    let focused_window = window.clone();
    window.on_window_event(move |event| {
        if let WindowEvent::Focused(focused) = event {
            let _ = focused_window.eval(if *focused {
                "window.dispatchEvent(new Event('fmresume'))"
            } else {
                "window.dispatchEvent(new Event('fmpause'))"
            });
        }
    });
    window.set_focus().map_err(err)?;
    *state.session.lock().map_err(err)? = Some(WebHomeSession { plugin_id: plugin_id.clone() });
    debug_log(app, &format!("[webhome] 窗口新建：{plugin_id}"));
    Ok(json!({ "url": home_page }))
}

/// iOS：给 WebHome 窗口开 WKWebView 的 back-forward 手势。Tauri 的
/// WebviewWindowBuilder 没暴露这项，用 with_webview 拿原生 WKWebView
/// 再开。只在建窗时开一次，窗口常驻，手势跟窗口走。开失败不影响开窗：
/// 窗口照常可用，只是右滑回不去。
#[cfg(target_os = "ios")]
fn enable_back_forward_gestures(window: &WebviewWindow, app: &AppHandle) {
    if let Err(e) = window.as_ref().with_webview(|webview| {
        let ptr = webview.inner();
        // SAFETY：inner() 返回的是活着的 WKWebView；with_webview 的闭包
        // 在主线程上跑，WKWebView 的属性设置要求主线程。
        unsafe {
            let wk = &*(ptr as *const objc2_web_kit::WKWebView);
            wk.setAllowsBackForwardNavigationGestures(true);
        }
    }) {
        debug_log(app, &format!("[webhome] 手势开启失败：{e}"));
    }
}

#[tauri::command]
pub async fn webhome_close(app: AppHandle) -> Result<(), String> {
    // × 只是隐藏窗口，不销毁：iOS 上窗口销毁后从窗口表注销的时机由
    // 系统把握，关了重建会撞 label 已存在。session 保留（窗口还绑
    // 着这个插件，hide 后 webhome_call 还要用它）。hide 不一定触发
    // Focused(false)，顺手补发 fmpause；eval 失败不影响关闭。
    if let Some(window) = app.get_webview_window(LABEL) {
        window.hide().map_err(err)?;
        let _ = window.eval("window.dispatchEvent(new Event('fmpause'))");
        debug_log(&app, "[webhome] 窗口隐藏");
    }
    Ok(())
}

/// 去主窗口的 Outcome（播放 / 打开详情 / 直播 / 收藏 / 设置）处理完后
/// 把 WebHome 窗口藏起来：主窗口原生播放器在播、WebHome 窗口盖在上面，
/// 用户会以为点了没反应。hide-and-reuse：窗口不销毁，用户可从主窗口
/// 底部 Webhome 页签回来。先发事件再 hide——事件已发出，主窗口那边
/// 已经在播；hide 失败只记日志，不让整个 call 报错。
fn hide_webhome_after_open(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(LABEL) {
        if let Err(e) = window.hide() {
            debug_log(app, &format!("[webhome] 播放后隐藏窗口失败：{e}"));
        } else {
            // 与 webhome_close 同款：hide 不一定触发 Focused(false)，
            // 补发 fmpause 让页面感知切后台。
            let _ = window.eval("window.dispatchEvent(new Event('fmpause'))");
            debug_log(app, "[webhome] 播放后窗口隐藏");
        }
    }
}

#[tauri::command]
pub async fn webhome_call(
    app: AppHandle,
    host: State<'_, PluginHost>,
    state: State<'_, WebHomeState>,
    method: String,
    payload: Value,
) -> Result<Value, String> {
    let plugin_id = state
        .session
        .lock()
        .map_err(err)?
        .as_ref()
        .map(|s| s.plugin_id.clone())
        .ok_or_else(|| "WebHome 会话未绑定插件".to_string())?;
    debug_log(
        &app,
        &format!(
            "[webhome] call 入：{plugin_id} {method}{}",
            call_marker(&method, &payload)
        ),
    );
    let manager = host.manager(&app).await.map_err(err)?;
    let player_state = state.player_state.lock().map_err(err)?.clone();
    let outcome = match dispatch_with_clearance(
        &app,
        &manager,
        &state,
        &plugin_id,
        &method,
        payload,
        player_state,
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(e) => {
            debug_log(&app, &format!("[webhome] call 出：Err {}", redact_error(&e)));
            return Err(e);
        }
    };
    let kind = outcome_kind(&outcome);
    let main = app.get_webview_window("main");
    let result = match outcome {
        Outcome::Value(value) => Ok(value),
        Outcome::Play { media, title } => {
            if let Some(main) = main {
                main.emit("webhome-play", json!({ "media": media, "title": title }))
                    .map_err(err)?;
            }
            hide_webhome_after_open(&app);
            Ok(json!({ "code": 0 }))
        }
        Outcome::OpenVod(detail) => {
            if let Some(main) = main {
                main.emit(
                    "webhome-open-vod",
                    json!({ "pluginId": plugin_id, "detail": detail }),
                )
                .map_err(err)?;
            }
            hide_webhome_after_open(&app);
            Ok(json!({ "code": 0 }))
        }
        Outcome::OpenInAppBrowser { url, toast } => {
            // 网盘分享链 / go.php 外链：在 App 内专用浏览器窗口打开，不走
            // 系统浏览器；打开成功再给主窗口一句提示，别静默。WebHome
            // 窗口不藏：浏览器窗口关掉后用户回到原来的详情页。
            browser::open_browser(&app, &url).map_err(err)?;
            if let Some(main) = main {
                main.emit("webhome-toast", json!({ "message": toast }))
                    .map_err(err)?;
            }
            Ok(json!({ "code": 0 }))
        }
        Outcome::Toast(message) => {
            if let Some(main) = main {
                main.emit("webhome-toast", json!({ "message": message }))
                    .map_err(err)?;
            }
            Ok(json!({ "code": 0 }))
        }
        Outcome::PlayerControl(action, seconds) => {
            if let Some(main) = main {
                main.emit(
                    "webhome-player-control",
                    json!({ "action": action, "seconds": seconds }),
                )
                .map_err(err)?;
            }
            Ok(json!({ "code": 0 }))
        }
        Outcome::OpenWebhome(target_id) => {
            // 小房子面板切源：与主窗口的 open_webhome 同一条复用逻辑
            //（窗口已存在就导航，窗口不存在才建新窗）。
            open_webhome_inner(&app, &host, &state, target_id).await
        }
        Outcome::OpenLive => {
            if let Some(main) = main {
                main.emit("webhome-open-live", json!({ "pluginId": plugin_id }))
                    .map_err(err)?;
            }
            hide_webhome_after_open(&app);
            Ok(json!({ "code": 0 }))
        }
        Outcome::OpenKeep => {
            if let Some(main) = main {
                main.emit("webhome-open-keep", json!({ "pluginId": plugin_id }))
                    .map_err(err)?;
            }
            hide_webhome_after_open(&app);
            Ok(json!({ "code": 0 }))
        }
        Outcome::OpenSetting => {
            if let Some(main) = main {
                main.emit("webhome-open-setting", json!({ "pluginId": plugin_id }))
                    .map_err(err)?;
            }
            hide_webhome_after_open(&app);
            Ok(json!({ "code": 0 }))
        }
        Outcome::SetToolbar(visible) => {
            if let Some(window) = app.get_webview_window(LABEL) {
                window
                    .eval(&format!(
                        "window.__hgWebHome && window.__hgWebHome.setToolbar({visible})"
                    ))
                    .map_err(err)?;
            }
            Ok(json!({ "code": 0 }))
        }
    };
    match &result {
        Ok(_) => debug_log(&app, &format!("[webhome] call 出：Ok {kind}")),
        Err(e) => debug_log(&app, &format!("[webhome] call 出：Err {}", redact_error(e))),
    }
    result
}

/// WKHTTPCookieStore 里一只 Cookie 的最小快照（v0.0.91）：只含桥与
/// clearance 要用的字段，全是普通 Rust 数据，可跨线程经 oneshot 回传。
/// `domain` 是 NSHTTPCookie 的 domain 原值（一定有值——wry 的
/// `cookie_from_wkwebview` 里 domain 一定被设置，所以这里没有 host_only
/// 字段；转 `WebViewCookie` 时 host_only 恒为 false，与旧转换一致）。
#[cfg(target_os = "ios")]
pub(crate) struct RawWebViewCookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub secure: bool,
    pub http_only: bool,
    /// unix 毫秒；None = 会话 Cookie（无 expiresDate）。
    pub expires_at: Option<u64>,
}

/// 一只 NSHTTPCookie 按 wry `cookies_for_url` 的同一规则过滤并转成
/// `RawWebViewCookie`，不过滤的返回 None。规则照搬 wry 0.57.1
/// `wkwebview/mod.rs:1292`：domain 与 URL 的 domain 相等，且 secure
/// 的只给 https（http 时只给 localhost 与环回地址），不 secure 的都行。
#[cfg(target_os = "ios")]
fn raw_cookie(
    cookie: &objc2_foundation::NSHTTPCookie,
    target_host: &Option<String>,
    target_scheme: &str,
) -> Option<RawWebViewCookie> {
    use std::net::Ipv4Addr;
    use std::str::FromStr;

    let domain = cookie.domain().to_string();
    let secure = cookie.isSecure();
    let host_matches = Some(domain.as_str()) == target_host.as_deref();
    let scheme_ok = (secure && target_scheme == "https")
        || (secure
            && target_scheme == "http"
            && (matches!(target_host.as_deref(), Some("localhost"))
                || target_host
                    .as_deref()
                    .and_then(|d| Ipv4Addr::from_str(d).ok())
                    .map(|ip| ip.is_loopback())
                    .unwrap_or(false)))
        || !secure;
    if !host_matches || !scheme_ok {
        return None;
    }
    Some(RawWebViewCookie {
        name: cookie.name().to_string(),
        value: cookie.value().to_string(),
        domain,
        path: cookie.path().to_string(),
        secure,
        http_only: cookie.isHTTPOnly(),
        expires_at: cookie
            .expiresDate()
            .map(|d| (d.timeIntervalSince1970().max(0.0) * 1000.0) as u64),
    })
}

/// 从 WebHome 窗口的 Cookie 存储异步读 `url` 的 Cookie（v0.0.91，iOS）。
///
/// v0.0.86–v0.0.90 的旧实现调同步 `WebviewWindow::cookies_for_url`：
/// wry 的 `cookies()` 在主线程上用 `wait_for_blocking_operation` 转嵌套
/// run loop 等 `getAllCookies` 回调；tao 正处在用户回调分发中
/// （InUserCallback）时，嵌套循环让 BeforeWaiting 观察者重入，tao 0.37.2
/// `app_state.rs:417` panic（真机 2026-11-24 21:05 的 `[panic]` 行）。
///
/// 新实现只在主线程上发异步请求：经 `WebviewWindow::run_on_main_thread`
/// 把闭包以正常用户事件投递到主线程（不转嵌套循环），闭包里调
/// `WKWebsiteDataStore::defaultDataStore().httpCookieStore().getAllCookies`
/// 并立即返回；completion 里把 `NSArray<NSHTTPCookie>` 按 wry 的同一规则
/// 过滤后经 oneshot 发回；tokio 侧 `await` oneshot（普通 park，不转
/// loop），2 秒超时返回空列表——与原来“读不到返回空”的语义一致。
/// WebHome 窗口用默认 data store（wry mod.rs:286，未配自定义
/// data_store_identifier 的窗口共享默认存储），就是同一个 store。
/// `getAllCookies` 的 completion 与 oneshot 发送失败都静默返回空列表，
/// 不失败请求——桥只是给引擎补 Cookie，罐子本身仍是权威。
#[cfg(target_os = "ios")]
pub(crate) async fn read_webview_cookies_async(
    window: &WebviewWindow,
    url: &Url,
) -> Vec<RawWebViewCookie> {
    use objc2::MainThreadMarker;
    use objc2_foundation::{NSArray, NSHTTPCookie};
    use objc2_web_kit::WKWebsiteDataStore;
    use std::ptr::NonNull;

    let target_host = url.domain().map(str::to_string);
    let target_scheme = url.scheme().to_string();
    let (tx, rx) = tokio::sync::oneshot::channel::<Vec<RawWebViewCookie>>();
    // getAllCookies 的 completion block 要求 `Fn`（RcBlock::new 的
    // IntoBlock 约束），而 oneshot::Sender::send 会消费 tx、只剩
    // FnOnce——v0.0.91 首版构建就栽在这（E0525）。用 Mutex<Option<_>>
    // 包一层，block 里只借 Mutex、第一次调用时 take 出来发掉。
    let tx = std::sync::Mutex::new(Some(tx));
    let posted = window
        .run_on_main_thread(move || {
            let Some(mtm) = MainThreadMarker::new() else {
                if let Some(tx) = tx.lock().ok().and_then(|mut g| g.take()) {
                    let _ = tx.send(Vec::new());
                }
                return;
            };
            // SAFETY：闭包跑在主线程上（run_on_main_thread 保证），
            // defaultDataStore 要求主线程标记；getAllCookies 只注册
            // 回调、立即返回，不阻塞主线程、不转 run loop。
            let store = unsafe { WKWebsiteDataStore::defaultDataStore(mtm) };
            let cookie_store = unsafe { store.httpCookieStore() };
            let block = block2::RcBlock::new(
                move |cookies: NonNull<NSArray<NSHTTPCookie>>| {
                    let list = unsafe { cookies.as_ref() }
                        .to_vec()
                        .into_iter()
                        .filter_map(|c| raw_cookie(&c, &target_host, &target_scheme))
                        .collect::<Vec<_>>();
                    if let Some(tx) = tx.lock().ok().and_then(|mut g| g.take()) {
                        let _ = tx.send(list);
                    }
                },
            );
            unsafe { cookie_store.getAllCookies(&block) };
        })
        .is_ok();
    if !posted {
        return Vec::new();
    }
    match tokio::time::timeout(std::time::Duration::from_secs(2), rx).await {
        Ok(Ok(list)) => list,
        _ => Vec::new(),
    }
}

/// 从 WebHome 窗口的 Cookie 存储里读 `url` 的 Cookie（v0.0.86 桥与
/// v0.0.87 加宽桥共用）。读不到时返回空列表——调用方照样记 `0 个`
/// （桥跑了、窗口里没有该 URL 的 Cookie），与桥没跑是两回事。
/// iOS（v0.0.91）：走异步 `read_webview_cookies_async`，同步读会在主线
/// 程上转嵌套 run loop、重入 tao 的事件循环而 panic。
#[cfg(target_os = "ios")]
async fn read_webview_cookies(window: &WebviewWindow, url: &Url) -> Vec<hghub_hpp::WebViewCookie> {
    read_webview_cookies_async(window, url)
        .await
        .into_iter()
        .map(|c| hghub_hpp::WebViewCookie {
            name: c.name,
            value: c.value,
            // 与旧转换一致：去前导点、小写；host_only 恒为 false
            //（wry 的转换里 domain 一定有值，见 RawWebViewCookie）。
            domain: c.domain.trim_start_matches('.').to_ascii_lowercase(),
            path: c.path,
            host_only: false,
            secure: c.secure,
            http_only: c.http_only,
            expires_at: c.expires_at,
        })
        .collect()
}

/// 从 WebHome 窗口的 Cookie 存储里读 `url` 的 Cookie（v0.0.86 桥与
/// v0.0.87 加宽桥共用）：`cookies_for_url` 是阻塞调用（等主线程回
/// 消息），放 spawn_blocking 里。读不到时返回空列表——调用方照样记
/// `0 个`（桥跑了、窗口里没有该 URL 的 Cookie），与桥没跑是两回事。
/// 非 iOS 沿用这条路（tao 的 InUserCallback 重入 panic 只在 iOS 上）。
#[cfg(not(target_os = "ios"))]
async fn read_webview_cookies(window: &WebviewWindow, url: &Url) -> Vec<hghub_hpp::WebViewCookie> {
    let w = window.clone();
    let u = url.clone();
    let cookies = tokio::task::spawn_blocking(move || w.cookies_for_url(u))
        .await
        .ok()
        .and_then(|r| r.ok())
        .unwrap_or_default();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let host = url.host_str().unwrap_or("").to_string();
    cookies
        .into_iter()
        .map(|c| {
            let expires_at = c
                .max_age()
                .map(|age| now.saturating_add(age.whole_milliseconds() as u64))
                .or_else(|| {
                    c.expires_datetime()
                        .map(|t| (t.unix_timestamp().max(0) as u64) * 1000)
                });
            hghub_hpp::WebViewCookie {
                name: c.name().to_string(),
                value: c.value().to_string(),
                domain: c
                    .domain()
                    .map(|d| d.trim_start_matches('.').to_ascii_lowercase())
                    .unwrap_or_else(|| host.clone()),
                path: c.path().unwrap_or("/").to_string(),
                host_only: c.domain().is_none(),
                secure: c.secure().unwrap_or(false),
                http_only: c.http_only().unwrap_or(false),
                expires_at,
            }
        })
        .collect()
}

/// WebView→引擎 Cookie 桥（v0.0.86）：发起 WebHome 引擎请求前，把
/// WebHome 窗口里目标 URL 的 Cookie 读出来合并进该插件的 Rust 罐。
/// 只做 WebView→引擎一个方向：引擎自己的 Set-Cookie 照常进罐，不
/// 回写 WebView。窗口不存在、读不到 Cookie 时返回 0、不失败请求——
/// 桥只是给引擎补它没见过的会话 Cookie，罐子本身仍是权威；
/// `__cf_bm` 与过期 Cookie 不进罐（hpp 的 `merge_` 侧按与
/// `set_clearance` / `receive_cookies` 同一条规矩过滤）。
async fn bridge_webview_cookies(
    app: &AppHandle,
    manager: &Arc<PluginManager>,
    plugin_id: &str,
    target: &Url,
) -> usize {
    let Some(window) = app.get_webview_window(LABEL) else {
        return 0;
    };
    // 空列表也往下传：hpp 侧照样写一行 `Cookie 桥接：0 个`——桥跑了、
    // 窗口里没有该 URL 的 Cookie，与桥没跑是两回事，真机验收靠这行区分。
    let bridged = read_webview_cookies(&window, target).await;
    manager
        .bridge_webview_cookies(plugin_id, bridged)
        .await
        .unwrap_or(0)
}

/// 加宽桥（v0.0.87）：首页的 Cookie 也读出来，记到“发往目标 origin
/// 的当次附加”里——WebView 只加载 homePage，从不访问目标域名，
/// v0.0.86 的桥按目标 URL 读永远是 0 个（真机每次都是 0 个）；首页落
/// 的会话 Cookie 是唯一剩下的来源，源站只看 Cookie 头里的名和值。
/// 只附当次请求，不进罐子不落盘（hpp 侧 `set_home_cookies`）。首页
/// 与目标同 origin 时跳过（目标桥已经处理了）。窗口不存在、首页地址
/// 读不出来时返回 0、不记日志——调用方静默回落。
async fn bridge_home_cookies(
    app: &AppHandle,
    manager: &Arc<PluginManager>,
    plugin_id: &str,
    target: &Url,
) -> usize {
    let Some(window) = app.get_webview_window(LABEL) else {
        return 0;
    };
    let Ok(home_page) = hghub_hpp::media::MediaSessions::webhome_page_url(manager, plugin_id).await
    else {
        return 0;
    };
    let Ok(home) = home_page.parse::<Url>() else {
        return 0;
    };
    if home.origin() == target.origin() {
        return 0;
    }
    let bridged = read_webview_cookies(&window, &home).await;
    let source_host = home.host_str().unwrap_or("").to_string();
    manager
        .bridge_home_cookies(
            plugin_id,
            &target.origin().ascii_serialization(),
            &source_host,
            bridged,
        )
        .await
        .unwrap_or(0)
}

/// 只对 net.request（`req` 别名同）做桥：其他方法不发任意 URL 请求。
async fn maybe_bridge_webview_cookies(
    app: &AppHandle,
    manager: &Arc<PluginManager>,
    plugin_id: &str,
    method: &str,
    payload: &Value,
) {
    if !matches!(method, "req" | "net.request") {
        return;
    }
    if let Some(url) = payload["url"].as_str() {
        if let Ok(target) = url.parse::<Url>() {
            bridge_webview_cookies(app, manager, plugin_id, &target).await;
            // v0.0.87 加宽桥：首页的 Cookie 也记下来，供当次请求附上。
            bridge_home_cookies(app, manager, plugin_id, &target).await;
        }
    }
}

/// 分发一次 WebHome 调用；命中 Cloudflare 拦截时走一趟 WebView
/// clearance 再重发一次（最多一趟，不循环）。触发条件只有两个——
/// 拿到拦截页，或 TLS 握手被断开——这两种失败都说明请求根本没执行
/// 到上游，重发是安全的；其他失败原样返回。只对 net.request（`req`
/// 别名同）做，其他方法不触发（它们不走 Node 发任意 URL）。
async fn dispatch_with_clearance(
    app: &AppHandle,
    manager: &Arc<PluginManager>,
    state: &WebHomeState,
    plugin_id: &str,
    method: &str,
    payload: Value,
    player_state: Value,
) -> Result<Outcome, String> {
    maybe_bridge_webview_cookies(app, manager, plugin_id, method, &payload).await;
    let first = webhome::dispatch(
        manager,
        plugin_id,
        method,
        payload.clone(),
        player_state.clone(),
    )
    .await;
    let trigger = match &first {
        Ok(Outcome::Value(v)) => {
            matches!(method, "req" | "net.request")
                && v["data"]
                    .as_str()
                    .is_some_and(hghub_hpp::looks_like_challenge)
        }
        Err(e) => {
            e.code == "NETWORK"
                && e.message
                    .contains("before secure TLS connection was established")
        }
        _ => false,
    };
    if !trigger {
        return first.map_err(err);
    }
    // 拦截页是 HTTP 200 没有宿主错误：目标 URL 从返回体里的最终地址
    // 取；TLS 失败没有返回体，从请求 payload 取。
    let url = match &first {
        Ok(Outcome::Value(v)) => v["url"].as_str().unwrap_or(""),
        _ => payload["url"].as_str().unwrap_or(""),
    };
    if url.is_empty() {
        return first.map_err(err);
    }
    let _guard = state.clearance_lock.lock().await;
    match clearance::obtain(app, manager, plugin_id, url).await {
        // 通行证拿到了、页面还在：重发一次，结果原样返回（再被拦也
        // 不再触发 clearance——一趟只做一次）。重发前再桥一次：
        // clearance 导航可能在 WebView 里落了新的站点 Cookie。
        Clearance::Ready => {
            maybe_bridge_webview_cookies(app, manager, plugin_id, method, &payload).await;
            webhome::dispatch(manager, plugin_id, method, payload, player_state)
                .await
                .map_err(err)
        }
        // 没拿到，或页面已被挑战页顶离（已导航回首页）：原结果原样
        // 返回，不掩盖原始报错；用户再点一次即带通行证。
        _ => first.map_err(err),
    }
}

/// 主窗口推送直链播放状态（播放/暂停/进度），供页面 player.status 读取。
#[tauri::command]
pub fn webhome_player_state(
    state: State<'_, WebHomeState>,
    playing: bool,
    position_sec: f64,
    duration_sec: f64,
) -> Result<(), String> {
    *state.player_state.lock().map_err(err)? = json!({
        "playing": playing,
        "positionSec": position_sec,
        "durationSec": duration_sec,
    });
    Ok(())
}

/// 全 App 缓存清理（v0.0.84；v0.0.83 是“清理 WebHome 全部数据”）：
/// 删页面 localStorage 里的三个域名缓存键、全清 WebView 浏览数据、
/// 清全部插件的 Rust 请求 Cookie 罐与响应缓存、`webhome:` 前缀的存储键、
/// 全部会话的媒体预加载缓存、插件目录下的 `__pycache__` 与 `*.pyc`、
/// Python 边车已加载蜘蛛的运行时缓存（边车在运行时才清）、插件导入
/// 暂存文件，再重载 webhome 窗口；最后清日志（全部插件的 ext.log
/// 内存缓冲 + App 自己的 debug.log 文件）。窗口不存在时返回
/// `WEBHOME_NOT_OPEN`，不建窗口。中间步骤按类别尽力清理、失败记入
/// warnings；窗口步骤失败时不重载、日志不清（留着查错）。
/// 不碰：App 设置、导入的插件及插件文件、插件存储里非 `webhome:` 的键、
/// 下载好的视频、插件源文件、HTML 源文件。
/// 步骤顺序与报告字段的逻辑在 hpp（`webhome_data`），这里只是窗口的
/// 薄包装 + App 侧的几项（Python 边车、导入暂存、debug.log）。
#[tauri::command]
pub async fn clear_webhome_data(
    app: AppHandle,
    host: State<'_, PluginHost>,
) -> Result<Value, String> {
    // 窗口不存在先报 WEBHOME_NOT_OPEN（刚装好、还没打开过 WebHome 时，
    // 用户要看的是这个）。v0.0.84 不再要求会话绑定插件——罐子是全部
    // 插件一起清，不再按绑定插件挑。
    let Some(window) = app.get_webview_window(LABEL) else {
        return Err(hghub_hpp::webhome_data::WEBHOME_NOT_OPEN.to_string());
    };
    let window = WebHomeWindow(window);
    let manager = host.manager(&app).await.map_err(err)?;
    let middle_manager = Arc::clone(&manager);
    let logs_manager = Arc::clone(&manager);
    let app_for_py = app.clone();
    let app_for_staging = app.clone();
    let report = hghub_hpp::webhome_data::clear_webhome_data(
        Some(&window),
        move || {
            let manager = middle_manager;
            let app_for_py = app_for_py;
            let app_for_staging = app_for_staging;
            async move {
                let mut middle = manager.clear_all_runtime_caches().await;
                // Python 边车运行时缓存：边车在运行时才清；没跑时
                // py_running 为 false、_RUNTIME_CACHE 本就是空的。
                match crate::python::running_client(&app_for_py).await {
                    Some(client) => {
                        middle.py_running = true;
                        let py = client.clear_all_spider_caches().await;
                        middle.py_spiders_total = py.spiders_total;
                        middle.py_spiders_cleared = py.spiders_cleared;
                        middle.warnings.extend(py.warnings);
                    }
                    None => middle.py_running = false,
                }
                // 插件导入暂存文件。
                match crate::plugins::clear_staging_dir(&app_for_staging) {
                    Ok(n) => middle.staging_files = n,
                    Err(e) => middle.warnings.push(format!("插件导入暂存清理失败：{e}")),
                }
                middle
            }
        },
        move || {
            let manager = logs_manager;
            async move { manager.clear_all_logs().await }
        },
    )
    .await?;
    // 最后清 App 自己的 debug.log（落盘文件）：清完日志从空开始，
    // 下次出问题时新日志更干净、更好定位。本次清理动作自身的日志行
    // 也会随之消失，是预期的。
    let debug_log_cleared = app
        .path()
        .app_data_dir()
        .map(|dir| std::fs::remove_file(dir.join("debug.log")).is_ok())
        .unwrap_or(false);
    Ok(json!({
        "cleared": report.cleared,
        "absent": report.absent,
        "jar_cleared": report.jar_cleared,
        "plugins": report.plugins,
        "response_cache_dropped": report.response_cache_dropped,
        "webhome_keys_deleted": report.webhome_keys_deleted,
        "media_entries_dropped": report.media_entries_dropped,
        "pycache_dirs": report.pycache_dirs,
        "pycache_files": report.pycache_files,
        "py_running": report.py_running,
        "py_spiders_total": report.py_spiders_total,
        "py_spiders_cleared": report.py_spiders_cleared,
        "staging_files": report.staging_files,
        "log_entries_dropped": report.log_entries_dropped,
        "debug_log_cleared": debug_log_cleared,
        "warnings": report.warnings,
    }))
}

/// `WebviewWindow` 的本地包装：hpp 的 `WebHomeDataWindow` trait 与
/// `WebviewWindow` 都不在本 crate 里，直接 impl 会撞孤儿规则（E0117），
/// 所以包一层本地类型再实现。方法体调的都是 `WebviewWindow` 自带的
/// inherent 方法（inherent 方法优先于 trait 方法，不会递归到自己）。
struct WebHomeWindow(WebviewWindow);

impl hghub_hpp::webhome_data::WebHomeDataWindow for WebHomeWindow {
    fn eval_js(&self, js: String, callback: Box<dyn Fn(String) + Send>) -> Result<(), String> {
        self.0
            .eval_with_callback(js, callback)
            .map_err(|e| e.to_string())
    }

    fn clear_browsing_data(&self) -> Result<(), String> {
        self.0.clear_all_browsing_data().map_err(|e| e.to_string())
    }

    fn reload_page(&self) -> Result<(), String> {
        self.0.eval("location.reload()").map_err(|e| e.to_string())
    }
}
