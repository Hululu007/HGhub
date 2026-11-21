import { existsSync, mkdirSync, copyFileSync, readdirSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

// 把 src-tauri/ios/ 下的原生 Swift 源码同步进 Tauri 生成的 Xcode 工程
// （原生播放器插件 + 屏幕方向插件）。
// 源文件只留在这里（可版本控制），gen/apple/ 由 `pnpm tauri ios init --ci`
// 生成，不进版本控制；每次 init / 拉新代码后重跑一次本脚本即可。
// 目标路径说明：Tauri 2 的 iOS 生成工程把插件 Swift 源码放在
// gen/apple/Sources/ 下（与 tauri 官方插件的 ios/Sources/ 布局一致）。
// 第一次在 Mac 上跑时若 Xcode 不收录该目录，把 Sources/NativePlayer
// 加进 app target 的 membership 即可，见 docs/developers/native-players.md。
const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const apple = join(root, "src-tauri/gen/apple");
if (!existsSync(apple)) {
  console.log("[ios] 未生成 gen/apple（先跑 pnpm tauri ios init --ci），跳过原生 Swift 同步");
  process.exit(0);
}
const dest = join(apple, "Sources/NativePlayer");
mkdirSync(dest, { recursive: true });
copyFileSync(join(root, "src-tauri/ios/NativePlayerPlugin.swift"), join(dest, "NativePlayerPlugin.swift"));
copyFileSync(join(root, "src-tauri/ios/OrientationPlugin.swift"), join(dest, "OrientationPlugin.swift"));
const engines = join(dest, "engines");
mkdirSync(engines, { recursive: true });
for (const name of readdirSync(join(root, "src-tauri/ios/engines")).filter((f) => f.endsWith(".swift"))) {
  copyFileSync(join(root, "src-tauri/ios/engines", name), join(engines, name));
}
console.log("[ios] 已同步原生 Swift 源码到 gen/apple/Sources/NativePlayer");
