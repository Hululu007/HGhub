#!/usr/bin/env node
/**
 * 把 BeeWare CPython 嵌进iOS 包（Python 运行时 v1）。
 *
 * 两个步骤（都在 build-ios-unsigned.sh 里调，CI 与本地同一套路径）：
 *
 * 1. 准备（默认模式，xcodebuild 之前）：
 *    - 下载 pinned 的 Python-Apple-support tarball，校验大小与 sha256，
 *      解到 src-tauri/gen/apple/Python.xcframework（已是同一版本时跳过）；
 *    - 把边车源码暂存到 gen/apple/app/（main.py 等 + base/），把
 *      site-packages/ 暂存到 gen/apple/app_packages/。
 *    xcframework 不提交进 git（解包 114 MB）；gen/apple 本来就是 CI 现生成的。
 *
 * 2. 装包（--install-app <APP 路径>，xcodebuild 之后、打 IPA 之前）：
 *    - rsync 标准库进 .app/python/lib/（排除 libpython*.dylib 与测试文件）；
 *    - 把 Python.framework 拷进 .app/Frameworks/；
 *    - 把暂存的 app/ 与 app_packages/ 拷进 .app；
 *    - 把 lib-dynload 与 app_packages 里的每个 .so 转成独立 Framework
 *      （BeeWare utils.sh 里 install_python / install_dylib 的同一套
 *      .fwork 机制：原位置留 .fwork 占位文件，framework 里留 .origin
 *      回指；解释器内置 AppleFrameworkLoader 能读 .fwork）。
 *    不做 codesign：本次构建是未签名的（CODE_SIGNING_ALLOWED=NO），
 *    签名由分发方（TrollStore）在安装时做；若将来走签名构建，这里要给
 *    每个生成的 framework 补 codesign（照 utils.sh 的 install_dylib）。
 *
 * 用法：
 *   node scripts/sync-ios-python.mjs
 *   node scripts/sync-ios-python.mjs --install-app <APP 路径>
 * 找不到 gen/apple 时跳过、不报错（和其它 sync-ios-*.mjs 同一约定）。
 */
import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import {
  cpSync,
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  renameSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { dirname, join, relative, sep } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const genApple = join(root, "src-tauri", "gen", "apple");
const sidecar = join(root, "src-tauri", "python-sidecar");

// BeeWare Python-Apple-support 3.12-b13（CPython 3.12.16），最低 iOS 13.0。
// 版本选择：与 ant-video 2.0.6 实证过的 CPython 3.12.13 同一 3.12 线，
// 取当前稳定补丁版；iPhone 13 Pro Max 的 iOS 16.6.1 高于两边底线。
// 下载地址与 sha256 已于 2026-11-23 实测核对（32,193,359 字节）。
const TARBALL_URL =
  "https://github.com/beeware/Python-Apple-support/releases/download/3.12-b13/Python-3.12-iOS-support.b13.tar.gz";
const TARBALL_SHA256 =
  "4eea2c76ad70f2df8c1767489315a16d72cd96ce5c086412ee2d154c3bd8e257";
const TARBALL_SIZE = 32193359;
const XC_VERSION_LINE = "Python version: 3.12.16";

const xcframework = join(genApple, "Python.xcframework");
const appStage = join(genApple, "app");
const appPackagesStage = join(genApple, "app_packages");

function log(msg) {
  console.log(`[python] ${msg}`);
}

function sha256(file) {
  return createHash("sha256").update(readFileSync(file)).digest("hex");
}

async function downloadTarball(dest) {
  log(`下载 ${TARBALL_URL}`);
  const res = await fetch(TARBALL_URL);
  if (!res.ok) throw new Error(`下载失败：HTTP ${res.status}`);
  const buf = Buffer.from(await res.arrayBuffer());
  writeFileSync(dest, buf);
  if (buf.length !== TARBALL_SIZE) {
    throw new Error(`大小不符：期望 ${TARBALL_SIZE}，实际 ${buf.length}`);
  }
  const digest = createHash("sha256").update(buf).digest("hex");
  if (digest !== TARBALL_SHA256) {
    throw new Error(`sha256 不符：期望 ${TARBALL_SHA256}，实际 ${digest}`);
  }
  log(`下载完成并校验通过（${buf.length} 字节）`);
}

async function prepare() {
  if (!existsSync(genApple)) {
    log("跳过：gen/apple 还没生成（先跑 tauri ios init）");
    return;
  }
  const versions = join(genApple, "VERSIONS");
  if (existsSync(versions) && readFileSync(versions, "utf-8").includes(XC_VERSION_LINE)) {
    log("Python.xcframework 已是 3.12-b13，跳过下载");
  } else {
    const tarball = join(genApple, "Python-3.12-iOS-support.b13.tar.gz");
    await downloadTarball(tarball);
    rmSync(xcframework, { recursive: true, force: true });
    execFileSync("tar", ["--no-same-owner", "-xzf", tarball, "-C", genApple], {
      stdio: "inherit",
    });
    rmSync(tarball, { force: true });
    log("Python.xcframework 已解包");
  }
  // 暂存边车源码：只拷运行时要的文件，不拷 tests/ 与 tools/。
  rmSync(appStage, { recursive: true, force: true });
  mkdirSync(appStage, { recursive: true });
  for (const file of ["main.py", "spider_server.py", "spider_runner.py", "proxy_response.py"]) {
    cpSync(join(sidecar, file), join(appStage, file));
  }
  cpSync(join(sidecar, "base"), join(appStage, "base"), { recursive: true });
  rmSync(appPackagesStage, { recursive: true, force: true });
  cpSync(join(sidecar, "site-packages"), appPackagesStage, { recursive: true });
  log("边车源码已暂存到 gen/apple/app/ 与 gen/apple/app_packages/");
}

function listFilesRecursive(dir, out = []) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const p = join(dir, entry.name);
    if (entry.isDirectory()) listFilesRecursive(p, out);
    else out.push(p);
  }
  return out;
}

/** BeeWare 的 install_dylib：把一个 .so 转成 Framework（不做 codesign）。 */
function installDylib(appDir, installBase, fullExt, bundleId) {
  const relExt = relative(appDir, fullExt).split(sep).join("/");
  const pythonExt = relExt.slice(installBase.length + 1);
  const fullModuleName = pythonExt.split(".")[0].split("/").join(".");
  const frameworkBundleId = `${bundleId}.${fullModuleName}`.replaceAll("_", "-");
  const frameworkFolder = `Frameworks/${fullModuleName}.framework`;
  const frameworkDir = join(appDir, frameworkFolder);
  if (!existsSync(frameworkDir)) {
    mkdirSync(frameworkDir, { recursive: true });
    const template = join(xcframework, "build", "iOS-dylib-Info-template.plist");
    cpSync(template, join(frameworkDir, "Info.plist"));
    execFileSync("plutil", [
      "-replace", "CFBundleExecutable", "-string", fullModuleName,
      join(frameworkDir, "Info.plist"),
    ]);
    execFileSync("plutil", [
      "-replace", "CFBundleIdentifier", "-string", frameworkBundleId,
      join(frameworkDir, "Info.plist"),
    ]);
  }
  // .so 移进 framework，原位置留 .fwork 占位，framework 里留 .origin 回指。
  const fworkPath = fullExt.slice(0, -".so".length) + ".fwork";
  writeFileSync(fworkPath, `${frameworkFolder}/${fullModuleName}\n`);
  writeFileSync(
    join(frameworkDir, `${fullModuleName}.origin`),
    `${relExt.slice(0, -".so".length)}.fwork\n`,
  );
  const dest = join(frameworkDir, fullModuleName);
  rmSync(dest, { force: true });
  renameSync(fullExt, dest);
  return fullModuleName;
}

function installApp(appDir) {
  if (!existsSync(appDir) || !statSync(appDir).isDirectory()) {
    throw new Error(`--install-app：找不到应用包目录 ${appDir}`);
  }
  if (!existsSync(join(genApple, "VERSIONS"))) {
    throw new Error("--install-app：Python.xcframework 不在 gen/apple，先跑准备步骤");
  }
  const bundleId = JSON.parse(
    readFileSync(join(root, "src-tauri", "tauri.conf.json"), "utf-8"),
  ).identifier;

  const pythonLib = join(appDir, "python", "lib");
  mkdirSync(pythonLib, { recursive: true });
  // 标准库：共享 lib/ + 真机切片 lib-arm64/（排除 libpython 链接与测试文件）。
  for (const src of [join(xcframework, "lib"), join(xcframework, "ios-arm64", "lib-arm64")]) {
    execFileSync("rsync", [
      "-a",
      "--exclude", "libpython*.dylib",
      "--exclude", "test",
      "--exclude", "tests",
      "--exclude", "__pycache__",
      "--exclude", "idlelib",
      `${src}/`, `${pythonLib}/`,
    ], { stdio: "inherit" });
  }
  log("标准库已拷进 .app/python/lib/");

  // Python.framework 嵌进 Frameworks/（Embed，Rust 侧的 Py_* 符号靠它解析）。
  cpSync(
    join(xcframework, "ios-arm64", "Python.framework"),
    join(appDir, "Frameworks", "Python.framework"),
    { recursive: true },
  );
  log("Python.framework 已嵌进 .app/Frameworks/");

  // 边车源码与第三方纯 Python 包。
  cpSync(appStage, join(appDir, "app"), { recursive: true });
  cpSync(appPackagesStage, join(appDir, "app_packages"), { recursive: true });
  log("边车源码已拷进 .app/app/ 与 .app/app_packages/");

  // .so → Framework：标准库 lib-dynload（66 个）与 app_packages（v1 无 .so，
  // 仍按同一流程扫一遍，以后加二进制包时不用改脚本）。
  let converted = 0;
  for (const [base, dir] of [
    ["python/lib/python3.12/lib-dynload", join(pythonLib, "python3.12", "lib-dynload")],
    ["app_packages", join(appDir, "app_packages")],
  ]) {
    if (!existsSync(dir)) continue;
    for (const file of listFilesRecursive(dir)) {
      if (!file.endsWith(".so")) continue;
      installDylib(appDir, base, file, bundleId);
      converted++;
    }
  }
  log(`已转 ${converted} 个 .so 为 Framework（.fwork 机制）`);
}

const mode = process.argv[2];
if (mode === "--install-app") {
  const appDir = process.argv[3];
  if (!appDir) {
    console.error("用法：node scripts/sync-ios-python.mjs --install-app <APP 路径>");
    process.exit(1);
  }
  installApp(appDir);
} else if (mode) {
  console.error(`未知参数：${mode}`);
  process.exit(1);
} else {
  await prepare();
}
