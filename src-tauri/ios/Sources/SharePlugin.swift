// iOS 分享插件：Tauri 移动插件，名字 "share"。下载完成后把文件递给系统
// 分享面（UIActivityViewController），用户可存到“文件”、发到微信等。
// 还管两件事：跳转 SenPlayer（openSenPlayer，用 canOpenURL 判是否装了，
// 未装明确报错；Info.plist 须有 LSApplicationQueriesSchemes: senplayer，
// 见 scripts/sync-ios-senplayer.mjs），以及导出到相册（saveToPhotos，
// PHPhotoLibrary 直接存视频；Info.plist 须有 NSPhotoLibraryAddUsageDescription，
// 见 scripts/sync-ios-photo-library.mjs）。
// 写法对照 OrientationPlugin.swift：`@objc func xxx(_ invoke: Invoke) throws`，
// `invoke.parseArgs` / `invoke.resolve` / `invoke.reject`，结尾的
// `@_cdecl("init_plugin_...") public func initSharePlugin() -> Plugin`。
// Rust 侧见 src-tauri/src/lib.rs（share 插件的 iOS 分支）与 commands.rs。
//
// 本机无 Xcode/Swift 工具链：本文件未编译，由 src-tauri/build.rs 在
// macOS 构建时经 SwiftLinker 编成静态库链进 Rust（见
// docs/developers/native-players.md）。真机验收：下载完成后点“分享”，
// 系统分享面能弹出、文件能存到“文件”App；点“SenPlayer 播放”能跳过去，
// 未装时提示未安装；导出到相册能在相册里找到视频。

import Photos
import Tauri
import UIKit

public class SharePlugin: Plugin {
  /// “用…打开”菜单须一直强引用着 UIDocumentInteractionController，
  /// 菜单关掉前不能放，否则一点就闪退。插件实例随应用常驻，放这里正好。
  private var documentInteractionController: UIDocumentInteractionController?

  @objc public func shareFile(_ invoke: Invoke) throws {
    struct ShareArgs: Decodable {
      var path: String
    }
    let args = try invoke.parseArgs(ShareArgs.self)
    DispatchQueue.main.async {
      guard FileManager.default.fileExists(atPath: args.path) else {
        invoke.reject("文件不存在")
        return
      }
      let url = URL(fileURLWithPath: args.path)
      let vc = UIActivityViewController(activityItems: [url], applicationActivities: nil)
      // iPad 上没有锚点会崩；iPhone 也给一个安全的源视图。
      if let popover = vc.popoverPresentationController {
        popover.sourceView = self.manager.viewController?.view
        popover.sourceRect = CGRect(x: 0, y: 0, width: 1, height: 1)
      }
      guard let presenter = self.manager.viewController else {
        invoke.reject("无法获取视图控制器")
        return
      }
      // 分享面弹出即 resolve（用户选什么、是否保存，由系统分享面接管）。
      presenter.present(vc, animated: true) {
        invoke.resolve()
      }
    }
  }

  /// 用外部播放器打开已下载文件：弹系统“用…打开”菜单（SenPlayer 在
  /// 列），用户点 SenPlayer 就把文件交给它播。为什么不用 SenPlayer 的
  /// URL Scheme 直接跳：下载的文件在红苹果沙盒里，别的 App 读不到
  /// file:// 地址；回环地址那条路要红苹果在后台播着才不断流，下载
  /// 本来就是为了摆脱这个依赖。文件是直接交给 SenPlayer 的，播起来
  /// 不用红苹果在后台留着。菜单里没有能打开的应用时明确 reject。
  @objc public func openIn(_ invoke: Invoke) throws {
    struct OpenInArgs: Decodable {
      var path: String
    }
    let args = try invoke.parseArgs(OpenInArgs.self)
    DispatchQueue.main.async {
      guard FileManager.default.fileExists(atPath: args.path) else {
        invoke.reject("文件不存在")
        return
      }
      guard let presenter = self.manager.viewController else {
        invoke.reject("无法获取视图控制器")
        return
      }
      let url = URL(fileURLWithPath: args.path)
      let controller = UIDocumentInteractionController(url: url)
      self.documentInteractionController = controller
      let shown = controller.presentOpenInMenu(
        from: CGRect(x: 0, y: 0, width: 1, height: 1),
        in: presenter.view, animated: true)
      if shown {
        invoke.resolve()
      } else {
        self.documentInteractionController = nil
        invoke.reject("没有能打开这个文件的应用")
      }
    }
  }

  /// 跳转 SenPlayer：canOpenURL 先判是否装了（须在 Info.plist 的
  /// LSApplicationQueriesSchemes 里登记 senplayer，否则 iOS 永远返回
  /// false）。未装明确 reject，不静默无反应；装了就 open，open 回调
  /// 失败也 reject。
  @objc public func openSenPlayer(_ invoke: Invoke) throws {
    struct SenPlayerArgs: Decodable {
      var url: String
    }
    let args = try invoke.parseArgs(SenPlayerArgs.self)
    guard let url = URL(string: args.url), url.scheme == "senplayer" else {
      invoke.reject("无效的 SenPlayer 地址")
      return
    }
    DispatchQueue.main.async {
      guard UIApplication.shared.canOpenURL(url) else {
        invoke.reject("未安装 SenPlayer，请先安装后再跳转")
        return
      }
      UIApplication.shared.open(url, options: [:]) { ok in
        DispatchQueue.main.async {
          if ok { invoke.resolve() } else { invoke.reject("打开 SenPlayer 失败") }
        }
      }
    }
  }

  /// 导出到相册：PHPhotoLibrary 直接把视频存进相册（addOnly 授权即可，
  /// 不读相册）。iOS 14+ 用 requestAuthorization(for: .addOnly)，更早
  /// 的系统走旧的无参接口；授权被拒明确 reject，不静默。
  @objc public func saveToPhotos(_ invoke: Invoke) throws {
    struct SaveArgs: Decodable {
      var path: String
    }
    let args = try invoke.parseArgs(SaveArgs.self)
    DispatchQueue.main.async {
      guard FileManager.default.fileExists(atPath: args.path) else {
        invoke.reject("文件不存在")
        return
      }
      let url = URL(fileURLWithPath: args.path)
      let save = {
        PHPhotoLibrary.shared().performChanges({
          PHAssetChangeRequest.creationRequestForAssetFromVideo(atFileURL: url)
        }) { ok, error in
          DispatchQueue.main.async {
            if ok { invoke.resolve() } else { invoke.reject(error?.localizedDescription ?? "保存到相册失败") }
          }
        }
      }
      if #available(iOS 14, *) {
        PHPhotoLibrary.requestAuthorization(for: .addOnly) { status in
          guard status == .authorized || status == .limited else {
            DispatchQueue.main.async { invoke.reject("未允许访问相册，无法保存") }
            return
          }
          save()
        }
      } else {
        PHPhotoLibrary.requestAuthorization { status in
          guard status == .authorized else {
            DispatchQueue.main.async { invoke.reject("未允许访问相册，无法保存") }
            return
          }
          save()
        }
      }
    }
  }
}

/// Rust 侧 `tauri::ios_plugin_binding!(init_plugin_share)` 找的就是
/// 这个符号；C 符号名改了要同步改 Rust 侧的宏参数。
@_cdecl("init_plugin_share")
public func initSharePlugin() -> Plugin {
  return SharePlugin()
}
