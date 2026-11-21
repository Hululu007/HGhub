// iOS 屏幕方向插件：Tauri 移动插件，名字 "orientation"（与 Android 的
// OrientationPlugin 同名，前端统一走 Rust 命令 set_orientation）。
// 写法对照 NativePlayerPlugin.swift 与官方 tauri-plugin-dialog：
//   - 基类：tauri-2.12.5/mobile/ios-api/Sources/Tauri/Plugin/Plugin.swift
//   - 调用约定：`@objc func xxx(_ invoke: Invoke) throws`，`invoke.parseArgs` /
//     `invoke.resolve` / `invoke.reject`，结尾的
//     `@_cdecl("init_plugin_...") public func initPlugin() -> Plugin`
// Rust 侧见 src-tauri/src/lib.rs（orientation 插件的 iOS 分支）与 commands.rs。
//
// 行为：setOrientation 是“设置一次方向”，不是“锁定”——调完之后用户再转
// 手机，系统仍可按设备方向转回来（Tauri 主界面支持 Info.plist 里声明的
// 全部方向）。AppDelegate 级的方向锁定不在本插件范围。
// landscape=false 切回竖屏；与 Android 的“恢复之前方向”语义不同，
// 前端按“竖屏视频→竖屏”调用它时两者结果一致。
//
// 本机无 Xcode/Swift 工具链：本文件未编译，待 Mac 上第一次
// `pnpm tauri ios init --ci` 后由 Xcode 校验（见 docs/developers/native-players.md）。

import Tauri
import UIKit

/// Rust set_orientation 发来的 JSON（{"landscape": true/false}），
// 与 Android OrientationArgs 的字段名一致。
struct OrientationArgs: Decodable {
  var landscape: Bool
}

public class OrientationPlugin: Plugin {
  @objc public func setOrientation(_ invoke: Invoke) throws {
    let args = try invoke.parseArgs(OrientationArgs.self)
    DispatchQueue.main.async {
      if args.landscape {
        // 已是横屏方向时保持当前横屏朝向，否则默认横屏左。
        let current = UIDevice.current.orientation
        let target: UIInterfaceOrientation =
          (current == .landscapeLeft || current == .landscapeRight)
          ? UIInterfaceOrientation(rawValue: current.rawValue) ?? .landscapeLeft
          : .landscapeLeft
        UIDevice.current.setValue(target.rawValue, forKey: "orientation")
      } else {
        UIDevice.current.setValue(UIInterfaceOrientation.portrait.rawValue, forKey: "orientation")
      }
      UIViewController.attemptRotationToDeviceOrientation()
      invoke.resolve()
    }
  }
}

/// Rust 侧 `tauri::ios_plugin_binding!(init_plugin_orientation)` 找的就是
/// 这个符号；名字改了要同步改 Rust 侧的宏参数。
@_cdecl("init_plugin_orientation")
public func initPlugin() -> Plugin {
  return OrientationPlugin()
}
