#!/usr/bin/env node
/**
 * 把“后台音频”写进生成的 Xcode 工程的 Info.plist。
 *
 * 为什么需要：
 *   复制的播放地址是应用内回环服务（http://127.0.0.1:<端口>/hpp/...，
 *   media.rs 的 MediaSessions::start）。地址本身不是普通外链——它只
 *   在本应用进程活着时才响应。用户在蚂蚁影视里录像证实：视频播着的
 *   时候切到浏览器、粘贴回环地址，能播；v0.0.28 里 HGhub 复制后打不
 *   开，是因为那次录像里视频是暂停的——暂停时切出去，iOS 立即挂起
 *   进程，服务器停止响应，浏览器一直白屏加载。差别不在地址格式，
 *   在切出时应用是不是还活着。
 *
 *   做法：Info.plist 加 UIBackgroundModes: ["audio"]。视频播着时
 *   （有音频在播）切出应用，iOS 按音频后台模式让进程继续跑，本地
 *   服务器不断，浏览器就能接着从回环地址取流；暂停时没有音频在播，
 *   后台模式不保活，地址照样失效——这是平台机制，不是 bug，协议也
 *   不以持续后台执行为前提（见 09-media-transport.md §7）。所以复
 *   制提示按当前是否在播区分：播着时“播着时切出可用”，暂停时
 *   “须先播放再切出”。
 *
 *   副作用：视频播着时切出应用，声音会继续在后台响（普通视频应用
 *   的标准行为；要停就先暂停或回应用内关掉）。WKWebView 与原生
 *   AVPlayer 都走系统音频会话，只加这个键就能用，不用改 Rust/Swift。
 *
 * 做法（和 sync-ios-status-bar.mjs 同一个套路）：在生成的工程里找
 *   到主应用的 Info.plist（以带 UILaunchStoryboardName 区分，扩展
 *   目标的 plist 没有启动屏配置），把 UIBackgroundModes 写成
 *   ["audio"]；已有该键时只往数组里补 audio，不重复插。
 *   `src-tauri/gen/` 不入库，CI 每次重新 `tauri ios init` 都会生成
 *   新的默认 Info.plist，所以每次构建前都要跑这个同步。
 *
 * 用法：
 *   node scripts/sync-ios-background-audio.mjs
 *   （build-ios-unsigned.sh 在 xcodebuild 之前调用；tauri.conf.json
 *   的 beforeBuildCommand 也调用。Info.plist 在构建时读，跑晚了当次
 *   包不生效。找不到 gen/apple 或主应用 Info.plist 时跳过，不报错，
 *   只打警告——复制地址在浏览器里播会失效，但不影响构建与播放。）
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
const KEY = "UIBackgroundModes";

if (!existsSync(genApple)) {
  console.log("[bgaudio] 跳过：gen/apple 还没生成（先跑 tauri ios init）");
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
  console.log("[bgaudio] 警告：gen/apple 里没找到 Info.plist，本次不改后台音频");
  process.exit(0);
}

let patched = 0;
for (const p of plists) {
  let text = readFileSync(p, "utf-8");
  // 只改主应用的 Info.plist：它带 UILaunchStoryboardName（启动屏配置）。
  if (!text.includes("UILaunchStoryboardName")) continue;
  if (text.includes(`<key>${KEY}</key>`)) {
    const m = text.match(
      new RegExp(`<key>${KEY}</key>\\s*<array>([\\s\\S]*?)</array>`),
    );
    if (!m) {
      console.log(`[bgaudio] 警告：${p} 已有 ${KEY} 但值不是数组，未改`);
      continue;
    }
    if (m[1].includes("<string>audio</string>")) {
      console.log(`[bgaudio] ${p} 已含 audio，不用改`);
      patched++;
      continue;
    }
    text = text.replace(
      new RegExp(`(<key>${KEY}</key>\\s*<array>)`),
      "$1\n\t\t<string>audio</string>",
    );
  } else {
    // 没有该键：插到顶层 </dict> 之前（即 </dict> 与 </plist> 之间）。
    const m = text.match(/<\/dict>(\s*)<\/plist>/);
    if (!m) {
      console.log(`[bgaudio] 警告：${p} 结构异常，未改`);
      continue;
    }
    text = text.replace(
      /<\/dict>\s*<\/plist>/,
      `\t<key>${KEY}</key>\n\t<array>\n\t\t<string>audio</string>\n\t</array>\n</dict>${m[1]}</plist>`,
    );
  }
  writeFileSync(p, text);
  patched++;
  console.log(`[bgaudio] 后台音频已开启 → ${p}`);
}
if (patched === 0) {
  console.log("[bgaudio] 警告：没找到带 UILaunchStoryboardName 的 Info.plist，本次不改后台音频");
}
