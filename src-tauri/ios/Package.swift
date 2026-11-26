// swift-tools-version:5.6
// （5.3 是 Tauri 模板原值；mpv 依赖的 `.package(url:exact:)` 要
// PackageDescription 5.6 才有，5.3 下 CI 报 'package(url:exact:)'
// is unavailable，见 v0.0.117 的 iOS 构建日志。）
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
    // macOS 底线 10.15：MPVKit 要求 macOS 10.15，10.13 下 SPM 直接
    // 报错（见 v0.0.119 构建日志）；iOS 底线仍是 13，不动。
    .macOS(.v10_15),
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
    // 钉 0.40.0-xcode exact：0.40.0 的 Libuavs3d / Libuchardet 清单
    // 校验和已过期（子仓库重发了 1.2.1 / 0.0.8 的包，清单没跟上；
    // 实测 sha256 与 CI 算出的一致、与清单不符，重跑也一样失败）。
    // -xcode 变体 38 个产物全是 -xcode 后缀、成套的，坏掉的那两个
    // 校验和已核对无误；mpv 仍是 0.40.0，iOS 底线仍是 13。
    // 0.41.0 起最低 iOS 14.0，1.x 起 iOS 15.0，升版要先定 iOS floor。
    // 注意 MPVKit 只提供 libmpv/FFmpeg 的 XCFramework，没有 Swift
    // 播放器类——引擎直接调 libmpv C API（见 engines/MPVEngine.swift）。
    .package(url: "https://github.com/mpvkit/MPVKit.git", exact: "0.40.0-xcode")
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
