//! WebHome 宿主面（08 §10）：为声明了 meta.webhome 的插件开一个独立 WebView
//! 窗口，加载其首页并注入 window.fm 桥（见 webhome_sdk.js）。窗口一次只开
//! 一个，绑定一个插件；页面只能操作绑定它的那个插件。窗口常驻不销毁：
//! × 只是隐藏，重开直接显示，换源时导航——iOS 上窗口销毁后从窗口表注销
//! 的时机由系统把握，关了重建会撞 label 已存在。
use crate::{
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
            return Ok(json!({ "url": home_page }));
        }
        // 换源：让活着的 webview 导航到新首页，桥在导航后重新注入
        //（wry 把初始化脚本挂成 AtDocumentStart 的 WKUserScript，每
        // 次加载都跑），再把 session 更新成新插件。
        window
            .navigate(home_page.parse().map_err(err)?)
            .map_err(err)?;
        *state.session.lock().map_err(err)? = Some(WebHomeSession { plugin_id });
        window.show().map_err(err)?;
        window.set_focus().map_err(err)?;
        return Ok(json!({ "url": home_page }));
    }
    // 窗口不存在才建新窗。建窗仍撞 label 说明有窗口存在但前面没认
    // 出来，聚焦它只会重现“打开了却什么都没出现”的静默失败；直接报出来。
    let window = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::External(home_page.parse().map_err(err)?))
        .title(meta.name.clone())
        .initialization_script(SDK)
        .build()
        .map_err(err)?;
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
    *state.session.lock().map_err(err)? = Some(WebHomeSession { plugin_id });
    Ok(json!({ "url": home_page }))
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
    }
    Ok(())
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
    let manager = host.manager(&app).await.map_err(err)?;
    let player_state = state.player_state.lock().map_err(err)?.clone();
    let outcome = dispatch_with_clearance(
        &app,
        &manager,
        &state,
        &plugin_id,
        &method,
        payload,
        player_state,
    )
    .await?;
    let main = app.get_webview_window("main");
    match outcome {
        Outcome::Value(value) => Ok(value),
        Outcome::Play { media, title } => {
            if let Some(main) = main {
                main.emit("webhome-play", json!({ "media": media, "title": title }))
                    .map_err(err)?;
            }
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
            Ok(json!({ "code": 0 }))
        }
        Outcome::OpenKeep => {
            if let Some(main) = main {
                main.emit("webhome-open-keep", json!({ "pluginId": plugin_id }))
                    .map_err(err)?;
            }
            Ok(json!({ "code": 0 }))
        }
        Outcome::OpenSetting => {
            if let Some(main) = main {
                main.emit("webhome-open-setting", json!({ "pluginId": plugin_id }))
                    .map_err(err)?;
            }
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
        // 不再触发 clearance——一趟只做一次）。
        Clearance::Ready => webhome::dispatch(manager, plugin_id, method, payload, player_state)
            .await
            .map_err(err),
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

/// 清理 WebHome 全部数据（v0.0.83）：删页面 localStorage 里的三个域名
/// 缓存键、全清 WebView 浏览数据、清绑定插件的 Rust 请求 Cookie 罐，
/// 再重载 webhome 窗口。窗口不存在时返回 `WEBHOME_NOT_OPEN`，不建
/// 窗口；会话未绑定插件时也不清（不知道清哪个插件的罐子）。删键、
/// 取值与步骤顺序的逻辑在 hpp（`webhome_data`），这里只是窗口的薄包装。
#[tauri::command]
pub async fn clear_webhome_data(
    app: AppHandle,
    host: State<'_, PluginHost>,
    state: State<'_, WebHomeState>,
) -> Result<Value, String> {
    // 窗口不存在先报 WEBHOME_NOT_OPEN（比会话未绑定更优先：刚装好、
    // 还没打开过 WebHome 时两者都没有，用户要看的是前者）。
    let Some(window) = app.get_webview_window(LABEL) else {
        return Err(hghub_hpp::webhome_data::WEBHOME_NOT_OPEN.to_string());
    };
    let window = WebHomeWindow(window);
    let plugin_id = state
        .session
        .lock()
        .map_err(err)?
        .as_ref()
        .map(|s| s.plugin_id.clone())
        .ok_or_else(|| "WebHome 会话未绑定插件".to_string())?;
    let manager = host.manager(&app).await.map_err(err)?;
    let report = hghub_hpp::webhome_data::clear_webhome_data(Some(&window), || async {
        manager
            .clear_cookie_jar(&plugin_id)
            .await
            .map_err(|e| e.to_string())
    })
    .await?;
    Ok(json!({
        "cleared": report.cleared,
        "absent": report.absent,
        "jar_cleared": report.jar_cleared,
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
