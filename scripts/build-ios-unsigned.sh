#!/usr/bin/env bash
set -euo pipefail

VERSION="${1:?Usage: build-ios-unsigned.sh VERSION}"
ROOT="$(pwd)"
LOG="$ROOT/ios-build-session.log"
ADDR_FILE="$(node -p "require('os').tmpdir() + '/' + require('./src-tauri/tauri.conf.json').identifier + '-server-addr'")"
mkdir -p artifacts
rm -f "$ADDR_FILE"

# Tauri 2.10.1 has no ios --no-sign flag. Keep its release options server
# alive for the generated Xcode 'Build Rust Code' phase instead.
pnpm tauri ios build --open --ci --features custom-protocol >"$LOG" 2>&1 &
SESSION_PID=$!
cleanup() {
  kill "$SESSION_PID" 2>/dev/null || true
  cat "$LOG"
}
trap cleanup EXIT
for ((attempt = 0; attempt < 180; attempt++)); do
  if ! kill -0 "$SESSION_PID" 2>/dev/null; then
    echo 'Tauri build session exited before becoming ready' >&2
    exit 1
  fi
  if [[ -s "$ADDR_FILE" ]]; then
    break
  fi
  sleep 1
done
if [[ ! -s "$ADDR_FILE" ]]; then
  echo 'Timed out waiting for the Tauri build options server' >&2
  exit 1
fi

# 前端与图标必须在 xcodebuild 之前就位，不能指望 beforeBuildCommand：
# 后台的 `tauri ios build --open` 只起 options server，beforeBuildCommand
# （pnpm build）要等到 Xcode 的 "Build Rust Code" 脚本阶段才跑，而那个阶段
# 在 "Copy Bundle Resources" 之后 —— 资源拷贝发生时 gen/apple/assets 还是
# 空的，包里的前端就是空的；图标编译（actool）同样在脚本阶段之前。
# v0.0.11 就是这么打出空前端 + 模板默认图标的。
pnpm build
rm -rf src-tauri/gen/apple/assets
cp -R dist src-tauri/gen/apple/assets
node scripts/sync-ios-icons.mjs
node scripts/sync-ios-launch-screen.mjs
node scripts/sync-ios-status-bar.mjs
node scripts/sync-ios-background-audio.mjs
node scripts/sync-ios-file-sharing.mjs
node scripts/sync-ios-senplayer.mjs
node scripts/sync-ios-photo-library.mjs
# Python 运行时：下载 BeeWare xcframework 并暂存边车源码（gen/apple 里），
# 链接与装包分别在下面的 xcodebuild 参数与 --install-app 里。
node scripts/sync-ios-python.mjs

# Node 引擎：下载 nodejs-mobile xcframework 并暂存边车源码（gen/apple 里），
# 链接与装包分别在下面的 xcodebuild 参数与 --install-app 里。
node scripts/sync-ios-node.mjs

# MPVKit 28 个 LGPL 产物（静态库）：build.rs 在 cargo 构建时（Xcode 的
# "Build Rust Code" 阶段，在链接之前）把它们的 ios-arm64 切片暂存到
# gen/apple/MPVKit/（见 src-tauri/build.rs），这里链进最终可执行文件。
# 顺序是依赖拓扑（被依赖的在后）；同一组列两遍——静态库单遍链接遇到
# 循环依赖会漏符号，第二遍补上。静态库不用嵌进 .app，没有 --install-app。
MPVKIT_LDFLAGS="-framework Libmpv -framework Libavdevice -framework Libavfilter -framework Libavformat -framework Libavcodec -framework Libass -framework Libbluray -framework Libssl -framework gnutls -framework Libswscale -framework Libswresample -framework Libplacebo -framework Libshaderc_combined -framework Libdovi -framework Libdav1d -framework Libuavs3d -framework Libharfbuzz -framework Libfreetype -framework Libfribidi -framework Libunibreak -framework lcms2 -framework Libcrypto -framework nettle -framework hogweed -framework gmp -framework Libavutil -framework Libuchardet"
# MoltenVK 不在上面：它是库型 XCFramework（切片里只有 libMoltenVK.a，
# 没有 .framework 包），用 -lMoltenVK + LIBRARY_SEARCH_PATHS 链，放
# 在最后（Libmpv / Libplacebo 用它的符号，被依赖的在后）。
# Tauri generates lowercase debug/release configurations. Using Release
# silently selected debug in the original workflow.
XCODE_TOOLCHAIN="$(xcode-select -p)/Toolchains/XcodeDefault.xctoolchain"
test -d "$XCODE_TOOLCHAIN/usr/lib/swift/iphoneos"
xcodebuild build \
  -toolchain com.apple.dt.toolchain.XcodeDefault \
  -project src-tauri/gen/apple/huangguo-client.xcodeproj \
  -scheme huangguo-client_iOS \
  -configuration release \
  -sdk iphoneos \
  -destination 'generic/platform=iOS' \
  -derivedDataPath "$ROOT/build_output" \
  TOOLCHAIN_DIR="$XCODE_TOOLCHAIN" \
  CODE_SIGNING_ALLOWED=NO CODE_SIGNING_REQUIRED=NO CODE_SIGN_IDENTITY="" \
  FRAMEWORK_SEARCH_PATHS='$(inherited) $(PROJECT_DIR) $(PROJECT_DIR)/Python.xcframework/ios-arm64 $(PROJECT_DIR)/MPVKit' \
  LIBRARY_SEARCH_PATHS='$(inherited) $(PROJECT_DIR)/MPVKit' \
  OTHER_LDFLAGS='$(inherited) -framework Python -framework SystemConfiguration'" $MPVKIT_LDFLAGS $MPVKIT_LDFLAGS -lMoltenVK"

APP_DIR="$ROOT/build_output/Build/Products/release-iphoneos"
# 产品名改名后 .app 文件名会跟着变（改名前是 HGhub.app），不写死名字，
# 直接取构建产物里唯一的 .app，避免改名后找不到包。
APP_COUNT=$(find "$APP_DIR" -maxdepth 1 -name "*.app" -type d | wc -l)
if [ "$APP_COUNT" -ne 1 ]; then
  echo "Expected exactly one .app in $APP_DIR, found $APP_COUNT" >&2
  ls -la "$APP_DIR" >&2 || true
  exit 1
fi
APP=$(find "$APP_DIR" -maxdepth 1 -name "*.app" -type d)
EXECUTABLE=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$APP/Info.plist")
test -s "$APP/$EXECUTABLE"
file "$APP/$EXECUTABLE" | grep 'Mach-O.*arm64'
# Avoid grep -q with pipefail: early pipe closure can hide a match.
strings "$APP/$EXECUTABLE" > "$ROOT/ios-binary-strings.txt"
if grep -F 'localhost:1420' "$ROOT/ios-binary-strings.txt"; then
  echo 'Refusing to package a development build' >&2
  exit 1
fi

# Python 运行时装包：标准库 + Python.framework + 边车源码进 .app，
# 再打 IPA 之前完成（.app 是每次现编的，装包步骤不可省略）。
node scripts/sync-ios-python.mjs --install-app "$APP"

# Node 引擎装包：NodeMobile.framework + 边车源码进 .app，
# 再打 IPA 之前完成。
node scripts/sync-ios-node.mjs --install-app "$APP"

PACKAGE_DIR=$(mktemp -d)
mkdir -p "$PACKAGE_DIR/Payload"
cp -R "$APP" "$PACKAGE_DIR/Payload/"
IPA="$ROOT/artifacts/HGhub_${VERSION}_ios.ipa"
(cd "$PACKAGE_DIR" && zip -qr "$IPA" Payload)
unzip -tq "$IPA"
ls -lh "$IPA"
