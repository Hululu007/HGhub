import { invoke } from "@tauri-apps/api/core";

/** 打开某插件的 WebHome 首页（独立 WebView 窗口，见 08 §10）。 */
export function openWebhome(pluginId: string) {
  return invoke<{ url: string }>("open_webhome", { pluginId });
}

/**
 * 清理 WebHome 全部数据（v0.0.83）：删页面 localStorage 里的三个域名
 * 缓存键（wo_domain_cache / wo_monitor_snapshot / wo_site_domains）、
 * 全清 WebView 浏览数据、清绑定插件的 Rust 请求 Cookie 罐，再重载页面。
 * 返回每键清理前是否存在、Cookie 罐清掉的条数。
 * WebHome 还没打开过时后端报 WEBHOME_NOT_OPEN。
 */
export interface ClearWebhomeDataResult {
  cleared: string[];
  absent: string[];
  jar_cleared: number;
}

export function clearWebhomeData() {
  return invoke<ClearWebhomeDataResult>("clear_webhome_data");
}
