import { invoke, isTauri } from "@tauri-apps/api/core";

/** 跳转 SenPlayer 播放：SenPlayer 的公开 URL Scheme，多方实现一致
 * （goplexcli、OrivioTV、lampa 的外部播放器表都是同一格式）：
 *   senplayer://x-callback-url/play?url=<编码后的播放地址>&name=<标题>
 * 只传 url + name，不编造它没公开的参数。须装 SenPlayer 才能打开；
 * 未装时 Swift 侧 canOpenURL 判出并报错（Info.plist 里须有
 * LSApplicationQueriesSchemes: senplayer，见 scripts/sync-ios-senplayer.mjs）。
 * 回环地址与浏览器跳转同理：只在应用进程活着时才响应，播着（有声音）
 * 切出时音频后台模式保活，暂停时保活不了。仅 App 内可用。 */
export function senPlayerUrl(url: string, title: string): string {
  const name = title ? `&name=${encodeURIComponent(title)}` : "";
  return `senplayer://x-callback-url/play?url=${encodeURIComponent(url)}${name}`;
}

export async function openInSenPlayer(url: string, title: string): Promise<void> {
  if (!isTauri()) throw new Error("SenPlayer 跳转仅在 App 内可用");
  await invoke("open_senplayer", { url: senPlayerUrl(url, title) });
}
