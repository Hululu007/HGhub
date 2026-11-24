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

/// 详情页左右竖滑调亮度/音量（仅 iOS）：前端按拖动增量调，返回调完后的新值
/// （0..1）供指示条显示。亮度走 UIScreen.main.brightness（下限 0.05）；
/// 音量走系统音量（MPVolumeView 滑块）。Android 的同名插件没有这两个方法，
/// 前端只在 iOS 上调用。
#[derive(serde::Deserialize)]
struct LevelResult {
    value: f64,
}

#[cfg(any(target_os = "android", target_os = "ios"))]
#[tauri::command]
pub async fn adjust_brightness(delta: f64, plugin: tauri::State<'_, OrientationPlugin>) -> Result<f64, String> {
    let result: LevelResult = plugin
        .0
        .run_mobile_plugin_async("adjustBrightness", serde_json::json!({"delta": delta}))
        .await
        .map_err(|error| error.to_string())?;
    Ok(result.value)
}

#[cfg(any(target_os = "android", target_os = "ios"))]
#[tauri::command]
pub async fn adjust_volume(delta: f64, plugin: tauri::State<'_, OrientationPlugin>) -> Result<f64, String> {
    let result: LevelResult = plugin
        .0
        .run_mobile_plugin_async("adjustVolume", serde_json::json!({"delta": delta}))
        .await
        .map_err(|error| error.to_string())?;
    Ok(result.value)
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
#[tauri::command]
pub fn adjust_brightness(delta: f64) -> Result<f64, String> {
    let _ = delta;
    Err("当前平台不支持原生亮度调节".into())
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
#[tauri::command]
pub fn adjust_volume(delta: f64) -> Result<f64, String> {
    let _ = delta;
    Err("当前平台不支持原生音量调节".into())
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

/// 读 App 自己的 debug.log（v0.0.89 诊断版，供设置页「运行日志」查看）：
/// 返回最后 N 行（默认 200，上限 1000），总量封顶约 50KB；文件不存在
/// 返回空串。只读，不删不改。
#[tauri::command]
pub fn read_debug_log(app: tauri::AppHandle, lines: Option<u32>) -> Result<String, String> {
    let path = app
        .path()
        .app_data_dir()
        .map_err(|e| e.to_string())?
        .join("debug.log");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(e) => return Err(e.to_string()),
    };
    let keep = lines.unwrap_or(200).clamp(1, 1000) as usize;
    const MAX_BYTES: usize = 50 * 1024;
    let mut kept: Vec<&str> = text.lines().rev().take(keep).collect();
    kept.reverse();
    let mut out = kept.join("\n");
    if out.len() > MAX_BYTES {
        let mut start = out.len() - MAX_BYTES;
        if let Some(pos) = out[start..].find('\n') {
            start += pos + 1;
        }
        while start < out.len() && !out.is_char_boundary(start) {
            start += 1;
        }
        out = out[start..].to_string();
    }
    Ok(out)
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

/// 分享已下载的文件：iOS 弹系统分享面（可存到“文件”、发到微信等）；
/// 其他平台用默认应用打开该文件。
#[cfg(target_os = "ios")]
pub struct SharePlugin(pub tauri::plugin::PluginHandle<tauri::Wry>);

#[cfg(target_os = "ios")]
#[tauri::command]
pub async fn share_file(path: String, plugin: tauri::State<'_, SharePlugin>) -> Result<(), String> {
    plugin.0.run_mobile_plugin_async::<()>("shareFile", serde_json::json!({"path": path})).await
        .map_err(|error| error.to_string())
}

/// 跳转 SenPlayer 播放：只接受 senplayer:// 地址（前端按公开 Scheme
/// senplayer://x-callback-url/play?url=…&name=… 拼好传进来），iOS 经
/// SharePlugin.openSenPlayer 用 canOpenURL 判是否装了 SenPlayer，未装
/// 时明确报错，不静默无反应。open_external_media 走不了这条路——它
/// 只放行 http/https 等协议，senplayer:// 会被拦下。
/// 回环播放地址须播着才不断流，见 src/senplayer.ts。
#[cfg(target_os = "ios")]
#[tauri::command]
pub async fn open_senplayer(url: String, plugin: tauri::State<'_, SharePlugin>) -> Result<(), String> {
    if !url.starts_with("senplayer://") {
        return Err("不是 SenPlayer 地址".into());
    }
    plugin.0.run_mobile_plugin_async::<()>("openSenPlayer", serde_json::json!({"url": url})).await
        .map_err(|error| error.to_string())
}

#[cfg(not(target_os = "ios"))]
#[tauri::command]
pub fn open_senplayer(url: String) -> Result<(), String> {
    let _ = url;
    Err("SenPlayer 跳转仅在 iOS App 内可用".into())
}

/// 导出已下载的文件到相册：iOS 经 SharePlugin.saveToPhotos 用
/// PHPhotoLibrary 直接存视频；其他平台不支持。
#[cfg(target_os = "ios")]
#[tauri::command]
pub async fn export_to_photos(path: String, plugin: tauri::State<'_, SharePlugin>) -> Result<(), String> {
    plugin.0.run_mobile_plugin_async::<()>("saveToPhotos", serde_json::json!({"path": path})).await
        .map_err(|error| error.to_string())
}

#[cfg(not(target_os = "ios"))]
#[tauri::command]
pub fn export_to_photos(path: String) -> Result<(), String> {
    let _ = path;
    Err("保存到相册仅在 iOS App 内可用".into())
}

#[cfg(not(target_os = "ios"))]
#[tauri::command]
pub fn share_file(app: tauri::AppHandle, path: String) -> Result<(), String> {
    app.opener().open_path(path, None::<&str>).map_err(|e| e.to_string())
}
