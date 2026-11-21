import type { NativeEngine } from "./nativePlayer";

// 播放引擎选择。默认 "webview"：原来的 HTML5 <video> + hls.js 路一动不动，
// 新增的四个原生引擎只在用户选了之后才启用；"auto" 按下面的链自动试。
export type PlaybackEngine = "webview" | NativeEngine | "auto";

export const DEFAULT_PLAYBACK_ENGINE: PlaybackEngine = "webview";

const ENGINES: readonly string[] = ["webview", "avplayer", "mdk", "ksplayer", "mpv", "auto"];

export function isPlaybackEngine(value: unknown): value is PlaybackEngine {
  return typeof value === "string" && (ENGINES as readonly string[]).includes(value);
}

export function isNativeEngine(engine: PlaybackEngine): engine is NativeEngine {
  return engine === "avplayer" || engine === "mdk" || engine === "ksplayer" || engine === "mpv";
}

export function playbackEngineLabel(engine: PlaybackEngine): string {
  switch (engine) {
    case "webview": return "WebView（原）";
    case "avplayer": return "AVPlayer";
    case "mdk": return "MDK";
    case "ksplayer": return "KSPlayer";
    case "mpv": return "mpv";
    case "auto": return "自动";
  }
}

/**
 * Auto 的 fallback 链（纯函数，可单测）。HLS / progressive 先试 AVPlayer：
 * 系统自带、零依赖，H.264/HEVC + MP4/HLS 都能硬解；AVPlayer 吃不下的
 * 格式再依次试 MDK -> KSPlayer -> mpv；最后回落 WebView。
 * 注意这里只排“尝试顺序”，真正播不播得了由各引擎在打开时决定，
 * 失败时调用方按链往下试（见 Feed.tsx 的 openNative）。
 */
export function playbackEngineFallbackChain(
  engine: PlaybackEngine,
  format: "hls" | "progressive",
): NativeEngine[] {
  if (engine === "webview") return [];
  if (isNativeEngine(engine)) return [engine];
  // auto：目前 HLS 与 progressive 都先走 AVPlayer；容器/编码信息不在
  // PreparedMedia 里，AVPlayer 打不开时再由调用方往下试。
  void format;
  return ["avplayer", "mdk", "ksplayer", "mpv"];
}
