#!/usr/bin/env node
/**
 * 把仓库里跟踪的 iOS 图标同步进生成的 Xcode 工程。
 *
 * 为什么需要：
 *   `src-tauri/gen/` 不入库，CI 和换机器都会重新 `tauri ios init`，
 *   而 init 只会写 Tauri 模板自带的默认图标（那个青黄「8」），**不会**读
 *   `src-tauri/icons/ios/`。于是本地手动拷过图标的人看着正常，
 *   CI 打出来的包却是默认图标 —— 与安卓是同一个坑（见 sync-android-icons.mjs）。
 *
 * 注意文件名一一对应：模板 AppIcon.appiconset/Contents.json 里列的 18 个
 * 文件名（AppIcon-20x20@2x.png、AppIcon-512@2x.png……）与
 * `src-tauri/icons/ios/` 里的 18 个文件同名，直接覆盖即可，Contents.json 不动。
 *
 * 用法：
 *   node scripts/sync-ios-icons.mjs
 *   （build-ios-unsigned.sh 在 xcodebuild 之前调用；tauri.conf.json 的
 *   beforeBuildCommand 也调用，本地 `tauri ios build` 同样受益。
 *   必须在 xcodebuild 的资源编译（actool）之前跑，跑晚了当次包不生效。）
 */
import { cpSync, existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const src = join(root, "src-tauri", "icons", "ios");
const dest = join(
  root,
  "src-tauri",
  "gen",
  "apple",
  "Assets.xcassets",
  "AppIcon.appiconset",
);

if (!existsSync(src)) {
  console.log(`[icons] 跳过：缺少 ${src}`);
  process.exit(0);
}
if (!existsSync(dest)) {
  console.log("[icons] 跳过：gen/apple 还没生成（先跑 tauri ios init）");
  process.exit(0);
}

cpSync(src, dest, { recursive: true, force: true });
console.log(`[icons] iOS 图标已同步 → ${dest}`);
