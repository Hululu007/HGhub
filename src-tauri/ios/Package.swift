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
    // v0.0.126 诊断版 MPVKit 已摘，10.15 先留着（无害），恢复 MPVKit
    // 时仍要它。
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
    // mpv（MPVKit 0.40.0-xcode，LGPL）：v0.0.126 诊断版暂时摘掉——
    // v0.0.124/125 带 MPVKit 静态链入后启动黑屏（无 .ips、无 JetsamEvent），
    // 125 的 smbc 修法已验在包里仍黑屏，怀疑是 MPVKit 这堆静态库导致
    // 启动挂起；摘掉后若能启动，即证实是它，再想办法把 mpv 接回来
    // （懒加载 / 换链接方式）。摘掉后 MPVEngine 走 #else 空实现（报
    // unavailable），播放内核选 mpv 会提示未接入。恢复办法：把下面
    // 这一段 MPVKit 依赖加回来（url exact "0.40.0-xcode"，target 里
    // 再加 .product(name: "MPVKit", package: "MPVKit")），同时恢复
    // build.rs 的 stage_mpvkit_frameworks 调用和 build-ios-unsigned.sh
    // 的 MPVKIT_* 链接参数。GPL 版产品 `MPVKit-GPL` 含 Samba，与 HGhub
    // 的 PolyForm Noncommercial 冲突，不要用。
  ],
  targets: [
    // Targets are the basic building blocks of a package. A target can define a module or a test suite.
    // Targets can depend on other targets in this package, and on products in packages this package depends on.
    .target(
      name: "NativePlugins",
      dependencies: [
        .byName(name: "Tauri"),
      ],
      path: "Sources")
  ]
)
