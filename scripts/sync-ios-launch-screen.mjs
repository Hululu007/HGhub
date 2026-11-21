#!/usr/bin/env node
/**
 * 把红苹果启动图同步进生成的 Xcode 工程的系统启动屏。
 *
 * 为什么需要：
 *   点图标后 iOS 先显示系统启动屏（LaunchScreen.storyboard），那是
 *   Tauri 模板的默认屏 —— 纯色底 + 文字，没有红苹果。之后 WebView
 *   加载，网页启动画面 #splash 才出现，但它只在 WebView 加载后、
 *   首渲染 +900ms 内可见；实测（v0.0.17）用户冷启动也看不到苹果，
 *   原因是 WebView 加载和 React 渲染发生在系统启动屏还盖着屏幕的
 *   那段时间里 —— 等启动屏撤掉、用户能看到 WebView 时，900ms 早
 *   就走完了。所以苹果必须上系统启动屏本身，那是启动时唯一保证
 *   能被看到的画面。
 *
 * 做法（和 sync-ios-icons.mjs 同一个套路）：
 *   1. 把 public/splash-logo.png（透明底红苹果，1024）拷进
 *      Assets.xcassets/LaunchLogo.imageset/，当作启动图资源；
 *   2. 用仓库里的 scripts/LaunchScreen.storyboard（黑底 + 居中
 *      240pt 的 LaunchLogo 图）覆盖生成的启动屏。
 *   `src-tauri/gen/` 不入库，CI 每次重新 `tauri ios init` 都会
 *   生成新的默认启动屏，所以每次构建前都要跑这个同步。
 *
 * 用法：
 *   node scripts/sync-ios-launch-screen.mjs
 *   （build-ios-unsigned.sh 在 xcodebuild 之前调用；tauri.conf.json
 *   的 beforeBuildCommand 也调用。必须在 ibtool 编译启动屏之前跑，
 *   跑晚了当次包不生效。找不到 gen/apple 或启动屏文件时跳过，不报错。）
 */
import {
  cpSync,
  existsSync,
  mkdirSync,
  readdirSync,
  writeFileSync,
} from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const genApple = join(root, "src-tauri", "gen", "apple");
const storyboardSrc = join(root, "scripts", "LaunchScreen.storyboard");
const logoSrc = join(root, "public", "splash-logo.png");
const xcassets = join(genApple, "Assets.xcassets");

if (!existsSync(genApple)) {
  console.log("[launch] 跳过：gen/apple 还没生成（先跑 tauri ios init）");
  process.exit(0);
}
if (!existsSync(storyboardSrc) || !existsSync(logoSrc)) {
  console.log("[launch] 跳过：缺少启动屏模板或 logo 文件");
  process.exit(0);
}

// 生成的工程里启动屏的位置可能因模板版本而异，递归找它。
function findStoryboard(dir) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const p = join(dir, entry.name);
    if (entry.isDirectory()) {
      if (entry.name.endsWith(".xcodeproj")) continue;
      const found = findStoryboard(p);
      if (found) return found;
    } else if (entry.name === "LaunchScreen.storyboard") {
      return p;
    }
  }
  return null;
}

const dest = findStoryboard(genApple);
if (!dest) {
  console.log("[launch] 警告：gen/apple 里没找到 LaunchScreen.storyboard，本次不改启动屏");
  process.exit(0);
}

// 1. 启动图资源：Assets.xcassets/LaunchLogo.imageset/
const imageSet = join(xcassets, "LaunchLogo.imageset");
mkdirSync(imageSet, { recursive: true });
cpSync(logoSrc, join(imageSet, "LaunchLogo.png"));
writeFileSync(
  join(imageSet, "Contents.json"),
  JSON.stringify(
    {
      images: [{ filename: "LaunchLogo.png", idiom: "universal" }],
      info: { author: "xcode", version: 1 },
    },
    null,
    2,
  ) + "\n",
);

// 2. 启动屏本身：用仓库里的黑底红苹果版覆盖生成的默认屏。
cpSync(storyboardSrc, dest);
console.log(`[launch] 启动屏已同步 → ${dest}`);
console.log(`[launch] 启动图资源已同步 → ${imageSet}`);
