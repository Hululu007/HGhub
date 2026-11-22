use serde_json::{json, Value};
use tauri::Manager;
use tauri_plugin_opener::OpenerExt;

#[cfg(any(target_os = "android", target_os = "ios"))]
pub struct OrientationPlugin(pub tauri::plugin::PluginHandle<tauri::Wry>);

#[cfg(any(target_os = "android", target_os = "ios"))]
#[tauri::command]
pub async fn set_orientation(landscape: bool, plugin: tauri::State<'_, OrientationPlugin>) -> Result<(), String> {
    plugin.0.run_mobile_plugin_async::<()>("setOrientation", serde_json::json!({"landscape": landscape})).await
        .map_err(|error| error.to_string())
}

/// 横屏播放时隐藏 iOS 状态栏（时间、电量、网速等不再压在视频上方），回竖屏
/// 或离开播放页时再显示。iOS 上调 OrientationPlugin.setStatusBarHidden；
/// Android 的同名插件没有这个方法，调了会返回错误，前端只在 iOS 上调用。
#[cfg(any(target_os = "android", target_os = "ios"))]
#[tauri::command]
pub async fn set_status_bar_hidden(hidden: bool, plugin: tauri::State<'_, OrientationPlugin>) -> Result<(), String> {
    plugin.0.run_mobile_plugin_async::<()>("setStatusBarHidden", serde_json::json!({"hidden": hidden})).await
        .map_err(|error| error.to_string())
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
#[tauri::command]
pub fn set_status_bar_hidden(hidden: bool) -> Result<(), String> {
    let _ = hidden;
    Err("当前平台不支持原生状态栏切换".into())
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
#[tauri::command]
pub fn set_orientation(landscape: bool) -> Result<(), String> {
    let _ = landscape;
    Err("当前平台不支持原生屏幕方向切换".into())
}

#[tauri::command]
pub fn app_boot() -> Value {
    json!({"strict_security":crate::network_policy::strict_security()})
}
#[tauri::command]
pub fn security_set(strict: bool) -> Result<bool, String> {
    crate::network_policy::set_strict_security(strict)?;
    Ok(crate::network_policy::strict_security())
}
#[tauri::command]
pub fn debug_log(app: tauri::AppHandle, msg: String) -> Result<(), String> {
    crate::logging::append_debug(&app.path().app_data_dir().map_err(|e| e.to_string())?, &msg)
}
#[tauri::command]
pub fn open_url(app: tauri::AppHandle, url: String) -> Result<(), String> {
    let target = crate::network_policy::parse_target(&url)?;
    app.opener()
        .open_url(target.as_str(), None::<&str>)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn open_external_media(app: tauri::AppHandle, url: String) -> Result<(), String> {
    hghub_hpp::model::url(&url, true, true).map_err(|e| e.message)?;
    app.opener().open_url(url, None::<&str>).map_err(|e| e.to_string())
}
