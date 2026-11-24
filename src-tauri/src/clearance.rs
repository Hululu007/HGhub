//! WebHome WebView clearance（v0.0.77，见 docs/developers/node-engine.md §10.3 ④）：
//! Node 请求被 Cloudflare 拦截（拦截页 / TLS 握手被断）时，用真浏览器
//! （WebHome 窗口自己的 WKWebView）取同一 URL，让挑战在浏览器里跑完，
//! 拿到 `cf_clearance` 后重发 Node 请求。只读 `cf_clearance`，`__cf_bm`
//! 绝不进 Rust 侧（v0.0.75 的教训）。
//!
//! 机制：先在首页里挂一个隐藏 iframe 导航到目标 URL（页面本身不导航，
//! 待回的 fm.req 还活着）；挑战跑完、Cookie 落进窗口的 Cookie 存储后，
//! 用 Tauri 2.12.5 自带的 `WebviewWindow::cookies_for_url` 读出来——
//! vendored wry 的 `WKWebView::cookies()` 本就存在，Tauri 已把它包成
//! 公开方法，不用自己接 wry。若 15 秒内没拿到（例如挑战页禁 iframe），
//! 再把窗口本身导航到目标 URL 跑一遍（顶层导航没有 frame 限制），拿到
//! 后导航回首页。整个过程最多 30 秒；拿不到就如实返回失败，不掩盖原
//! 始报错。`fetch` 不能替这件事：它不执行挑战页的 JS，拿不到通行证。

use hghub_hpp::{manager::PluginManager, ClearanceCookie};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Manager, WebviewWindow};
use url::Url;

const FRAME_ID: &str = "__hg_clearance_frame";
/// iframe 阶段与顶层导航阶段各最多 15 秒（挑战通常几秒内自动通过；
/// 需要人点验证框的挑战等再久也不会自己通过）。
const PHASE_TIMEOUT: Duration = Duration::from_secs(15);
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// 取通行证的结果。
pub enum Clearance {
    /// 拿到了，页面还在首页，可以重发 Node 请求。
    Ready,
    /// 拿到了（或没拿到），但窗口已被挑战页顶离首页，已导航回首页；
    /// 原页面已死，不要重发——用户再点一次即带通行证。
    PageReloaded,
    /// 没拿到（窗口没了 / WebView 也被拦 / 超时）。
    Failed,
}

#[cfg(not(target_os = "ios"))]
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 在 WebHome 窗口里为 `url` 取 `cf_clearance`。窗口不存在、首页地址
/// 读不出来、URL 非法时直接返回 `Failed`，不动窗口。
pub async fn obtain(
    app: &AppHandle,
    manager: &Arc<PluginManager>,
    plugin_id: &str,
    url: &str,
) -> Clearance {
    let Some(window) = app.get_webview_window("webhome") else {
        return Clearance::Failed;
    };
    let Ok(home_page) = window.url() else {
        return Clearance::Failed;
    };
    let home_page = home_page.as_str().to_string();
    let Ok(target) = url.parse::<Url>() else {
        return Clearance::Failed;
    };

    // 阶段一：隐藏 iframe（页面本身不导航，待回的 fm.req 还活着）。
    let iframe_js = format!(
        "(function(){{var d=document;if(!d||!d.body)return;var f=d.getElementById({});if(!f){{f=d.createElement('iframe');f.id={};f.style.cssText='position:fixed;left:0;top:0;width:1px;height:1px;opacity:0;pointer-events:none;border:0;';d.body.appendChild(f);}}f.src={};}})()",
        json!(FRAME_ID),
        json!(FRAME_ID),
        json!(url),
    );
    if window.eval(iframe_js).is_err() {
        return Clearance::Failed;
    }
    if let Some(found) = poll_for_clearance(&window, &target, PHASE_TIMEOUT).await {
        return finish(window, manager, plugin_id, found, &home_page).await;
    }
    // 阶段一没拿到：窗口还在首页才值得顶层导航（若已被挑战页顶离，
    // 顶层挑战已经在跑，继续等它跑完，不再另起导航）。
    let Ok(current) = window.url() else {
        return Clearance::Failed;
    };
    if current.as_str() == home_page && window.navigate(target.clone()).is_err() {
        remove_frame(&window);
        return Clearance::Failed;
    }
    if let Some(found) = poll_for_clearance(&window, &target, PHASE_TIMEOUT).await {
        return finish(window, manager, plugin_id, found, &home_page).await;
    }
    // 超时：清掉 iframe，把窗口导航回首页（若已被顶离）。
    remove_frame(&window);
    if let Ok(current) = window.url() {
        if current.as_str() != home_page {
            if let Ok(home) = home_page.parse::<Url>() {
                let _ = window.navigate(home);
            }
            return Clearance::PageReloaded;
        }
    }
    Clearance::Failed
}

/// 把拿到的通行证存进该插件的内存存储，再看窗口还在不在首页：
/// 在就 `Ready`（可重发），不在就导航回首页、回 `PageReloaded`。
async fn finish(
    window: WebviewWindow,
    manager: &Arc<PluginManager>,
    plugin_id: &str,
    cookie: ClearanceCookie,
    home_page: &str,
) -> Clearance {
    remove_frame(&window);
    if manager.set_clearance(plugin_id, cookie).await.is_err() {
        return Clearance::Failed;
    }
    match window.url() {
        Ok(current) if current.as_str() == home_page => Clearance::Ready,
        _ => {
            if let Ok(home) = home_page.parse::<Url>() {
                let _ = window.navigate(home);
            }
            Clearance::PageReloaded
        }
    }
}

/// 在窗口的 Cookie 存储里找一只 `cf_clearance`（iOS，v0.0.91）：走
/// `webhome` 的异步读——同步 `cookies_for_url` 会在主线程上转嵌套
/// run loop、重入 tao 的事件循环而 panic（见 webhome.rs 的
/// `read_webview_cookies_async`）。转换与旧路一致：domain 取原值、
/// 不去前导点、不小写。
#[cfg(target_os = "ios")]
async fn find_clearance_cookie(window: &WebviewWindow, target: &Url) -> Option<ClearanceCookie> {
    crate::webhome::read_webview_cookies_async(window, target)
        .await
        .into_iter()
        .find(|c| c.name.eq_ignore_ascii_case("cf_clearance"))
        .map(|c| ClearanceCookie {
            name: c.name,
            value: c.value,
            domain: c.domain,
            path: c.path,
            secure: c.secure,
            http_only: c.http_only,
            expires_at: c.expires_at,
        })
}

/// 在窗口的 Cookie 存储里找一只 `cf_clearance`（非 iOS）：
/// `cookies_for_url` 是阻塞调用（等主线程回消息），放 spawn_blocking 里。
#[cfg(not(target_os = "ios"))]
async fn find_clearance_cookie(window: &WebviewWindow, target: &Url) -> Option<ClearanceCookie> {
    let w = window.clone();
    let u = target.clone();
    tokio::task::spawn_blocking(move || w.cookies_for_url(u))
        .await
        .ok()
        .and_then(|r| r.ok())
        .and_then(|cookies| {
            cookies
                .into_iter()
                .find(|c| c.name().eq_ignore_ascii_case("cf_clearance"))
        })
        .map(|c| {
            let expires_at = c
                .max_age()
                .map(|age| now_ms().saturating_add(age.whole_milliseconds() as u64))
                .or_else(|| {
                    c.expires_datetime()
                        .map(|t| (t.unix_timestamp().max(0) as u64) * 1000)
                });
            ClearanceCookie {
                name: c.name().to_string(),
                value: c.value().to_string(),
                domain: c
                    .domain()
                    .map(str::to_string)
                    .unwrap_or_else(|| target.host_str().unwrap_or("").to_string()),
                path: c.path().unwrap_or("/").to_string(),
                secure: c.secure().unwrap_or(false),
                http_only: c.http_only().unwrap_or(false),
                expires_at,
            }
        })
}

/// 每秒读一次窗口的 Cookie 存储，拿到 `cf_clearance` 即返回；
/// 超时返回 None。
async fn poll_for_clearance(
    window: &WebviewWindow,
    target: &Url,
    budget: Duration,
) -> Option<ClearanceCookie> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if let Some(cookie) = find_clearance_cookie(window, target).await {
            return Some(cookie);
        }
        if tokio::time::Instant::now() + POLL_INTERVAL >= deadline {
            return None;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

fn remove_frame(window: &WebviewWindow) {
    let _ = window.eval(format!(
        "(function(){{var f=document.getElementById({});if(f)f.remove();}})()",
        json!(FRAME_ID)
    ));
}
