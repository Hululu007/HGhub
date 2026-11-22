#!/usr/bin/env node
/**
 * 把相册“仅添加”使用说明写进生成的 Xcode 工程的 Info.plist：
 *   NSPhotoLibraryAddUsageDescription = 保存已下载的视频到相册
 * 导出到相册（PHPhotoLibrary 存视频）时系统会弹授权框读这句说明；
 * 缺了这个键，首次保存时会直接崩溃。
 *
 * 做法（和 sync-ios-file-sharing.mjs 同一个套路）：在生成的工程里
 * 找到主应用的 Info.plist（以带 UILaunchStoryboardName 区分），已有
 * 该键时不动，没有时在顶层 </dict> 之前插。
 * `src-tauri/gen/` 不入库，CI 每次重新 `tauri ios init` 都会生成新的
 * 默认 Info.plist，所以每次构建前都要跑这个同步。
 *
 * 用法：
 *   node scripts/sync-ios-photo-library.mjs
 *   （build-ios-unsigned.sh 在 xcodebuild 之前调用；tauri.conf.json
 *   的 beforeBuildCommand 也调用。找不到 gen/apple 或主应用 Info.plist
 *   时跳过，不报错，只打警告——导出到相册会失败，但不影响构建。）
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
const KEY = "NSPhotoLibraryAddUsageDescription";
const VALUE = "保存已下载的视频到相册";

if (!existsSync(genApple)) {
  console.log("[photolibrary] 跳过：gen/apple 还没生成（先跑 tauri ios init）");
  process.exit(0);
}

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
  console.log("[photolibrary] 警告：gen/apple 里没找到 Info.plist，本次不改");
  process.exit(0);
}

let patched = 0;
for (const p of plists) {
  let text = readFileSync(p, "utf-8");
  // 只改主应用的 Info.plist：它带 UILaunchStoryboardName（启动屏配置）。
  if (!text.includes("UILaunchStoryboardName")) continue;
  if (text.includes(`<key>${KEY}</key>`)) {
    console.log(`[photolibrary] ${p} 已含 ${KEY}，不用改`);
    patched++;
    continue;
  }
  const m = text.match(/<\/dict>(\s*)<\/plist>/);
  if (!m) {
    console.log(`[photolibrary] 警告：${p} 结构异常，未改 ${KEY}`);
    continue;
  }
  text = text.replace(
    /<\/dict>\s*<\/plist>/,
    `\t<key>${KEY}</key>\n\t<string>${VALUE}</string>\n</dict>${m[1]}</plist>`,
  );
  writeFileSync(p, text);
  console.log(`[photolibrary] ${KEY} 已写入 → ${p}`);
  patched++;
}
if (patched === 0) {
  console.log("[photolibrary] 警告：没找到带 UILaunchStoryboardName 的 Info.plist，本次不改");
}
