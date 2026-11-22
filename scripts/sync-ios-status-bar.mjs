#!/usr/bin/env node
/**
 * 把“状态栏改由 UIApplication 控制”写进生成的 Xcode 工程的 Info.plist。
 *
 * 为什么需要：
 *   横屏播放时要隐藏 iOS 状态栏（时间、电量、网速等系统信息不再压在
 *   视频上方），用的是 OrientationPlugin.setStatusBarHidden，走
 *   UIApplication.shared.isStatusBarHidden。默认情况下 Info.plist
 *   没有 UIViewControllerBasedStatusBarAppearance（即按视图控制器），
 *   这时改 UIApplication 会被忽略，状态栏照常显示；只有把它设成
 *   false，UIApplication 的开关才生效。
 *
 * 做法（和 sync-ios-icons.mjs / sync-ios-launch-screen.mjs 同一个套路）：
 *   在生成的工程里找到主应用的 Info.plist（以带 UILaunchStoryboardName
 *   区分，扩展目标的 plist 没有启动屏配置），把
 *   UIViewControllerBasedStatusBarAppearance 写成 <false/>；已有该键
 *   时只改值，不重复插。`src-tauri/gen/` 不入库，CI 每次重新
 *   `tauri ios init` 都会生成新的默认 Info.plist，所以每次构建前
 *   都要跑这个同步。
 *
 * 用法：
 *   node scripts/sync-ios-status-bar.mjs
 *   （build-ios-unsigned.sh 在 xcodebuild 之前调用；tauri.conf.json
 *   的 beforeBuildCommand 也调用。Info.plist 在构建时读，跑晚了当次
 *   包不生效。找不到 gen/apple 或主应用 Info.plist 时跳过，不报错，
 *   只打警告——状态栏隐藏会失效，但不影响构建与播放。）
 */
import {
  existsSync,
  readFileSync,
  readdirSync,
  writeFileSync,
} from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const genApple = join(root, "src-tauri", "gen", "apple");
const KEY = "UIViewControllerBasedStatusBarAppearance";

if (!existsSync(genApple)) {
  console.log("[statusbar] 跳过：gen/apple 还没生成（先跑 tauri ios init）");
  process.exit(0);
}

// 生成的工程里 Info.plist 的位置可能因模板版本而异，递归找。
function findPlists(dir, out = []) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const p = join(dir, entry.name);
    if (entry.isDirectory()) {
      if (entry.name.endsWith(".xcodeproj")) continue;
      findPlists(p, out);
    } else if (entry.name === "Info.plist") {
      out.push(p);
    }
  }
  return out;
}

const plists = findPlists(genApple);
if (plists.length === 0) {
  console.log("[statusbar] 警告：gen/apple 里没找到 Info.plist，本次不改状态栏");
  process.exit(0);
}

let patched = 0;
for (const p of plists) {
  let text = readFileSync(p, "utf-8");
  // 只改主应用的 Info.plist：它带 UILaunchStoryboardName（启动屏配置）。
  if (!text.includes("UILaunchStoryboardName")) continue;
  if (text.includes(`<key>${KEY}</key>`)) {
    if (new RegExp(`<key>${KEY}</key>\\s*<false/>`).test(text)) {
      console.log(`[statusbar] ${p} 已是 false，不用改`);
      patched++;
      continue;
    }
    const next = text.replace(
      new RegExp(`(<key>${KEY}</key>\\s*)<true/>`),
      "$1<false/>",
    );
    if (next === text) {
      console.log(`[statusbar] 警告：${p} 已有 ${KEY} 但值不是 <true/>，未改`);
      continue;
    }
    text = next;
  } else {
    // 没有该键：插到顶层 </dict> 之前（即 </dict> 与 </plist> 之间）。
    const m = text.match(/<\/dict>(\s*)<\/plist>/);
    if (!m) {
      console.log(`[statusbar] 警告：${p} 结构异常，未改`);
      continue;
    }
    text = text.replace(
      /<\/dict>\s*<\/plist>/,
      `\t<key>${KEY}</key>\n\t<false/>\n</dict>${m[1]}</plist>`,
    );
  }
  writeFileSync(p, text);
  patched++;
  console.log(`[statusbar] 状态栏已交由 UIApplication 控制 → ${p}`);
}
if (patched === 0) {
  console.log("[statusbar] 警告：没找到带 UILaunchStoryboardName 的 Info.plist，本次不改状态栏");
}
