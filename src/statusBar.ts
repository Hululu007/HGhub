import { invoke, isTauri } from "@tauri-apps/api/core";
import { platform } from "./environment";

/** 横屏播放时隐藏 iOS 状态栏（时间、电量、网速等系统信息不再压在视频
 *  上方），回竖屏或离开播放页时再显示。只在 iOS 上调原生命令：
 *  Swift 侧（OrientationPlugin.setStatusBarHidden）走
 *  UIApplication.isStatusBarHidden，需要 Info.plist 里
 *  UIViewControllerBasedStatusBarAppearance=false（scripts/
 *  sync-ios-status-bar.mjs 在 CI 生成工程后写入），否则调用会被
 *  按视图控制器的 appearance 忽略。失败静默，不影响播放。 */
export async function setStatusBarHidden(hidden: boolean): Promise<void> {
  if (platform === "ios" && isTauri()) {
    await invoke("set_status_bar_hidden", { hidden });
  }
}
