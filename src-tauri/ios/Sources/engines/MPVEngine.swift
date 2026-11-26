// mpv 引擎（libmpv / MPVKit）：四个里格式和 ASS 字幕最强，但也最难接。
// MPVKit（mpvkit/MPVKit，LGPL 版产品 `MPVKit`）只提供 libmpv/FFmpeg
// 的 XCFramework，没有 Swift 播放器类——MPVPlayer/MPVGLView 是另一个
// libmpv-ios 项目的包，不在 MPVKit 里。所以这里直接调 libmpv 的 C API
//（import Libmpv），渲染走 MPVKit Demo 同款：CAMetalLayer +
// vo=gpu-next + gpu-api=vulkan + gpu-context=moltenvk，
// hwdec=videotoolbox（H.264/HEVC 硬解；AV1 等走 dav1d 软解）。
// 钉的版本见 src-tauri/ios/Package.swift（MPVKit 0.40.0 exact，
// iOS 13.0 底线；0.41.0 起要 iOS 14.0，1.x 起要 iOS 15.0）。
// 详见 docs/developers/native-players.md。
//
// 依赖未装时走 #else 分支：open 直接报 unavailable，说明缺什么、怎么装。
// 第一次在 Mac 上编时以接入的 MPVKit 版本为准由 Xcode 校验。

import UIKit

#if canImport(Libmpv)
import Libmpv

/// mpv 渲染用的 Metal 层（照搬 MPVKit Demo 的 MetalLayer）：
/// MoltenVK 会把 drawableSize 压成 1x1 强行完成一帧，导致闪烁还可能
/// 一直停在 1x1，这里拦掉；EDR 开关必须在主线程改。
final class MPVMetalLayer: CAMetalLayer {
  override var drawableSize: CGSize {
    get { super.drawableSize }
    set {
      if Int(newValue.width) > 1 && Int(newValue.height) > 1 {
        super.drawableSize = newValue
      }
    }
  }

  override var wantsExtendedDynamicRangeContent: Bool {
    get { super.wantsExtendedDynamicRangeContent }
    set {
      if Thread.isMainThread {
        super.wantsExtendedDynamicRangeContent = newValue
      } else {
        DispatchQueue.main.sync { super.wantsExtendedDynamicRangeContent = newValue }
      }
    }
  }
}

/// 承载 Metal 层的 view：layerClass 换成 MPVMetalLayer，尺寸变化时
/// 层跟着变（横竖屏旋转不用另处理）。
final class MPVVideoView: UIView {
  override class var layerClass: AnyClass { MPVMetalLayer.self }
  var metalLayer: MPVMetalLayer { layer as! MPVMetalLayer }
}

final class MPVEngine: NSObject, NativePlayerEngine {
  var eventHandler: ((NativePlayerEvent) -> Void)?

  private var mpv: OpaquePointer?
  private var videoView: MPVVideoView?
  private var container: UIViewController?
  private var timer: Timer?
  private var eventQueue = DispatchQueue(label: "mpv-event", qos: .userInitiated)
  private var didFireEnded = false
  private var didFireError = false
  private var engineName = "mpv"
  private var videoW = 0
  private var videoH = 0

  /// 分辨率：dwidth/dheight（mpv 实际显示尺寸），没拿到就报未知（.zero）。
  var videoSize: CGSize { CGSize(width: videoW, height: videoH) }

  var currentState: NativePlayerState {
    let position = getDouble("time-pos")
    let duration = getDouble("duration")
    let playing = mpv != nil && !getFlag("pause") && !getFlag("idle-active")
    return NativePlayerState(
      engine: engineName,
      playing: playing,
      position: position.isFinite ? position : 0,
      duration: duration.isFinite ? duration : 0,
      rate: getDouble("speed"),
      volume: getDouble("volume") / 100,
      muted: getFlag("mute"),
      videoWidth: videoW,
      videoHeight: videoH)
  }

  /// 引擎被插件丢掉也没调 close() 时也要把 libmpv 句柄销毁，
  /// 不然 wakeup 回调还会往已释放的 self 上打。
  deinit {
    timer?.invalidate()
    if let handle = mpv {
      mpv = nil
      mpv_terminate_destroy(handle)
    }
  }

  func open(
    url: URL, position: Double, rate: Double, volume: Double, muted: Bool, title: String?,
    present: @escaping (UIViewController) -> Void,
    completion: @escaping (Result<NativePlayerState, Error>) -> Void
  ) {
    _ = title
    guard let handle = mpv_create() else {
      completion(.failure(NativePlayerUnavailableError(message: "mpv 创建失败")))
      return
    }
    // 渲染层先建好：wid 要的就是这个 CAMetalLayer，initialize 之前就要给。
    // 容器先建不 present：loadfile 万一失败直接销毁，连 present 都不走，
    // 避免 present 后立刻 dismiss 的竞态。
    let videoView = MPVVideoView()
    let container = UIViewController()
    container.modalPresentationStyle = .fullScreen
    container.view.backgroundColor = .black
    videoView.frame = container.view.bounds
    videoView.autoresizingMask = [.flexibleWidth, .flexibleHeight]
    container.view.addSubview(videoView)
    self.videoView = videoView
    self.container = container

    var layer = videoView.metalLayer
    mpv_set_option(handle, "wid", MPV_FORMAT_INT64, &layer)
    mpv_set_option_string(handle, "vo", "gpu-next")
    mpv_set_option_string(handle, "gpu-api", "vulkan")
    mpv_set_option_string(handle, "gpu-context", "moltenvk")
    mpv_set_option_string(handle, "hwdec", "videotoolbox")
    mpv_set_option_string(handle, "subs-match-os-language", "yes")
    mpv_set_option_string(handle, "subs-fallback", "yes")
    mpv_set_option_string(handle, "video-rotate", "no")
    #if DEBUG
    mpv_request_log_messages(handle, "debug")
    #else
    mpv_request_log_messages(handle, "no")
    #endif
    guard mpv_initialize(handle) >= 0 else {
      mpv_terminate_destroy(handle)
      self.videoView = nil
      self.container = nil
      completion(.failure(NativePlayerUnavailableError(message: "mpv 初始化失败")))
      return
    }
    self.mpv = handle
    mpv_observe_property(handle, 0, "dwidth", MPV_FORMAT_INT64)
    mpv_observe_property(handle, 0, "dheight", MPV_FORMAT_INT64)
    mpv_set_wakeup_callback(handle, { ctx in
      guard let ctx = ctx else { return }
      let engine = Unmanaged<MPVEngine>.fromOpaque(ctx).takeUnretainedValue()
      engine.readEvents()
    }, UnsafeMutableRawPointer(Unmanaged.passUnretained(self).toOpaque()))

    // 回环 session 地址直接播：上游头/Range/密钥由 Rust 代理处理，
    // 不用再给 mpv 拼 http-header-fields。
    // loadfile 只是把命令排队（异步），返回 <0 才是参数级错误。
    if command(handle, "loadfile", args: [url.absoluteString, "replace"]) < 0 {
      close()
      completion(.failure(NativePlayerUnavailableError(message: "mpv 打开失败")))
      return
    }
    present(container)
    if position > 0 {
      _ = command(handle, "seek", args: ["\(position)", "absolute"])
    }
    setRate(rate)
    setVolume(volume, muted: muted)
    setFlag(handle, "pause", false)
    didFireEnded = false
    didFireError = false
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
    guard let handle = mpv else { return }
    setFlag(handle, "pause", false)
  }

  func pause() {
    guard let handle = mpv else { return }
    setFlag(handle, "pause", true)
  }

  func seek(to position: Double) {
    guard let handle = mpv else { return }
    _ = command(handle, "seek", args: ["\(position)", "absolute"])
  }

  func setRate(_ rate: Double) {
    guard let handle = mpv else { return }
    var value = rate
    mpv_set_property(handle, "speed", MPV_FORMAT_DOUBLE, &value)
  }

  func setVolume(_ volume: Double, muted: Bool) {
    guard let handle = mpv else { return }
    var value = muted ? 0 : volume * 100
    mpv_set_property(handle, "volume", MPV_FORMAT_DOUBLE, &value)
    setFlag(handle, "mute", muted)
  }

  func close() {
    timer?.invalidate()
    timer = nil
    // 先把引用清掉再销毁：后台事件循环每次循环都重读 self.mpv，
    // 读到 nil 就退出，不会再对已销毁的句柄调 mpv_wait_event。
    // terminate_destroy 会把阻塞在 wait_event 里的循环唤醒。
    let handle = mpv
    mpv = nil
    if let handle = handle {
      _ = command(handle, "stop")
      mpv_terminate_destroy(handle)
    }
    videoView?.removeFromSuperview()
    videoView = nil
    container?.dismiss(animated: true, completion: nil)
    container = nil
    didFireEnded = false
    didFireError = false
    videoW = 0
    videoH = 0
  }

  // MARK: - libmpv 小工具（照搬 MPVKit Demo 的同名做法）

  private func getDouble(_ name: String) -> Double {
    guard let handle = mpv else { return 0 }
    var data = Double()
    mpv_get_property(handle, name, MPV_FORMAT_DOUBLE, &data)
    return data
  }

  private func getFlag(_ name: String) -> Bool {
    guard let handle = mpv else { return false }
    var data = Int64()
    mpv_get_property(handle, name, MPV_FORMAT_FLAG, &data)
    return data > 0
  }

  private func setFlag(_ handle: OpaquePointer, _ name: String, _ flag: Bool) {
    var data: Int = flag ? 1 : 0
    mpv_set_property(handle, name, MPV_FORMAT_FLAG, &data)
  }

  /// 发一条 mpv 命令（argv 式）；返回 mpv 的错误码（0 成功，负数失败）。
  /// C 参数用 strdup/free 管理（Demo 同款）。
  @discardableResult
  private func command(_ handle: OpaquePointer, _ command: String, args: [String?] = []) -> Int32 {
    var cargs = makeCArgs(command, args).map { $0.flatMap { UnsafePointer<CChar>(strdup($0)) } }
    defer {
      for ptr in cargs where ptr != nil {
        free(UnsafeMutablePointer(mutating: ptr!))
      }
    }
    return mpv_command(handle, &cargs)
  }

  private func makeCArgs(_ command: String, _ args: [String?]) -> [String?] {
    if args.last == nil {
      fatalError("Command do not need a nil suffix")
    }
    var strArgs = args
    strArgs.insert(command, at: 0)
    strArgs.append(nil)
    return strArgs
  }

  /// mpv 事件排空（wakeup 回调进来后在后台队列跑）：只处理与本引擎
  /// 有关的几件——dwidth/dheight 变了记分辨率；播完/播错发事件。
  /// 其它事件（日志等）直接丢掉，不打印。
  private func readEvents() {
    eventQueue.async { [weak self] in
      guard let self = self else { return }
      while let handle = self.mpv {
        guard let event = mpv_wait_event(handle, 0) else { break }
        if event.pointee.event_id == MPV_EVENT_NONE { break }
        switch event.pointee.event_id {
        case MPV_EVENT_PROPERTY_CHANGE:
          guard let property = UnsafePointer<mpv_event_property>(OpaquePointer(event.pointee.data))?.pointee else { continue }
          let name = String(cString: property.name)
          if name == "dwidth" || name == "dheight" {
            guard let value = UnsafePointer<Int64>(OpaquePointer(property.data))?.pointee else { continue }
            DispatchQueue.main.async {
              if name == "dwidth" { self.videoW = Int(value) } else { self.videoH = Int(value) }
            }
          }
        case MPV_EVENT_END_FILE:
          guard let endFile = UnsafePointer<mpv_event_end_file>(OpaquePointer(event.pointee.data))?.pointee else { continue }
          if endFile.reason == MPV_END_FILE_REASON_ERROR {
            DispatchQueue.main.async { self.fireError() }
          } else if endFile.reason == MPV_END_FILE_REASON_EOF {
            DispatchQueue.main.async { self.fireEnded() }
          }
        case MPV_EVENT_SHUTDOWN:
          // 句柄正在销毁：只把引用清掉就退出循环，不再碰它；
          // 真正的销毁只由 close()/deinit 做，不在这里重复 destroy。
          self.mpv = nil
          break
        default:
          break
        }
      }
    }
  }

  private func fireEnded() {
    guard !didFireEnded else { return }
    didFireEnded = true
    eventHandler?(.ended)
  }

  private func fireError() {
    guard !didFireError else { return }
    didFireError = true
    eventHandler?(.error("mpv 播放失败"))
  }
}

#else

final class MPVEngine: NSObject, NativePlayerEngine {
  var eventHandler: ((NativePlayerEvent) -> Void)?
  var videoSize: CGSize { .zero }
  var currentState: NativePlayerState {
    NativePlayerState(
      engine: "mpv", playing: false, position: 0, duration: 0,
      rate: 1, volume: 0, muted: false, videoWidth: 0, videoHeight: 0)
  }

  func open(
    url: URL, position: Double, rate: Double, volume: Double, muted: Bool, title: String?,
    present: @escaping (UIViewController) -> Void,
    completion: @escaping (Result<NativePlayerState, Error>) -> Void
  ) {
    _ = url; _ = position; _ = rate; _ = volume; _ = muted; _ = title; _ = present
    completion(
      .failure(
        NativePlayerUnavailableError(
          message:
            "mpv 未接入：Package.swift 里 MPVKit 0.40.0（LGPL）依赖未生效（可能 SPM 未解析或版本不对），详见 docs/developers/native-players.md")))
  }

  func play() {}
  func pause() {}
  func seek(to position: Double) { _ = position }
  func setRate(_ rate: Double) { _ = rate }
  func setVolume(_ volume: Double, muted: Bool) { _ = volume; _ = muted }
  func close() {}
}

#endif
