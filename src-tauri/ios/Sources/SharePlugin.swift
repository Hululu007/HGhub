// iOS 分享插件：Tauri 移动插件，名字 "share"。下载完成后把文件递给系统
// 分享面（UIActivityViewController），用户可存到“文件”、发到微信等。
// 写法对照 OrientationPlugin.swift：`@objc func xxx(_ invoke: Invoke) throws`，
// `invoke.parseArgs` / `invoke.resolve` / `invoke.reject`，结尾的
// `@_cdecl("init_plugin_...") public func initSharePlugin() -> Plugin`。
// Rust 侧见 src-tauri/src/lib.rs（share 插件的 iOS 分支）与 commands.rs。
//
// 本机无 Xcode/Swift 工具链：本文件未编译，由 src-tauri/build.rs 在
// macOS 构建时经 SwiftLinker 编成静态库链进 Rust（见
// docs/developers/native-players.md）。真机验收：下载完成后点“分享”，
// 系统分享面能弹出、文件能存到“文件”App。

import Tauri
import UIKit

public class SharePlugin: Plugin {
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
}

/// Rust 侧 `tauri::ios_plugin_binding!(init_plugin_share)` 找的就是
/// 这个符号；C 符号名改了要同步改 Rust 侧的宏参数。
@_cdecl("init_plugin_share")
public func initSharePlugin() -> Plugin {
  return SharePlugin()
}
