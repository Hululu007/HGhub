//! iOS 启动时预注册 WebKit 的 CFBundle，避免启动闪退。
//!
//! 证据（用户真机 .ips，红苹果影视 0.0.12，iPhone 13 Pro Max，iOS 16.6.1）：
//! 启动时 `tauri-runtime-wry` 的 `Wry::init` 调 `wry::webview_version()`，
//! 它调 `NSBundle::bundleWithIdentifier("com.apple.WebKit")`；此时还没有任何
//! WKWebView，WebKit 的 CFBundle 尚未注册，CoreFoundation 走
//! `_CFBundleEnsureBundleExistsForImagePath` →
//! `__CFBundleCopyFrameworkURLForExecutablePath`，后者返回 NULL，
//! 调用方直接 `CFRelease(NULL)`，触发 EXC_BREAKPOINT（SIGTRAP），
//! 主线程死在 `main → run → Builder::run → Wry::init → webview_version`，
//! 任何界面出现之前。`webview_version()` 的返回值只被当布尔用
//! （`webview_runtime_installed: wry::webview_version().is_ok()`），
//! 却足以让应用起不来。
//!
//! 修复：在 `Builder::run()` 之前先用 `bundleWithPath:` 把 WebKit 的
//! bundle 注册进 CFBundle 表。`bundleWithPath:` 只按给定路径读 Info.plist，
//! 不走 identifier 查找那条路；注册之后 `bundleWithIdentifier:` 直接命中
//! 已注册的表，不再扫描 image，也就不会再撞上那个 NULL 释放。
//! 返回值故意忽略：路径不对时只是没注册上，不改变任何行为、不 panic。

/// 在 Tauri 启动之前把 WebKit 的 bundle 注册进 CFBundle 表。
/// 必须在 `Builder::run()` 之前调——`Wry::init` 里第一次碰 WebKit
/// bundle 的就是那个会闪退的 `bundleWithIdentifier` 调用。
pub fn preload() {
    use objc2_foundation::{ns_string, NSBundle};
    let _ = NSBundle::bundleWithPath(ns_string!("/System/Library/Frameworks/WebKit.framework"));
}
