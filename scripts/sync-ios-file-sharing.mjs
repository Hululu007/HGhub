#!/usr/bin/env node
/**
 * 把“文件共享”写进生成的 Xcode 工程的 Info.plist，让下载到
 * 文稿/Downloads 的文件能在“文件”App 里看到（“我的 iPhone”→
 * 红苹果影视 → Downloads），不用每次都走分享面。
 *
 * 做法（和 sync-ios-background-audio.mjs 同一个套路）：在生成的工程里
 * 找到主应用的 Info.plist（以带 UILaunchStoryboardName 区分，扩展
 * 目标的 plist 没有启动屏配置），加两个布尔键：
 *   UIFileSharingEnabled = true（文稿目录暴露给“文件”App）
 *   LSSupportsOpeningDocumentsInPlace = true（允许在原位置打开）
 * 已有该键时不重复插。`src-tauri/gen/` 不入库，CI 每次重新
 * `tauri ios init` 都会生成新的默认 Info.plist，所以每次构建前
 * 都要跑这个同步。
 *
 * 用法：
 *   node scripts/sync-ios-file-sharing.mjs
 *   （build-ios-unsigned.sh 在 xcodebuild 之前调用；tauri.conf.json
 *   的 beforeBuildCommand 也调用。Info.plist 在构建时读，跑晚了当次
 *   包不生效。找不到 gen/apple 或主应用 Info.plist 时跳过，不报错，
 *   只打警告——下载仍可用分享面拿出来，只是不进“文件”App。）
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
const KEYS = ["UIFileSharingEnabled", "LSSupportsOpeningDocumentsInPlace"];

if (!existsSync(genApple)) {
  console.log("[fileshare] 跳过：gen/apple 还没生成（先跑 tauri ios init）");
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
  console.log("[fileshare] 警告：gen/apple 里没找到 Info.plist，本次不改文件共享");
  process.exit(0);
}

let patched = 0;
for (const p of plists) {
  let text = readFileSync(p, "utf-8");
  // 只改主应用的 Info.plist：它带 UILaunchStoryboardName（启动屏配置）。
  if (!text.includes("UILaunchStoryboardName")) continue;
  let changed = false;
  for (const key of KEYS) {
    if (text.includes(`<key>${key}</key>`)) {
      console.log(`[fileshare] ${p} 已含 ${key}，不用改`);
      continue;
    }
    // 没有该键：插到顶层 </dict> 之前（即 </dict> 与 </plist> 之间）。
    const m = text.match(/<\/dict>(\s*)<\/plist>/);
    if (!m) {
      console.log(`[fileshare] 警告：${p} 结构异常，未改 ${key}`);
      continue;
    }
    text = text.replace(
      /<\/dict>\s*<\/plist>/,
      `\t<key>${key}</key>\n\t<true/>\n</dict>${m[1]}</plist>`,
    );
    changed = true;
  }
  if (changed) {
    writeFileSync(p, text);
    console.log(`[fileshare] 文件共享已开启 → ${p}`);
  }
  patched++;
}
if (patched === 0) {
  console.log("[fileshare] 警告：没找到带 UILaunchStoryboardName 的 Info.plist，本次不改文件共享");
}
