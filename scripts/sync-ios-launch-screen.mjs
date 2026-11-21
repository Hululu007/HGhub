#!/usr/bin/env node
/**
 * 把纯黑启动屏同步进生成的 Xcode 工程的系统启动屏。
 *
 * 为什么需要：
 *   点图标后 iOS 先显示系统启动屏（LaunchScreen.storyboard），那是
 *   Tauri 模板的默认屏 —— 纯色底 + 文字。用户不要启动画面了，所以
 *   用仓库里的 scripts/LaunchScreen.storyboard（纯黑、无图）覆盖
 *   生成的默认屏；iOS 仍需一个启动屏，不能直接删掉配置。
 *   网页那层 #splash（index.html 内联）也已删掉，App.tsx 里不再
 *   有移除逻辑，public/splash-logo.png 已删除。
 *
 * 做法（和 sync-ios-icons.mjs 同一个套路）：
 *   用仓库里的 scripts/LaunchScreen.storyboard（纯黑、无图）覆盖
 *   生成的启动屏。不再拷任何 logo 资源。
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
  readdirSync,
} from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const genApple = join(root, "src-tauri", "gen", "apple");
const storyboardSrc = join(root, "scripts", "LaunchScreen.storyboard");

if (!existsSync(genApple)) {
  console.log("[launch] 跳过：gen/apple 还没生成（先跑 tauri ios init）");
  process.exit(0);
}
if (!existsSync(storyboardSrc)) {
  console.log("[launch] 跳过：缺少启动屏模板");
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

// 启动屏本身：用仓库里的纯黑无图版覆盖生成的默认屏。
cpSync(storyboardSrc, dest);
console.log(`[launch] 启动屏已同步 → ${dest}`);
