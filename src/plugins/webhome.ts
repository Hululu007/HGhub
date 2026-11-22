import { invoke } from "@tauri-apps/api/core";

/** 打开某插件的 WebHome 首页（独立 WebView 窗口，见 08 §10）。 */
export function openWebhome(pluginId: string) {
  return invoke<{ url: string }>("open_webhome", { pluginId });
}
