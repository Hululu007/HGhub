// mpv 引擎（libmpv / MPVKit）：四个里格式和 ASS 字幕最强，但也最难接。
// 没有官方现成的 iOS 包：Libmpv.xcframework 必须在 Mac 上自己编
// （FFmpeg、libass、libplacebo、dav1d 等整条依赖链），不能从 Linux
// 交叉编译；省事的路是 MPVKit 的 SPM，但要选 LGPL 版（GPL 版含
// Samba 等 GPL 组件，发行口径与 HGhub 的 PolyForm Noncommercial 冲突）。
// 详见 docs/developers/native-players.md。
//
// 依赖未装时走 #else 分支：open 直接报 unavailable，说明缺什么、怎么装。
// #if 分支里按 MPVKit 的 MPVPlayer / MPVGLView 所写，类名/方法名以
// 接入的 MPVKit 版本为准，第一次在 Mac 上编时由 Xcode 校验。

import UIKit

#if canImport(MPVKit)
import MPVKit

final class MPVEngine: NSObject, NativePlayerEngine {
  var eventHandler: ((NativePlayerEvent) -> Void)?

  private var player: MPVPlayer?
  private var glView: MPVGLView?
  private var container: UIViewController?
  private var timer: Timer?
  private var didFireEnded = false
  private var engineName = "mpv"

  /// 分辨率上报暂未接这个引擎：先报未知（.zero），覆盖层与 state
  /// 按 0 处理为不显示；AVPlayer 已报真实尺寸。
  var videoSize: CGSize { .zero }

  var currentState: NativePlayerState {
    let position = player.map { $0.position } ?? 0
    let duration = player.map { $0.duration } ?? 0
    return NativePlayerState(
      engine: engineName,
      playing: player?.isPlaying ?? false,
      position: position.isFinite ? position : 0,
      duration: duration.isFinite ? duration : 0,
      rate: 1,
      volume: 1,
      muted: false,
      videoWidth: 0,
      videoHeight: 0)
  }

  func open(
    url: URL, position: Double, rate: Double, volume: Double, muted: Bool, title: String?,
    present: @escaping (UIViewController) -> Void,
    completion: @escaping (Result<NativePlayerState, Error>) -> Void
  ) {
    // mpv 的倍速/音量由前端 open 后的 setRate/setVolume 命令再调。
    _ = (rate, volume, muted, title)
    let glView = MPVGLView()
    let player = MPVPlayer()
    player.attach(to: glView)
    self.glView = glView
    self.player = player

    let container = UIViewController()
    container.modalPresentationStyle = .fullScreen
    container.view.backgroundColor = .black
    glView.frame = container.view.bounds
    glView.autoresizingMask = [.flexibleWidth, .flexibleHeight]
    container.view.addSubview(glView)
    self.container = container
    present(container)

    // 回环 session 地址直接播：上游头/Range/密钥由 Rust 代理处理，
    // 不用再给 mpv 拼 http-header-fields。
    player.loadFile(url.absoluteString, mode: .replace)
    if position > 0 {
      player.seek(to: position)
    }
    player.play()
    didFireEnded = false
    timer?.invalidate()
    timer = Timer.scheduledTimer(withTimeInterval: 0.5, repeats: true) { [weak self] _ in
      guard let self = self else { return }
      let state = self.currentState
      self.eventHandler?(.time(position: state.position, duration: state.duration, videoSize: self.videoSize))
      if !self.didFireEnded && state.duration > 0 && state.position >= state.duration - 0.3 {
        self.didFireEnded = true
        self.eventHandler?(.ended)
      }
    }
    completion(.success(currentState))
  }

  func play() {
    player?.play()
  }

  func pause() {
    player?.pause()
  }

  func seek(to position: Double) {
    player?.seek(to: position)
  }

  func setRate(_ rate: Double) {
    player?.setRate(rate)
  }

  func setVolume(_ volume: Double, muted: Bool) {
    player?.setVolume(muted ? 0 : volume)
  }

  func close() {
    timer?.invalidate()
    timer = nil
    player?.stop()
    player = nil
    glView?.removeFromSuperview()
    glView = nil
    container?.dismiss(animated: true, completion: nil)
    container = nil
  }
}

#else

/// MPVKit 未接入（未在 Mac 上编出 Libmpv.xcframework / 未加 SPM 依赖）
// 时的替身：open 直接失败，报错里写清怎么装，不冒充可用。
final class MPVEngine: NSObject, NativePlayerEngine {
  var eventHandler: ((NativePlayerEvent) -> Void)?
  private var engineName = "mpv"

  var currentState: NativePlayerState {
    NativePlayerState(
      engine: engineName, playing: false, position: 0, duration: 0, rate: 1, volume: 1,
      muted: false, videoWidth: 0, videoHeight: 0)
  }

  func open(
    url: URL, position: Double, rate: Double, volume: Double, muted: Bool, title: String?,
    present: @escaping (UIViewController) -> Void,
    completion: @escaping (Result<NativePlayerState, Error>) -> Void
  ) {
    _ = (url, position, rate, volume, muted, title, present)
    completion(
      .failure(
        NativePlayerUnavailableError(message:
          "mpv 未接入：请在 Mac 上编出 Libmpv.xcframework 或用 SPM 加入 MPVKit"
          + "（选 LGPL 版），再同步到 Xcode 工程；"
          + "详见 docs/developers/native-players.md")))
  }

  func play() {}
  func pause() {}
  func seek(to position: Double) {}
  func setRate(_ rate: Double) {}
  func setVolume(_ volume: Double, muted: Bool) {}
  func close() {}
}

#endif
