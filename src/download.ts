import { invoke, isTauri } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

/** Rust 侧 download-progress 事件的载荷（见 src-tauri/src/download.rs）。
 * total（字节）直链文件才有；segmentsDone/segmentsTotal 是 HLS 的分片进度。 */
export interface DownloadProgress {
  id: string;
  downloaded: number;
  total: number | null;
  segmentsDone: number | null;
  segmentsTotal: number | null;
}

export interface DownloadResult {
  path: string;
  bytes: number;
  durationSec: number | null;
  live: boolean;
}

/** 下载当前资源：url 是回环播放地址（media.url），name 是标题（不带扩展名，
 * 扩展名由 Rust 侧按实际内容定）。进度经 download-progress 事件回传，
 * 按 downloadId 过滤；完成返回保存路径与字节数，直播返回当前窗口的片段
 * 时长（durationSec）与 live=true。仅 App 内可用，网页版抛错。 */
export async function downloadMedia(
  url: string,
  name: string,
  onProgress: (p: DownloadProgress) => void,
): Promise<DownloadResult> {
  if (!isTauri()) throw new Error("下载仅在 App 内可用");
  const downloadId = crypto.randomUUID();
  const unlisten = await listen<DownloadProgress>("download-progress", (e) => {
    if (e.payload.id === downloadId) onProgress(e.payload);
  });
  try {
    // Tauri invoke 参数默认按 camelCase 传（tauri-macros 默认
    // rename_all="camelCase"）：Rust 形参 download_id 对应 JS 键 downloadId。
    // 传 snake_case（download_id）会报 missing required key downloadId。
    return await invoke<DownloadResult>("download_media", { url, name, downloadId });
  } finally {
    unlisten();
  }
}

/** 分享已下载的文件：iOS 弹系统分享面（可存到“文件”、发到微信等）；
 * 其他平台用默认应用打开该文件。 */
export async function shareDownloadedFile(path: string): Promise<void> {
  await invoke("share_file", { path });
}

/** 设置里下载管理列出的一个已下载文件。modified 是修改时间的 Unix 秒。 */
export interface DownloadedFile {
  name: string;
  path: string;
  bytes: number;
  modified: number | null;
}

/** 列出文稿/Downloads 里的已完成下载（按修改时间倒序）。仅 App 内可用。 */
export async function listDownloads(): Promise<DownloadedFile[]> {
  if (!isTauri()) return [];
  return await invoke<DownloadedFile[]>("list_downloads");
}

/** 删除一个已下载文件（只允许删 Downloads 目录里的文件）。 */
export async function deleteDownload(path: string): Promise<void> {
  await invoke("delete_download", { path });
}

/** 进度换算成百分比（0..100）：HLS 按分片数，直链按字节；都没总量时返回 null。 */
export function downloadPercent(p: DownloadProgress): number | null {
  if (p.segmentsTotal && p.segmentsTotal > 0 && p.segmentsDone != null) {
    return Math.min(100, Math.round((p.segmentsDone / p.segmentsTotal) * 100));
  }
  if (p.total && p.total > 0) {
    return Math.min(100, Math.round((p.downloaded / p.total) * 100));
  }
  return null;
}

/** 字节数换算成 MB（一位小数），下载中无总量时显示用。 */
export function downloadMB(bytes: number): string {
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}
