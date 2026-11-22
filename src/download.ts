import { invoke, isTauri } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { clearLocalNativePlayback, markLocalNativePlayback, openNativePlayer } from "./plugins/nativePlayer";

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
 * 扩展名由 Rust 侧按实际内容定）。folder 是可选的资源文件夹名（多集资源
 * 如作品标题，分集文件存进 Downloads/<folder>/；单文件资源不传，散放在
 * Downloads 下）。进度经 download-progress 事件回传，按 downloadId 过滤；
 * 完成返回保存路径与字节数，直播返回当前窗口的片段时长（durationSec）与
 * live=true。仅 App 内可用，网页版抛错。 */
export async function downloadMedia(
  url: string,
  name: string,
  onProgress: (p: DownloadProgress) => void,
  folder?: string,
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
    // folder 是单字，无大小写问题；没传时不带这个键，Rust 侧得 None。
    return await invoke<DownloadResult>("download_media", { url, name, downloadId, ...(folder ? { folder } : {}) });
  } finally {
    unlisten();
  }
}

/** 导出已下载的文件到相册：iOS 用 PHPhotoLibrary 直接存视频（见
 * SharePlugin.saveToPhotos），不用走分享面；其他平台不支持。 */
export async function exportDownloadToPhotos(path: string): Promise<void> {
  await invoke("export_to_photos", { path });
}

/** AVPlayer 能直接播的本地文件扩展名。HLS 拼合的 .ts 不在内——
 * AVPlayer / WebView 都播不了裸 .ts 文件（只有 .m3u8 清单能播），
 * 这类文件点按会提示改用导出、到 SenPlayer 里打开。 */
const NATIVE_PLAYABLE_EXT = /\.(mp4|mov|m4v)$/i;

/** 点播一个已下载文件：用 iOS 原生 AVPlayer 全屏播本地文件，不走
 * 插件会话（不占在线流、不写播放进度）。文件路径拼成 file:// URL
 * 交给原生播放器；失败时清掉本地标记并把错抛给调用方提示。 */
export async function playDownloadedFile(file: DownloadedFile): Promise<void> {
  if (!isTauri()) throw new Error("仅在 App 内可用");
  if (!NATIVE_PLAYABLE_EXT.test(file.name)) {
    throw new Error("这是 HLS 拼合的 .ts 文件，本机播不了，请点导出、用 SenPlayer 打开");
  }
  const fileUrl = `file://${file.path.split("/").map(encodeURIComponent).join("/")}`;
  markLocalNativePlayback();
  try {
    await openNativePlayer({
      engine: "avplayer",
      url: fileUrl,
      sessionId: `local-${file.name}`,
      position: 0,
      rate: 1,
      volume: 1,
      muted: false,
      title: file.name,
    });
  } catch (error) {
    clearLocalNativePlayback();
    throw error;
  }
}

/** 分享已下载的文件：iOS 弹系统分享面（可存到“文件”、发到微信等）；
 * 其他平台用默认应用打开该文件。 */
export async function shareDownloadedFile(path: string): Promise<void> {
  await invoke("share_file", { path });
}

/** 把已下载文件挂到回环服务上，返回 SenPlayer 可直接取流的地址
 * （http://127.0.0.1:端口/local/<随机 id>）。文件本身不走插件会话、
 * 不过期；地址只在应用进程活着时响应——回环取流快，短剧一集几 MB，
 * SenPlayer 几秒内就能取完；文件特别大、取到一半进程被挂起时仍会断，
 * 那时用分享按钮的系统分享面“用 SenPlayer 打开”兜底。 */
export async function serveDownloadForSenPlayer(path: string): Promise<string> {
  return invoke<string>("serve_download", { path });
}

/** 设置里下载管理列出的一个已下载文件。modified 是修改时间的 Unix 秒。
 * folder 是文件所在的资源文件夹名（多集资源才有）；单文件资源为 null。 */
export interface DownloadedFile {
  name: string;
  path: string;
  bytes: number;
  modified: number | null;
  folder: string | null;
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
