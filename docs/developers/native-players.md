# iOS 原生播放器

WebView / HTML5 `<video>` + hls.js 仍是默认播放路径（`src/components/VideoSurface.tsx`
一动不动）。本文档只讲新增的四条原生路：AVPlayer、MDK、mpv、KSPlayer。
它们只在用户选了非 WebView 引擎且平台是 iOS 时启用，失败自动回落 WebView。

> 状态：桥已搭好，未在设备验证。**本机是 Linux，没有 Xcode / Swift 工具链，
> iOS 原生代码一行都没编译过**，也没有跑过真机。下面“Mac 验证”一节是必须
> 走完的清单，不要跳过。

## 架构

```text
React（Feed.tsx 更多播放选项菜单里的“原生播放”入口）
  → src/plugins/nativePlayer.ts（invoke 封装 + addPluginListener 事件）
  → Rust 命令 src-tauri/src/native_player.rs
      native_player_open / close / play / pause / seek / set_rate / set_volume / state
  → Tauri 移动插件 "native-player"（lib.rs 注册）
  → Swift src-tauri/ios/Sources/NativePlayerPlugin.swift（统一 NativePlayerEngine 协议）
  → src-tauri/ios/Sources/engines/ 下四个引擎适配（原生全屏 present，退出时发回事件）
  → 前端收到 native-player-time / -ended / -error / -closed 后回写进度、回落 WebView
```

取流：四个引擎都直接播 Rust `media.rs` 给的本地回环地址
`http://127.0.0.1:{port}/hpp/{id}/{index}?v={version}`，上游头、Range、
密钥都由 Rust 代理处理，原生侧不拼头、不改地址、不碰 HPP 会话逻辑。
`PreparedMedia` 的形状、会话 / 刷新 / 资源归属一律没改（协议 09 章不动）。

编译方式（SPM + SwiftLinker，官方插件同款）：`src-tauri/ios/` 是
一个 SPM 包（`Package.swift`，包名 `NativePlugins`，含
`NativePlayerPlugin` 与 `OrientationPlugin` 两个插件、同一个 Swift
模块）。`src-tauri/build.rs` 在 macOS 上为 iOS target 构建时，先把
tauri 的 iOS Swift API（`DEP_TAURI_IOS_LIBRARY_PATH`）拷到
`src-tauri/.tauri/tauri-api`（构建产物，不进版本控制），再调
`tauri_utils::build::link_apple_library("NativePlugins", ios-dir)`，
由 swift-rs 以 `swift build` 编成静态库链进 Rust。**Xcode 不编译
这些文件**——不要再把它们拷进 `gen/apple/Sources`，那样会与静态
库重复定义 `init_plugin_*` 符号。v0.0.8 之前走的就是拷进 Xcode 的
路线，死在 `import Tauri`：Tauri 的 iOS Xcode 模板里根本没有 Tauri
Swift 模块，无论是否重跑 xcodegen 都编不过（v0.0.5–v0.0.8 的教训：
Rust 编不过 → 同步脚本 → xcodegen 重生成 → cdylib 链接缺符号 →
Xcode 下 `import Tauri` 找不到模块）。

链接说明：`src-tauri/Cargo.toml` 的 `[lib] crate-type` 含 `cdylib`，
而 Swift 插件符号只由 SwiftLinker 编 Swift 包时才产生，cargo 在
Xcode 的 Build Rust Code 阶段链接 cdylib 时它们还不存在。仓库根目
录的 `.cargo/config.toml` 给两个 iOS target 加了
`-C link-args=-Wl,-undefined,dynamic_lookup`，让这次链接允许未定义
符号（最终 App 链接的是 staticlib + SwiftLinker 编出的 Swift 对象，
符号在那里解析；cdylib 在 iOS 上只是构建副产物）。Android / 桌面
构建不受影响。Swift 包里两个插件的 Swift 入口函数名必须不同
（`initNativePlayerPlugin` / `initOrientationPlugin`），`@_cdecl` 的
C 符号名（`init_plugin_native_player` / `init_plugin_orientation`）
保持不动——同一模块内同名同签名是非法重声明。

## 引擎选择

存在 store 的 `playbackEngine`（localStorage `hpp.playbackEngine`），
默认 `"webview"`。可选：`webview`、`avplayer`、`mdk`、`ksplayer`、`mpv`、
`auto`。设置页已接：`src/plugins/ApplicationSettings.tsx` 的“播放”区有
“播放内核”选择器，只在 iOS 显示，非 iOS 平台不显示（原生引擎只支持 iOS）。

`auto` 的 fallback 链（`src/plugins/playbackEngine.ts`，纯函数）：
AVPlayer → MDK → KSPlayer → mpv → 回落 WebView。
指定某个引擎时只试它，失败也回落 WebView。

选引擎有两个入口，共用同一个 store（`playbackEngine` /
`setPlaybackEngine`，与设置页选择器同一套）：设置页“播放”区的
“播放内核”选择器，和播放页“更多播放选项”菜单里的“播放内核”
选择行。菜单只在 iOS 显示；选了新引擎不切当前播放，下次点菜单里
的“原生播放”时才按新引擎打开。播放画面右上角不再放“原生播放”
pill（v0.0.22 删掉），入口只在更多菜单里。

## 四个引擎

| 引擎 | 格式 | 许可证（发行前必读） | 接入方式 | 已知限制 |
| --- | --- | --- | --- | --- |
| AVPlayer | H.264/HEVC + MP4/HLS | 系统自带，无许可证问题 | 零依赖，`AVPlayerViewController` 全屏 | 不吃 MKV/FLV/WebM/VP9/AV1，不支持 DASH，ASS 只能走 HLS 内嵌字幕 |
| MDK | FFmpeg 底，格式宽，ASS 需可选 `ass.framework` | key 制：开源 / 非商业免费，找作者申请 key；无 key 且 SDK 过期时最后一帧可能出现二维码水印 | CocoaPods `pod 'mdk'` 或 SPM `swift-mdk` | key 要另外申请；`setDecoders` 目前配 `VT / FFmpeg / dav1d` |
| KSPlayer | AVPlayer + FFmpeg，HDR / Dolby Vision / Atmos 好 | **默认 GPL-3.0，要求开源自家代码**；HGhub 是 PolyForm Noncommercial，**不能直接按 GPL 发行**；完整格式 / 全量 ASS / AV1 硬解 / FFmpeg 8.1 在付费 LGPL 版（联系 kingslay@icloud.com），免费 GPL 版是 FFmpeg 6.1.0 且功能受限 | SPM `kingslay/KSPlayer`，`KSOptions.secondPlayerType = KSMEPlayer.self` 切 FFmpeg 路 | 没拿到付费 LGPL 之前不要用它发正式包 |
| mpv（libmpv） | 格式 / ASS 最强 | 选 MPVKit 的 LGPL 版；GPL 版含 Samba 等 GPL 组件，发行口径与 PolyForm Noncommercial 冲突 | 无官方现成 iOS 包：要在 Mac 上编 `Libmpv.xcframework`（FFmpeg、libass、libplacebo、dav1d 整条链），不能从 Linux 交叉编；或用 `mpvkit/MPVKit` SPM（LGPL 版） | 最难编，包体最大 |

当前实现状态：

- **AVPlayer 已完整实现**：`engines/AVPlayerEngine.swift`，起播、续播、进度观察、
  结束 / 失败回调、Done 关闭回传都有。
- **MDK / KSPlayer / mpv 是 `canImport` 守卫**：依赖装了走 `#if` 分支的真实调用，
  没装走 `#else` 分支——`open` 直接失败，报错里写清缺哪个依赖、怎么装，
  不留“能打开但播不了”的空实现。`#if` 分支里的方法名是按各家 README 所写，
  第一次在 Mac 上编时以接入的依赖版本为准在构建时校验（见下）。

## Mac 验证（按顺序）

```bash
# 1. 在 Mac 上取代码（fork / 分支同底部白条补丁的流程）
git clone <你的 fork> HGhub && cd HGhub
pnpm install --frozen-lockfile

# 2. 生成 iOS 工程（Swift 插件不用同步，build.rs 在构建时自己处理）
pnpm tauri ios init --ci
# Swift 插件是 SPM 包：cargo 构建时 build.rs 把 tauri 的 iOS Swift API
# 拷到 src-tauri/.tauri/tauri-api，再经 SwiftLinker 编成静态库链进
# Rust。不要把 Swift 文件拷进 gen/apple/Sources，也不要重跑 xcodegen
# 收录它们——那样会与静态库重复定义 init_plugin_* 符号。

# 3. 按要测的引擎装依赖（AVPlayer 零依赖，跳过这步就能先测桥）
#    MDK：CocoaPods pod 'mdk' 或 SPM swift-mdk，并向作者申请 key
#    KSPlayer：SPM kingslay/KSPlayer（先确认 LGPL 授权，见上表）
#    mpv：在 Mac 上编 Libmpv.xcframework，或 SPM 加入 MPVKit（LGPL 版）

# 4. 打未签名 IPA（仓库现有脚本，不用改）
bash scripts/build-ios-unsigned.sh 0.0.4

# 5. 真机（TrollStore 或自签名）安装后逐项测：
#    - 设置 playbackEngine 为 avplayer，播 HLS/MP4，确认全屏、进度、Done 后回落 WebView
#    - 播一个 AVPlayer 不支持的格式，确认 auto 链 fallback 到下一个引擎或 WebView
#    - 再逐个测 MDK / KSPlayer / mpv（各依赖装好后）
#    - 方向：播横屏视频（16:9）自动转横屏，播竖屏视频自动回竖屏；
#      点控制条的旋转按钮手动切到反面，本集内不再自动改回；切集后
#      回到按分辨率自动；退出播放页后恢复竖屏。手动旋转成功时应有
#      “已切换到横屏/竖屏”提示，失败时应有“无法切换屏幕方向”提示。
#      若仍不转：先确认 `src-tauri/gen/apple` 里窗口根视图控制器
#      有没有重写 supportedInterfaceOrientations 限制方向（本地
#      查不到，gen 目录是 CI 生成的、不进版本控制），再看 Swift
#      侧 requestGeometryUpdate 是否被 reject。
```

## 本机能验证的

- `pnpm run build`（tsc + vite）通过。
- `cargo check --manifest-path src-tauri/Cargo.toml` 通过（host check；
  `cfg(target_os = "ios")` 的代码只要求语法解析通过）。
- `cargo check --target aarch64-linux-android` 通过（build.rs 里
  macOS 以外的部分、拷贝辅助函数、Cargo.toml 的新 build-dependency
  都能在这步验证；macOS 专属的 SwiftLinker 调用是 `#[cfg]` 门控的，
  Linux 上编不出来）。
- Swift 文件**未编译**（本机无 swiftc / Xcode），上面 Mac 清单跑完之前，
  不要写“iOS 编译通过”或“真机验证通过”。

## 分辨率显示（播放时）

- WebView 路：`VideoSurface` 的 `onSize` 已回传 `videoWidth/videoHeight`，
  `Feed.tsx` 在播放画面标题栏下方叠一个角标（`1920×1080` 这种格式，
  顶边 + 安全区 + 96px），拿到尺寸才显示，未知时隐藏；起播后显示
  3 秒自动消失，切集重开一个 3 秒窗口。首页预览卡片上右上角的
  “原生播放”pill 已删（v0.0.22），角标仍是左上角那一个，3 秒行为不变。
- 详情页（full 模式，仅 iOS，v0.0.25）：同一位置改成左右两个角标，
  分辨率在左、当前播放内核在右（store 的 `playbackEngine`，即设置页/
  更多菜单里选的那个），同款式、左右按横向安全区（`--sal`/`--sar`）
  留白。显隐跟控制条同显同隐（移动端清屏或锁定即隐藏，桌面闲置
  即隐藏，暂停/面板打开时仍显示），起播/切集后的 3 秒窗口内也短暂
  显示一次。首页预览卡片的 3 秒角标与清屏浮层不受影响。
- 原生路：`NativePlayerState` 与 `native-player-time` 事件都带
  `videoWidth/videoHeight`（0 表示未知）；`AVPlayerEngine` 用
  `AVPlayerItem.presentationSize` 取真实尺寸，在
  `AVPlayerViewController.contentOverlayView` 右上角下方（顶边 +72px）
  叠同款角标，每 0.5s 的进度回调里刷新一次，HLS 切清晰度时能跟上；
  同样起播后显示 3 秒自动消失。
  MDK / KSPlayer / mpv 的 `videoSize` 目前返回 `.zero`（未接各家
  的尺寸 API），角标按未知隐藏，等 Mac 上接依赖时再按各家 API 补。
- 真机验收时加一项：播 HLS/MP4 看角标是否出现、数值是否与实际
  清晰度一致、3 秒后是否自动消失；切集后角标是否跟着变。

## 首页清屏信息浮层（v0.0.23，仅 iOS 预览卡片）

- 手势：单击切换清屏（隐藏/恢复全部界面控件，只留视频），双击在
  非清屏时切换暂停/播放；**已清屏时双击改为唤醒/关闭信息浮层，
  不暂停播放**。单击的 250ms 延迟计时器在第二下点按时被取消，
  所以一次双击不会触发两次单击的清屏切换。播放详情页（full
  模式）双击仍是暂停/播放，不受影响。
- 浮层内容（`Feed.tsx`，与 3 秒角标同款式，顶边 + 安全区 + 96px）：
  左边是分辨率（与 3 秒角标同一位置），右边对称显示当前播放内核
  （store 的 `playbackEngine`，即设置页/更多菜单里选的那个）；
  下方一行只有“复制播放地址”按钮（v0.0.24：地址文字本身不再显示，
  点按复制当前视频的真实播放地址 `media.url`，成功提示“已复制，切出
  应用后地址会失效”）；旁边一个旋转按钮，复用 v0.0.22 的方向机制
  与“已切换到横屏/竖屏”提示。竖屏时两行左右按横向安全区（`--sal`/
  `--sar`，`App.tsx` 定义，竖屏时等于 12px）留白；横屏时左边两行改取
  固定 230px：v0.0.24 以为横屏刘海侧安全区约 44px 是错的——实测本机
  WKWebView 里横屏的 `env(safe-area-inset-left)` 是 0，`--sal` 只剩
  12px，角标仍贴在左边缘（v0.0.28 真机截图测量：分辨率文字左沿约
  25pt，pill 左沿约 13pt）；230px 是用户横屏截图上红框的左沿位置，
  从该位置起落在视频区内。右边内核 pill 不动，仍按 `--sar` 留白。
- 显隐：双击唤醒，再双击关闭；15 秒无操作自动消失（复制地址或
  点旋转会续 15 秒，3 秒不够点到复制）；切集、退出清屏、切模式时
  关闭。浮层打开时 3 秒角标隐藏，避免两者在左上角同一位置重叠。
- 复制：`navigator.clipboard.writeText`（WKWebView 里需用户手势，
  点按即满足），不可用时走 textarea + `execCommand("copy")` 兜底，
  失败提示“复制失败”，不静默。复制的地址是本机回环地址
  （`http://127.0.0.1:<端口>/hpp/<会话>/<资源>?v=<版本>`，`media.rs`
  的 `MediaSessions::start` 在应用内起的本地服务器），只在应用
  处于前台时可用：切到其他应用后 iOS 会挂起本应用进程，服务器停
  止响应，浏览器里粘贴打开会一直白屏加载（v0.0.28 用户录像证实），
  协议也不以持续后台执行为前提（见 `09-media-transport.md`
  §7）。所以复制成功的提示写明“切出应用后地址会失效”，不要让人
  以为这是个能随处打开的普通链接。浮层容器本身不接指针事件，只有
  复制行与旋转按钮可点，其余点按仍走视频手势。
- 真机验收：首页卡片单击清屏后双击，浮层出现，左分辨率、右内核、
  “复制播放地址”按钮可点复制（粘贴验证地址与当前播放地址一致，
  不再显示 URL 文字），旋转按钮能切横竖屏；再双击关闭；15 秒后
  自动消失；单击退出清屏时浮层跟着关；切到下一条卡片浮层不残留；
  详情页双击仍是暂停/播放。横屏下浮层左边两行从约 230pt 起（红框
  位置），分辨率 pill 与复制按钮、旋转按钮完整可见，不贴左边缘；
  竖屏下位置与原来一致。

## 影片信息（详情页，v0.0.25）

- 入口：详情页右上角“•••”→“更多播放选项”→“影片信息”，打开
  影片信息面板（复用已有的面板系统：移动端底部弹层、桌面右侧
  停靠面板，返回手势/Esc 关闭；`onPanelChange` 不上报 `info`，
  父级只关心 `detail`/`episodes`）。
- 内容：分辨率（`vsize`，未知时写“分辨率未知”）、格式、资源播放
  地址（`media.url`）+“复制播放地址”按钮（复用 `copyPlayUrl` 的
  clipboard + 兜底，成功提示“已复制，切出应用后地址会失效”——地址
  是应用内回环服务，只在前台可用，见上面浮层一节）。注意：这里
  地址文字是显示出来的（用户明确要求），与首页浮层 v0.0.24“只留
  复制按钮、不显示地址”不同，不要把两处搞混。
- 横屏：面板是全宽弹层，内容贴左沿，v0.0.29 起横屏时整个面板内容
  右移 330px（`Feed.tsx` 里弹层 div 上 `panel === "info" &&
  vpLandscape` 才加 `paddingLeft: 330`，只动 info 面板，其他面板
  与竖屏不受影响）；330px 是用户横屏截图上红框的左沿位置。
- 格式来源：`media.format` 是 `hls` 标 HLS；是 `progressive` 时从
  播放地址后缀推导（`.m3u8` → HLS、`.mp4` → MP4，另有 mkv/webm/
  mov/m4v/ts 的映射），拿不到后缀只写“未知”，不编造编码信息。
- 真机验收：详情页点 ••• → 影片信息，窗口弹出，含分辨率/格式/
  地址三行；点复制出现“已复制，切出应用后地址会失效”，粘贴核对
  地址与当前在播的一致；横屏下标题、标签、地址、复制按钮从约
  330pt 起（红框位置），不在左沿；首页浮层与旋转无回归。

## 横屏播放时隐藏 iOS 状态栏（v0.0.29）

- 行为：`Feed.tsx` 里当前播放卡激活且屏幕横置（`vpLandscape &&
  active`）时调原生命令隐藏状态栏，时间、电量、网速等系统信息
  不再压在视频上方；回竖屏或离开播放页时恢复（effect 的 cleanup
  里恢复，卸载时也恢复）。只在 iOS 上调用，其他平台没有这条命令。
- 机制：`OrientationPlugin.setStatusBarHidden`（`src-tauri/ios/
  Sources/OrientationPlugin.swift`，与方向切换同一个插件）走
  `UIApplication.shared.isStatusBarHidden`。默认 Info.plist 没有
  `UIViewControllerBasedStatusBarAppearance`（即按视图控制器），
  这时改 UIApplication 会被忽略，所以 `scripts/
  sync-ios-status-bar.mjs` 在 CI 生成工程后把它写成 `<false/>`
  （只改带 `UILaunchStoryboardName` 的主应用 Info.plist；找不到
  时打警告、不报错）；`build-ios-unsigned.sh` 与 `tauri.conf.json`
  的 `beforeBuildCommand` 都调它。Rust 命令是
  `commands::set_status_bar_hidden`（`lib.rs` 已注册），前端包在
  `src/statusBar.ts`，失败静默、不影响播放。
- 真机验收：横屏播视频时顶部时间/电量/网速都不见了；转回竖屏
  立即恢复；退出播放页、切后台再回来，状态栏状态与方向一致；
  竖屏下状态栏始终可见。

## 播放方向（横竖屏）

- 自动：`Feed.tsx` 拿到视频尺寸后按分辨率选方向，横屏视频（宽>高）
  调 `set_orientation(true)`，竖屏视频调 `set_orientation(false)`，
  不再只在全屏时才转；切集后回到自动。
- 手动：移动端控制条（锁定、全屏之间）多了一个旋转按钮，点一下切
  到当前方向的反面，并覆盖本集的自动选择；退出播放页时恢复竖屏。
  “当前方向”必须在点按时读实时视口尺寸（`window.innerWidth >
  window.innerHeight`），不能读 `vpLandscape` state——程序触发
  旋转后 resize 事件可能不来、或来时尺寸还是旧的，state 就一直
  停在竖屏，每次点都继续要求横屏、切不回竖屏（v0.0.23 录像里
  每次都提示“已切换到横屏”就是这个症状，v0.0.24 改的就是这个）；
  `vpLandscape` 只留给布局（全屏/画面比例/控制条位置）用。自动
  转方向没生效时屏幕还是竖屏、视频却是横屏，用视频方向算会得出
  “当前已是横屏”，点一下反而要求切回竖屏，看起来就像按钮没反应
  （v0.0.21 修的就是这个）。
- iOS 方向控制是新加的原生插件 `src-tauri/ios/Sources/OrientationPlugin.swift`
  与播放器插件同属 `NativePlugins`
  SPM 包，前端 `src/orientation.ts`
  在 iOS 上也走 Rust 命令 `set_orientation`——iOS 的 WKWebView 不支持
  Screen Orientation API 的 `lock()`，之前 iOS 走浏览器分支会直接失败。
- 机制（v0.0.22）：iOS 16+ 走
  `UIWindowScene.requestGeometryUpdate(.iOS(interfaceOrientations:))`，
  等 0.5s 后按场景实际方向验收，方向对上才 resolve，对不上 reject；
  iOS 15 没有这个 API，才走 `UIDevice.setValue(forKey: "orientation")` +
  `attemptRotationToDeviceOrientation()` 的 KVC 老路（`#available`
  守卫，两条路按系统版本二选一，不叠加）。v0.0.21 及之前全量走 KVC，
  两段用户录像里自动与手动切换从没生效过，且旧实现调完无条件
  resolve，前端分不清成功失败——这是 v0.0.22 换机制的原因。
  手动旋转成功时提示“已切换到横屏/竖屏”，失败时提示“无法切换屏幕
  方向，请重试”；自动切换失败仍静默。
- 注意：`setOrientation` 是“设置一次方向”，不是“锁定”——调完之后
  用户再转手机，系统仍可按设备方向转回来；AppDelegate 级的方向
  锁定没做。另外 iOS 上 `set_orientation(false)` 是切回竖屏，与
  Android 的“恢复之前方向”语义不同。
