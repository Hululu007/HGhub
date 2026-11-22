// iOS 屏幕方向插件：Tauri 移动插件，名字 "orientation"（与 Android 的
// OrientationPlugin 同名，前端统一走 Rust 命令 set_orientation）。
// 写法对照 NativePlayerPlugin.swift 与官方 tauri-plugin-dialog：
//   - 基类：tauri-2.12.5/mobile/ios-api/Sources/Tauri/Plugin/Plugin.swift
//   - 调用约定：`@objc func xxx(_ invoke: Invoke) throws`，`invoke.parseArgs` /
//     `invoke.resolve` / `invoke.reject`，结尾的
//     `@_cdecl("init_plugin_...") public func initOrientationPlugin() -> Plugin`
// Rust 侧见 src-tauri/src/lib.rs（orientation 插件的 iOS 分支）与 commands.rs。
//
// 行为：setOrientation 是“设置一次方向”，不是“锁定”——调完之后用户再转
// 手机，系统仍可按设备方向转回来（Tauri 主界面支持 Info.plist 里声明的
// 全部方向）。AppDelegate 级的方向锁定不在本插件范围。
// landscape=false 切回竖屏；与 Android 的“恢复之前方向”语义不同，
// 前端按“竖屏视频→竖屏”调用它时两者结果一致。
//
// 机制（v0.0.22）：iOS 16+ 不再用 UIDevice KVC
// （setValue(forKey: "orientation") + attemptRotationToDeviceOrientation()）：
// 两段用户录像里自动与手动方向切换从没生效过——横屏视频播着，屏幕一直
// 竖屏——且旧实现调完就无条件 resolve，前端根本分不清成功还是失败。
// iOS 16 起方向归 UIWindowScene 管，改走
// scene.requestGeometryUpdate(.iOS(interfaceOrientations:))，
// 等一拍后按场景实际方向验收，方向对上才 resolve，对不上 reject；
// iOS 15 没有这个 API，才走 KVC 老路（#available 守卫，
// 两条路按系统版本二选一，不叠加）。
//
// 本机无 Xcode/Swift 工具链：本文件未编译，由 src-tauri/build.rs 在
// macOS 构建时经 SwiftLinker 编成静态库链进 Rust（见
// docs/developers/native-players.md）。

import Tauri
import UIKit
import AVFoundation
import MediaPlayer

/// Rust set_orientation 发来的 JSON（{"landscape": true/false}），
// 与 Android OrientationArgs 的字段名一致。
struct OrientationArgs: Decodable {
  var landscape: Bool
}

public class OrientationPlugin: Plugin {
  @objc public func setOrientation(_ invoke: Invoke) throws {
    let args = try invoke.parseArgs(OrientationArgs.self)
    DispatchQueue.main.async {
      if #available(iOS 16.0, *) {
        guard let scene = self.manager.viewController?.view.window?.windowScene
          ?? UIApplication.shared.connectedScenes.first(where: { $0.activationState == .foregroundActive }) as? UIWindowScene
        else {
          invoke.reject("无法获取窗口场景")
          return
        }
        let mask: UIInterfaceOrientationMask = args.landscape ? [.landscapeLeft, .landscapeRight] : .portrait
        // requestGeometryUpdate 成功时没有回调，只有失败时调 errorHandler，
        // 所以用 settled 守卫：失败立即 reject；否则等 0.5s 后按场景的
        // 实际方向验收，方向对上才 resolve，对不上 reject。
        var settled = false
        scene.requestGeometryUpdate(.iOS(interfaceOrientations: mask)) { error in
          guard !settled else { return }
          settled = true
          invoke.reject(error.localizedDescription)
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) {
          guard !settled else { return }
          settled = true
          if scene.interfaceOrientation.isLandscape == args.landscape {
            invoke.resolve()
          } else {
            invoke.reject("无法切换屏幕方向")
          }
        }
      } else {
        // iOS 15：requestGeometryUpdate 不可用，走原来的 KVC 老路。
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
/// 状态栏：横屏播放时隐藏（时间、电量、网速等系统信息不再压在视频上方），
/// 回竖屏或离开播放页时再显示。走 UIApplication.isStatusBarHidden，需要
/// Info.plist 里 UIViewControllerBasedStatusBarAppearance=false（由
/// scripts/sync-ios-status-bar.mjs 在 CI 生成工程后写入），否则调用会被
/// 按视图控制器的 appearance 忽略。isStatusBarHidden 自 iOS 9 起
/// deprecated，但在按视图控制器 appearance 关掉时仍可用；本机无 Xcode，
/// 与本文件其他方法一样未编译，真机验收时核对。
@objc public func setStatusBarHidden(_ invoke: Invoke) throws {
  struct StatusBarArgs: Decodable {
    var hidden: Bool
  }
  let args = try invoke.parseArgs(StatusBarArgs.self)
  DispatchQueue.main.async {
    UIApplication.shared.isStatusBarHidden = args.hidden
    invoke.resolve()
  }
}

/// 详情页左右竖滑调亮度/音量（仅详情页，首页卡片上下滑仍切视频）：
/// 前端按拖动增量调（上滑增大），每次返回调完后的新值（0..1）供指示条
/// 显示。亮度走 UIScreen.main.brightness，下限 0.05，避免全黑后看不
/// 见界面；音量走系统音量——读 AVAudioSession.outputVolume，写
/// MPVolumeView 的滑块（iOS 没有公开的设置系统音量 API，这是标准做
/// 法）。MPVolumeView 只有挂进窗口后滑块才能稳定读写系统音量，所以
/// 第一次用时把它（1x1、隐藏）加进当前窗口；找不到滑块时 reject，
/// 前端不更新指示条。两个方法都在主队列跑；本机无 Xcode，与本文件
/// 其他方法一样未编译，真机验收时核对（亮度/音量是否跟着变、指示条
/// 数字是否与系统值一致）。
private var volumeView: MPVolumeView?

private func volumeSlider() -> UISlider? {
  if volumeView == nil {
    let v = MPVolumeView(frame: CGRect(x: -100, y: -100, width: 1, height: 1))
    v.isHidden = true
    if let window = self.manager.viewController?.view.window
      ?? UIApplication.shared.connectedScenes.compactMap({ ($0 as? UIWindowScene)?.keyWindow }).first {
      window.addSubview(v)
    } else {
      self.manager.viewController?.view.addSubview(v)
    }
    v.layoutIfNeeded()
    volumeView = v
  }
  return volumeView?.subviews.compactMap { $0 as? UISlider }.first
}

@objc public func adjustBrightness(_ invoke: Invoke) throws {
  struct AdjustArgs: Decodable {
    var delta: Double
  }
  let args = try invoke.parseArgs(AdjustArgs.self)
  DispatchQueue.main.async {
    let current = Double(UIScreen.main.brightness)
    let next = min(1.0, max(0.05, current + args.delta))
    UIScreen.main.brightness = CGFloat(next)
    invoke.resolve(["value": next])
  }
}

@objc public func adjustVolume(_ invoke: Invoke) throws {
  struct AdjustArgs: Decodable {
    var delta: Double
  }
  let args = try invoke.parseArgs(AdjustArgs.self)
  DispatchQueue.main.async {
    let current = Double(AVAudioSession.sharedInstance().outputVolume)
    let next = min(1.0, max(0.0, current + args.delta))
    guard let slider = self.volumeSlider() else {
      invoke.reject("找不到系统音量滑块")
      return
    }
    slider.value = Float(next)
    slider.sendActions(for: .valueChanged)
    invoke.resolve(["value": next])
  }
}
}

/// Rust 侧 `tauri::ios_plugin_binding!(init_plugin_orientation)` 找的就是
/// 这个符号；C 符号名改了要同步改 Rust 侧的宏参数。Swift 函数名本身
/// 在包内必须唯一（NativePlayerPlugin 里的同名函数叫 initNativePlayerPlugin）。
@_cdecl("init_plugin_orientation")
public func initOrientationPlugin() -> Plugin {
  return OrientationPlugin()
}
