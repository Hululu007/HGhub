// swift-tools-version:5.3
// Copyright 2019-2023 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

import PackageDescription

// HGhub iOS 原生插件的 SPM 包：NativePlayerPlugin（native-player）与
// OrientationPlugin（orientation）共用这一个包、同一个 Swift 模块。
// 写法照搬官方 tauri-plugin-dialog 2.8.3 的 ios/Package.swift，只改
// 了包名与 target 名；依赖的 Tauri 模块来自 build.rs 在构建时拷到
// src-tauri/.tauri/tauri-api 的 tauri ios-api（见 src-tauri/build.rs）。
// 本包由 build.rs 经 SwiftLinker（swift build）在 macOS 构建时编成
// 静态库链进 Rust，Xcode 不编译这些文件——不要再把它们拷进
// gen/apple/Sources（那样会与静态库重复定义 init_plugin_* 符号）。
let package = Package(
  name: "NativePlugins",
  platforms: [
    .macOS(.v10_13),
    .iOS(.v13),
  ],
  products: [
    // Products define the executables and libraries a package produces, and make them visible to other packages.
    .library(
      name: "NativePlugins",
      type: .static,
      targets: ["NativePlugins"])
  ],
  dependencies: [
    .package(name: "Tauri", path: "../.tauri/tauri-api"),
    // mpv 软解（LGPL 版产品 `MPVKit`；GPL 版产品 `MPVKit-GPL` 含 Samba，
    // 与 HGhub 的 PolyForm Noncommercial 冲突，不要用）。
    // 钉 0.40.0 exact：0.41.0 起最低 iOS 14.0，1.x 起 iOS 15.0，
    // 本包与应用目前都是 iOS 13.0 底线；升版要先定 iOS floor 再说。
    // 注意 MPVKit 只提供 libmpv/FFmpeg 的 XCFramework，没有 Swift
    // 播放器类——引擎直接调 libmpv C API（见 engines/MPVEngine.swift）。
    .package(url: "https://github.com/mpvkit/MPVKit.git", exact: "0.40.0")
  ],
  targets: [
    // Targets are the basic building blocks of a package. A target can define a module or a test suite.
    // Targets can depend on other targets in this package, and on products in packages this package depends on.
    .target(
      name: "NativePlugins",
      dependencies: [
        .byName(name: "Tauri"),
        .product(name: "MPVKit", package: "MPVKit")
      ],
      path: "Sources")
  ]
)
