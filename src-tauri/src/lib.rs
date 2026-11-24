mod commands;
mod clearance;
mod download;
pub mod fmp4;
mod logging;
mod native_player;
#[cfg(target_os = "macos")]
mod macos_titlebar;
mod network_policy;
mod node;
mod plugins;
mod python;
mod webhome;
#[cfg(target_os = "windows")]
mod windows_login;
#[cfg(target_os = "android")]
mod android_login;
#[cfg(target_os = "android")]
mod android_documents;
use tauri::Manager;

/// Swift 侧的插件入口（src-tauri/ios/OrientationPlugin.swift 里的
/// `@_cdecl("init_plugin_orientation")`）。只在 iOS 编译；与 native_player.rs
/// 的 init_plugin_native_player 同一写法，绑定只在本模块可见，orientation
/// 插件的 setup 闭包也在本文件内引用它。
#[cfg(target_os = "ios")]
tauri::ios_plugin_binding!(init_plugin_orientation);
/// Swift 侧的分享插件入口（src-tauri/ios/Sources/SharePlugin.swift 里的
/// `@_cdecl("init_plugin_share")`），只在 iOS 编译；与 orientation 同一写法。
#[cfg(target_os = "ios")]
tauri::ios_plugin_binding!(init_plugin_share);
/// 启动时装 panic hook：release 是 `panic = "abort"`（Cargo.toml），
/// 任何 Rust panic 都会直接 abort 进程——不 unwind、不返回错误、
/// 不写 debug.log。hook 在 abort 之前执行，把 message + 位置写进
/// debug.log（绕开 DEBUG_WRITER 锁，见 logging::write_panic_record），
/// 再把原默认 hook 也跑一遍（stderr 输出保留）。
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "panic（消息不是字符串）".to_string()
        };
        let loc = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "未知位置".to_string());
        // 截断到 500 字，避免超大消息把日志撑爆。
        let msg: String = msg.chars().take(500).collect();
        crate::logging::write_panic_record(&format!("[panic] {msg}（{loc}）"));
        default_hook(info);
    }));
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    install_panic_hook();
    let builder = tauri::Builder::default();
    #[cfg(target_os = "android")]
    let builder = builder.plugin(tauri::plugin::Builder::<tauri::Wry, ()>::new("web-login")
        .setup(|app, api| {
            let handle = api.register_android_plugin("com.hghub.player", "LoginWebViewPlugin")?;
            app.manage(android_login::AndroidLogin(handle));
            Ok(())
        }).build());
    #[cfg(target_os = "android")]
    let builder = builder.plugin(tauri::plugin::Builder::<tauri::Wry, ()>::new("plugin-documents")
        .setup(|app, api| {
            let handle = api.register_android_plugin("com.hghub.player", "PluginDocumentsPlugin")?;
            app.manage(android_documents::AndroidDocuments(handle));
            Ok(())
        }).build());
    builder
        .plugin(native_player::plugin())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(
            tauri::plugin::Builder::<tauri::Wry, ()>::new("orientation")
                .setup(|app, api| {
                    #[cfg(target_os = "android")]
                    {
                        let handle = api.register_android_plugin("com.hghub.player", "OrientationPlugin")?;
                        app.manage(commands::OrientationPlugin(handle));
                    }
                    #[cfg(target_os = "ios")]
                    {
                        let handle = api.register_ios_plugin(init_plugin_orientation)?;
                        app.manage(commands::OrientationPlugin(handle));
                    }
                    #[cfg(not(any(target_os = "android", target_os = "ios")))]
                    let _ = (app, api);
                    Ok(())
                })
                .build(),
        )
        .plugin(
            tauri::plugin::Builder::<tauri::Wry, ()>::new("share")
                .setup(|app, api| {
                    #[cfg(target_os = "ios")]
                    {
                        let handle = api.register_ios_plugin(init_plugin_share)?;
                        app.manage(commands::SharePlugin(handle));
                    }
                    #[cfg(not(target_os = "ios"))]
                    let _ = (app, api);
                    Ok(())
                })
                .build(),
        )
        .plugin(
            tauri_plugin_opener::Builder::new()
                .open_js_links_on_click(false)
                .build(),
        )
        .setup(|app| {
            let dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&dir)?;
            crate::logging::set_panic_log_dir(&dir);
            network_policy::bind_security(&dir);
            hghub_hpp::proxy::bind(&dir);
            app.manage(plugins::PluginHost::new(dir.join("hpp")));
            app.manage(webhome::WebHomeState::default());
            // The WebView registers its playback capabilities before the first
            // plugin command; eager initialization would expose an incorrect env.
            #[cfg(target_os = "macos")]
            macos_titlebar::create_main_window(app)?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            plugins::hpp_manage,
            plugins::pick_plugin_files,
            plugins::pick_plugin_directory,
            commands::app_boot,
            commands::security_set,
            commands::set_orientation,
            commands::set_status_bar_hidden,
            commands::adjust_brightness,
            commands::adjust_volume,
            commands::share_file,
            commands::open_senplayer,
            commands::export_to_photos,
            download::download_media,
            download::list_downloads,
            download::delete_download,
            download::serve_download,
            commands::debug_log,
            commands::read_debug_log,
            commands::open_url,
            commands::open_external_media,
            webhome::open_webhome,
            webhome::webhome_close,
            webhome::webhome_call,
            webhome::webhome_player_state,
            webhome::clear_webhome_data,
            native_player::native_player_open,
            native_player::native_player_close,
            native_player::native_player_play,
            native_player::native_player_pause,
            native_player::native_player_seek,
            native_player::native_player_set_rate,
            native_player::native_player_set_volume,
            native_player::native_player_state
        ])
        .run(tauri::generate_context!())
        .expect("error while running HGhub");
}
