#!/usr/bin/env node
/**
 * 把 nodejs-mobile 嵌进iOS 包（Node 引擎 v0.0.70）。
 *
 * 两个步骤（都在 build-ios-unsigned.sh 里调，CI 与本地同一套路径）：
 *
 * 1. 准备（默认模式，xcodebuild 之前）：
 *    - 下载 pinned 的 nodejs-mobile-v18.20.4-ios.zip，校验大小与
 *      sha256，解到 src-tauri/gen/apple/NodeMobile.xcframework
 *      （已是同一版本时跳过）；
 *    - 把 Node 边车源码暂存到 gen/apple/node-app/（host.js 与 NOTICE.md）。
 *    xcframework 不提交进 git（解包约 100 MB）；gen/apple 本来就是
 *    CI 现生成的。
 *
 * 2. 装包（--install-app <APP 路径>，xcodebuild 之后、打 IPA 之前）：
 *    - 把 NodeMobile.framework（device slice）拷进 .app/Frameworks/；
 *    - 把暂存的 node-app/ 拷进 .app/node-app/。
 *    不做 codesign：本次构建是未签名的（CODE_SIGNING_ALLOWED=NO），
 *    签名由分发方（TrollStore）在安装时做；若将来走签名构建，这里要
 *    给 framework 补 codesign。
 *
 * 用法：
 *   node scripts/sync-ios-node.mjs
 *   node scripts/sync-ios-node.mjs --install-app <APP 路径>
 * 找不到 gen/apple 时跳过、不报错（和其它 sync-ios-*.mjs 同一约定）。
 *
 * 注意：Node 18 已 EOL、nodejs-mobile 实际停滞（见
 * docs/developers/node-engine.md §3.4）。本版是 pin 死 v18.20.4
 * 的假设检验，不是长期运行时承诺。
 */
import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import {
  cpSync,
  existsSync,
  mkdirSync,
  readFileSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const genApple = join(root, "src-tauri", "gen", "apple");
const sidecar = join(root, "src-tauri", "node-sidecar");

// nodejs-mobile v18.20.4（Node.js 18.20.4），2024-10-07 发布。
// 下载地址与 sha256 已于 2026-11-23 实测核对（51,492,431 字节；
// release 页未公布官方校验值，该值是实测下载所得，见 node-engine.md §3.1）。
const ZIP_URL =
  "https://github.com/nodejs-mobile/nodejs-mobile/releases/download/v18.20.4/nodejs-mobile-v18.20.4-ios.zip";
const ZIP_SHA256 =
  "8c5ca3a0d1e38de7f182a5642593e82593b820efd375a14b3ecafc4bcfee620e";
const ZIP_SIZE = 51492431;
const XC_VERSION_LINE = "NodeMobile version: 18.20.4";

const xcframework = join(genApple, "NodeMobile.xcframework");
const nodeAppStage = join(genApple, "node-app");

function log(msg) {
  console.log(`[node] ${msg}`);
}

async function downloadZip(dest) {
  log(`下载 ${ZIP_URL}`);
  const res = await fetch(ZIP_URL);
  if (!res.ok) throw new Error(`下载失败：HTTP ${res.status}`);
  const buf = Buffer.from(await res.arrayBuffer());
  writeFileSync(dest, buf);
  if (buf.length !== ZIP_SIZE) {
    throw new Error(`大小不符：期望 ${ZIP_SIZE}，实际 ${buf.length}`);
  }
  const digest = createHash("sha256").update(buf).digest("hex");
  if (digest !== ZIP_SHA256) {
    throw new Error(`sha256 不符：期望 ${ZIP_SHA256}，实际 ${digest}`);
  }
  log(`下载完成并校验通过（${buf.length} 字节）`);
}

async function prepare() {
  if (!existsSync(genApple)) {
    log("跳过：gen/apple 还没生成（先跑 tauri ios init）");
    return;
  }
  const versions = join(genApple, "NODE_VERSIONS");
  if (existsSync(versions) && readFileSync(versions, "utf-8").includes(XC_VERSION_LINE)) {
    log("NodeMobile.xcframework 已是 v18.20.4，跳过下载");
  } else {
    const zip = join(genApple, "nodejs-mobile-v18.20.4-ios.zip");
    await downloadZip(zip);
    rmSync(xcframework, { recursive: true, force: true });
    execFileSync("unzip", ["-q", "-o", zip, "-d", genApple], {
      stdio: "inherit",
    });
    rmSync(zip, { force: true });
    if (!existsSync(join(xcframework, "Info.plist"))) {
      throw new Error("解包后找不到 NodeMobile.xcframework/Info.plist");
    }
    writeFileSync(versions, `${XC_VERSION_LINE}\n`);
    log("NodeMobile.xcframework 已解包");
  }
  // 暂存边车源码：只拷运行时要的文件。package.json 强制 host.js
  // 按 CommonJS 跑（仓库根 package.json 是 type: module，且 iOS 包
  // 里 node-app/ 下没有 package.json，缺了它 host.js 会被当 ESM）。
  rmSync(nodeAppStage, { recursive: true, force: true });
  mkdirSync(nodeAppStage, { recursive: true });
  for (const file of ["host.js", "NOTICE.md", "package.json"]) {
    cpSync(join(sidecar, file), join(nodeAppStage, file));
  }
  log("边车源码已暂存到 gen/apple/node-app/");
}

function installApp(appDir) {
  if (!existsSync(appDir) || !statSync(appDir).isDirectory()) {
    throw new Error(`--install-app：找不到应用包目录 ${appDir}`);
  }
  if (!existsSync(join(genApple, "NODE_VERSIONS"))) {
    throw new Error("--install-app：NodeMobile.xcframework 不在 gen/apple，先跑准备步骤");
  }

  // NodeMobile.framework 嵌进 Frameworks/（Embed，Rust 侧的 node_start
  // 符号靠它解析）。
  cpSync(
    join(xcframework, "ios-arm64", "NodeMobile.framework"),
    join(appDir, "Frameworks", "NodeMobile.framework"),
    { recursive: true },
  );
  log("NodeMobile.framework 已嵌进 .app/Frameworks/");

  // 边车源码。
  cpSync(nodeAppStage, join(appDir, "node-app"), { recursive: true });
  log("边车源码已拷进 .app/node-app/");
}

const mode = process.argv[2];
if (mode === "--install-app") {
  const appDir = process.argv[3];
  if (!appDir) {
    console.error("用法：node scripts/sync-ios-node.mjs --install-app <APP 路径>");
    process.exit(1);
  }
  installApp(appDir);
} else if (mode) {
  console.error(`未知参数：${mode}`);
  process.exit(1);
} else {
  await prepare();
}
