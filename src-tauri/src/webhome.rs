//! WebHome 宿主面（08 §10）：为声明了 meta.webhome 的插件开一个独立 WebView
//! 窗口，加载其首页并注入 window.fm 桥（见 webhome_sdk.js）。窗口一次只开
//! 一个，绑定一个插件；页面只能操作绑定它的那个插件。
use crate::plugins::PluginHost;
use hghub_hpp::webhome::{self, Outcome};
use serde_json::{json, Value};
use std::sync::Mutex;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder, WindowEvent};

const LABEL: &str = "webhome";
const SDK: &str = include_str!("webhome_sdk.js");

pub struct WebHomeState {
    session: Mutex<Option<WebHomeSession>>,
    /// 主窗口推送的直链播放状态，供 player.status 读取。
    player_state: Mutex<Value>,
    /// open_webhome 串行化：窗口 close() 是异步的，两次打开并发会
    /// 在旧窗还没从窗口表注销时建新窗，撞上 label 已存在。
    open_lock: tokio::sync::Mutex<()>,
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
    // 整个打开过程串行化：第二下必须等第一下建完窗、写完 session
    // 再看窗口与 session，否则两下都以为窗口不存在或对不上。
    let _open_guard = state.open_lock.lock().await;
    let manager = host.manager(&app).await.map_err(err)?;
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
            window.set_focus().map_err(err)?;
            return Ok(json!({ "url": home_page }));
        }
        window.close().map_err(err)?;
        // close() 只是发了关闭请求：窗口要等主线程跑完才从窗口表
        // 里注销，立刻建同 label 的新窗会报 already exists；等它
        // 消失再建，最多等约 2 秒。
        for _ in 0..100 {
            if app.get_webview_window(LABEL).is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    let window = match WebviewWindowBuilder::new(&app, LABEL, WebviewUrl::External(home_page.parse().map_err(err)?))
        .title(meta.name.clone())
        .initialization_script(SDK)
        .build()
    {
        Ok(window) => window,
        Err(e) if e.to_string().contains("already exists") => {
            // 旧窗注销后仍撞 label：窗口开着就是用户要的结果，聚
            // 焦它并返回成功，不把报错抛给前端。
            if let Some(window) = app.get_webview_window(LABEL) {
                window.set_focus().map_err(err)?;
                return Ok(json!({ "url": home_page }));
            }
            return Err(err(e));
        }
        Err(e) => return Err(err(e)),
    };
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
pub async fn webhome_close(
    app: AppHandle,
    state: State<'_, WebHomeState>,
) -> Result<(), String> {
    if let Some(window) = app.get_webview_window(LABEL) {
        window.close().map_err(err)?;
    }
    *state.session.lock().map_err(err)? = None;
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
    let outcome = webhome::dispatch(&manager, &plugin_id, &method, payload, player_state)
        .await
        .map_err(err)?;
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
