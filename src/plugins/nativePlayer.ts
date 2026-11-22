import { invoke, addPluginListener, type PluginListener } from "@tauri-apps/api/core";
import { platform } from "../environment";

// iOS 原生播放器前端封装：React -> Rust native_player_* 命令 ->
// Swift NativePlayerPlugin -> 四个引擎。WebView / <video> 仍是默认路，
// 这里只在用户选了原生引擎时调用（见 playbackEngine.ts 与 Feed.tsx）。
//
// 注意 invoke 参数名：Rust 命令形参是 snake_case（session_id），
// 这里按 snake_case 传，不要改成 camelCase，否则 Tauri 反序列化不到。

export type NativeEngine = "avplayer" | "mdk" | "ksplayer" | "mpv";

export interface NativePlayerOpenOptions {
  engine: NativeEngine;
  url: string;
  sessionId: string;
  position?: number;
  rate?: number;
  volume?: number;
  muted?: boolean;
  title?: string;
}

export interface NativePlayerState {
  engine: string | null;
  playing: boolean;
  position: number;
  duration: number;
  rate: number;
  volume: number;
  muted: boolean;
  /** 当前视频原始分辨率（像素）；0 表示未知。 */
  videoWidth: number;
  videoHeight: number;
}

export interface NativePlayerTimeEvent {
  engine: string;
  sessionId: string;
  position: number;
  duration: number;
  /** 当前视频原始分辨率（像素）；0 表示未知。 */
  videoWidth: number;
  videoHeight: number;
}

export interface NativePlayerEndEvent {
  engine: string;
  sessionId: string;
  position: number;
}

export interface NativePlayerErrorEvent {
  engine: string;
  sessionId: string;
  message: string;
}

export interface NativePlayerClosedEvent {
  engine: string;
  sessionId: string;
  position: number;
}

/** 只有 iOS 才有原生播放器桥；其它平台 invoke 会直接报“不支持”。 */
export function isNativePlayerAvailable(): boolean {
  return platform === "ios";
}

export function openNativePlayer(options: NativePlayerOpenOptions): Promise<NativePlayerState> {
  return invoke<NativePlayerState>("native_player_open", {
    engine: options.engine,
    url: options.url,
    sessionId: options.sessionId,
    position: options.position ?? 0,
    rate: options.rate ?? 1,
    volume: options.volume ?? 1,
    muted: options.muted ?? false,
    title: options.title,
  });
}

export function closeNativePlayer(): Promise<void> {
  return invoke<void>("native_player_close", {});
}

export function playNativePlayer(): Promise<NativePlayerState> {
  return invoke<NativePlayerState>("native_player_play", {});
}

export function pauseNativePlayer(): Promise<NativePlayerState> {
  return invoke<NativePlayerState>("native_player_pause", {});
}

export function seekNativePlayer(position: number): Promise<NativePlayerState> {
  return invoke<NativePlayerState>("native_player_seek", { position });
}

export function setNativePlayerRate(rate: number): Promise<NativePlayerState> {
  return invoke<NativePlayerState>("native_player_set_rate", { rate });
}

export function setNativePlayerVolume(volume: number, muted: boolean): Promise<NativePlayerState> {
  return invoke<NativePlayerState>("native_player_set_volume", { volume, muted });
}

export function nativePlayerState(): Promise<NativePlayerState> {
  return invoke<NativePlayerState>("native_player_state", {});
}

export function onNativePlayerTime(cb: (event: NativePlayerTimeEvent) => void): Promise<PluginListener> {
  return addPluginListener<NativePlayerTimeEvent>("native-player", "native-player-time", cb);
}

export function onNativePlayerEnded(cb: (event: NativePlayerEndEvent) => void): Promise<PluginListener> {
  return addPluginListener<NativePlayerEndEvent>("native-player", "native-player-ended", cb);
}

export function onNativePlayerError(cb: (event: NativePlayerErrorEvent) => void): Promise<PluginListener> {
  return addPluginListener<NativePlayerErrorEvent>("native-player", "native-player-error", cb);
}

export function onNativePlayerClosed(cb: (event: NativePlayerClosedEvent) => void): Promise<PluginListener> {
  return addPluginListener<NativePlayerClosedEvent>("native-player", "native-player-closed", cb);
}
