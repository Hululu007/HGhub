import { invoke, isTauri } from "@tauri-apps/api/core";
import { platform } from "./environment";

/** Rotate the mobile activity itself; browser-only playback uses the Screen API.
 *  Android 与 iOS 都走 Rust 命令 set_orientation（Android 调 Kotlin
 *  OrientationPlugin，iOS 调 Swift OrientationPlugin）；iOS 的 WKWebView
 *  不支持 Screen Orientation API 的 lock()，所以不能走浏览器分支。 */
export async function setMobileOrientation(landscape: boolean): Promise<void> {
  if ((platform === "android" || platform === "ios") && isTauri()) {
    await invoke("set_orientation", { landscape });
    return;
  }
  const orientation = screen.orientation as ScreenOrientation & { lock?: (value: string) => Promise<void> };
  if (landscape) {
    if (!orientation?.lock) throw new Error("当前环境不支持切换屏幕方向");
    await orientation.lock("landscape");
  } else {
    orientation?.unlock?.();
  }
}
