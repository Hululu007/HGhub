import { invoke, isTauri } from "@tauri-apps/api/core";
import { platform } from "./environment";

/** 详情页左右竖滑调亮度（左半屏）/音量（右半屏），仅 iOS 原生应用。
 * 亮度走 UIScreen.main.brightness（下限 0.05，避免全黑），音量走系统
 * 音量（MPVolumeView 滑块；iOS 没有公开的设置系统音量 API，这是标准
 * 做法）。每次按拖动增量调，返回调完后的新值（0..1）供指示条显示。
 * 非 iOS 或网页版返回 null（手势不启用）；失败也返回 null，不抛错，
 * 不影响播放。 */
export async function adjustBrightness(delta: number): Promise<number | null> {
  if (platform !== "ios" || !isTauri()) return null;
  try {
    return await invoke<number>("adjust_brightness", { delta });
  } catch {
    return null;
  }
}

export async function adjustVolume(delta: number): Promise<number | null> {
  if (platform !== "ios" || !isTauri()) return null;
  try {
    return await invoke<number>("adjust_volume", { delta });
  } catch {
    return null;
  }
}
