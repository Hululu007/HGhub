//! 应用内浏览器窗口：网盘分享链 / go.php 外链在 App 内打开，不走系统
//! 浏览器。窗口只做浏览：不注入 WebHome 的 SDK、不接插件桥、不走引擎
//! 代理，网页由平台原生网络栈加载（与登录视图同理）。
//!
//! 复用 WebHome 的 hide-and-reuse：关闭只是 hide，不销毁——iOS 上窗口
//! 销毁后从窗口表注销的时机由系统把握，关了重建会撞 label 已存在；
//! 再次打开同一/新链接时，已有窗口导航到新 URL 并显示。

use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_opener::OpenerExt;
use url::Url;

const LABEL: &str = "browser";

/// 注入的极简外框：顶部一条浮动栏，显示当前域名 + 一个 × 关闭按钮。
/// 初始化脚本在每次页面加载开始时都跑，域名随导航自动更新；× 调
/// browser_close 命令把窗口藏起来。不是 WebHome 的 SDK，不接桥。
/// 浮动栏落在状态栏下方（top 取 safe-area），并把页面顶端下推同等高度，
/// 否则刘海机上页面自己的顶部会被状态栏和浮动栏盖住、显示不全。
const CHROME: &str = r#"(function(){
  if (window.__hgBrowserChrome) return; window.__hgBrowserChrome = true;
  function bar(){
    var old = document.getElementById('hg-browser-bar'); if (old) old.remove();
    var b = document.createElement('div'); b.id = 'hg-browser-bar';
    b.style.cssText = 'position:fixed;top:env(safe-area-inset-top, 0px);left:0;right:0;height:36px;z-index:2147483647;display:flex;align-items:center;gap:8px;padding:0 8px;background:rgba(20,20,24,.92);color:#fff;font:13px/1 -apple-system,BlinkMacSystemFont,sans-serif;';
    var t = document.createElement('span');
    t.textContent = location.hostname || location.href;
    t.style.cssText = 'flex:1;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;';
    var x = document.createElement('button'); x.type = 'button'; x.textContent = '✕';
    x.setAttribute('aria-label', '关闭');
    x.style.cssText = 'flex:none;width:28px;height:28px;border:none;border-radius:50%;background:rgba(255,255,255,.16);color:#fff;font-size:14px;line-height:1;cursor:pointer;';
    x.onclick = function(){ try { var c = window.__TAURI__ && window.__TAURI__.core; if (c && c.invoke) c.invoke('browser_close', {}); } catch (e) {} };
    b.appendChild(t); b.appendChild(x);
    document.documentElement.style.paddingTop = 'calc(env(safe-area-inset-top, 0px) + 36px)';
    document.documentElement.appendChild(b);
  }
  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', bar); else bar();
})();"#;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn debug_log(app: &AppHandle, msg: &str) {
    if let Ok(dir) = app.path().app_data_dir() {
        let _ = crate::logging::append_debug(&dir, msg);
    }
}

/// 在应用内浏览器窗口打开一个 URL。窗口已存在就导航到新 URL 并显示；
/// 不存在才建新窗。http(s) 留在窗内；页面里点“用 App 打开”这类非
/// http(s) 深链时 WKWebView 打不开，交给系统处理（网盘 App 接手）。
pub fn open_browser(app: &AppHandle, url: &str) -> Result<(), String> {
    let parsed: Url = url.parse().map_err(err)?;
    if let Some(window) = app.get_webview_window(LABEL) {
        window.navigate(parsed).map_err(err)?;
        window.show().map_err(err)?;
        window.set_focus().map_err(err)?;
        debug_log(app, "[browser] 窗口导航并显示");
        return Ok(());
    }
    let nav_app = app.clone();
    let window = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::External(parsed))
        .title("浏览器")
        .initialization_script(CHROME)
        .on_navigation(move |url| {
            let scheme = url.scheme();
            if scheme == "http" || scheme == "https" {
                return true;
            }
            // 非 http(s) 深链：交给系统处理，窗口内不导航。
            let _ = nav_app.opener().open_url(url.as_str(), None::<&str>);
            false
        })
        .build()
        .map_err(err)?;
    window.set_focus().map_err(err)?;
    debug_log(app, "[browser] 窗口新建");
    Ok(())
}

/// × 只是隐藏窗口，不销毁（与 webhome_close 同款 hide-and-reuse）。
#[tauri::command]
pub async fn browser_close(app: AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window(LABEL) {
        window.hide().map_err(err)?;
        debug_log(&app, "[browser] 窗口隐藏");
    }
    Ok(())
}

/// 主窗口点按 URL 型分类时在应用内浏览器打开（如猫源配置中心的
/// /website 页）。只接受 http(s)；open_browser 自己会解析校验 URL。
/// 与 commands::open_url 的区别：那个走系统浏览器，这个留在 App 内。
#[tauri::command]
pub async fn browser_open(app: AppHandle, url: String) -> Result<(), String> {
    open_browser(&app, &url)
}
