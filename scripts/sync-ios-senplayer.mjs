#!/usr/bin/env node
/**
 * 把 SenPlayer 登记进生成的 Xcode 工程的 Info.plist 的
 * LSApplicationQueriesSchemes，让 Swift 侧 canOpenURL("senplayer://…")
 * 能判出 SenPlayer 是否装了（iOS 9+ 起，不登记的 scheme canOpenURL
 * 永远返回 false，跳转就会永远报“未安装”）。
 *
 * 做法（和 sync-ios-file-sharing.mjs 同一个套路）：在生成的工程里
 * 找到主应用的 Info.plist（以带 UILaunchStoryboardName 区分），已有
 * 该键且已含 senplayer 时不动；已有该键但缺 senplayer 时往数组里加
 * 一项；没有该键时在顶层 </dict> 之前插整个键。
 * `src-tauri/gen/` 不入库，CI 每次重新 `tauri ios init` 都会生成新的
 * 默认 Info.plist，所以每次构建前都要跑这个同步。
 *
 * 用法：
 *   node scripts/sync-ios-senplayer.mjs
 *   （build-ios-unsigned.sh 在 xcodebuild 之前调用；tauri.conf.json
 *   的 beforeBuildCommand 也调用。找不到 gen/apple 或主应用 Info.plist
 *   时跳过，不报错，只打警告——跳转会报未安装，但不影响构建。）
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

if (!existsSync(genApple)) {
  console.log("[senplayer] 跳过：gen/apple 还没生成（先跑 tauri ios init）");
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
  console.log("[senplayer] 警告：gen/apple 里没找到 Info.plist，本次不改");
  process.exit(0);
}

let patched = 0;
for (const p of plists) {
  let text = readFileSync(p, "utf-8");
  // 只改主应用的 Info.plist：它带 UILaunchStoryboardName（启动屏配置）。
  if (!text.includes("UILaunchStoryboardName")) continue;
  if (text.includes("<string>senplayer</string>")) {
    console.log(`[senplayer] ${p} 已登记 senplayer，不用改`);
    patched++;
    continue;
  }
  if (text.includes("<key>LSApplicationQueriesSchemes</key>")) {
    // 已有该键：往它的 <array> 里加一项。
    const m = text.match(
      /(<key>LSApplicationQueriesSchemes<\/key>\s*<array>)/,
    );
    if (!m) {
      console.log(`[senplayer] 警告：${p} 的 LSApplicationQueriesSchemes 结构异常，未改`);
      continue;
    }
    text = text.replace(m[1], `${m[1]}\n\t\t<string>senplayer</string>`);
  } else {
    // 没有该键：插到顶层 </dict> 之前。
    const m = text.match(/<\/dict>(\s*)<\/plist>/);
    if (!m) {
      console.log(`[senplayer] 警告：${p} 结构异常，未改`);
      continue;
    }
    text = text.replace(
      /<\/dict>\s*<\/plist>/,
      `\t<key>LSApplicationQueriesSchemes</key>\n\t<array>\n\t\t<string>senplayer</string>\n\t</array>\n</dict>${m[1]}</plist>`,
    );
  }
  writeFileSync(p, text);
  console.log(`[senplayer] senplayer 已登记 → ${p}`);
  patched++;
}
if (patched === 0) {
  console.log("[senplayer] 警告：没找到带 UILaunchStoryboardName 的 Info.plist，本次不改");
}
