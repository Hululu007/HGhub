import { invoke } from "@tauri-apps/api/core";

/** 打开某插件的 WebHome 首页（独立 WebView 窗口，见 08 §10）。 */
export function openWebhome(pluginId: string) {
  return invoke<{ url: string }>("open_webhome", { pluginId });
}

/**
 * 全 App 缓存清理（v0.0.84；v0.0.83 是“清理 WebHome 全部数据”）：
 * 删页面 localStorage 里的三个域名缓存键（wo_domain_cache /
 * wo_monitor_snapshot / wo_site_domains）、全清 WebView 浏览数据、
 * 清全部插件的 Rust 请求 Cookie 罐与响应缓存、`webhome:` 前缀的存储键、
 * 全部会话的媒体预加载缓存、插件目录下的 `__pycache__` 与 `*.pyc`、
 * Python 边车已加载蜘蛛的运行时缓存（边车在运行时才清）、插件导入
 * 暂存文件，再重载页面；最后清日志（全部插件的 ext.log 内存缓冲 +
 * App 自己的 debug.log 文件）。返回每键清理前是否存在、各类别清掉
 * 的数量、日志是否已清。WebHome 还没打开过时后端报 WEBHOME_NOT_OPEN。
 * 不碰：App 设置、导入的插件及插件文件、插件存储里非 `webhome:` 的键、
 * 下载好的视频、插件源文件、HTML 源文件。
 */
export interface ClearWebhomeDataResult {
  cleared: string[];
  absent: string[];
  jar_cleared: number;
  plugins: number;
  response_cache_dropped: number;
  webhome_keys_deleted: number;
  media_entries_dropped: number;
  pycache_dirs: number;
  pycache_files: number;
  py_running: boolean;
  py_spiders_total: number;
  py_spiders_cleared: number;
  staging_files: number;
  log_entries_dropped: number;
  debug_log_cleared: boolean;
  warnings: string[];
}

export function clearWebhomeData() {
  return invoke<ClearWebhomeDataResult>("clear_webhome_data");
}
