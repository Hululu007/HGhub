import { useCallback, useEffect, useState } from "react";
import { deleteDownload, exportDownloadToPhotos, listDownloads, playDownloadedFile, shareDownloadedFile, type DownloadedFile } from "../download";
import { IDownload, IPlayF, IRefresh, ISendF, ITrash } from "../icons";
import { Notice } from "./BrowsePages";

/** 设置与更多播放选项里的下载管理：列出文稿/Downloads 里的已完成下载。
 * 点按文件名直接在本机播（AVPlayer 全屏；HLS 拼合的 .ts 本机播不了，
 * 点按会提示改用导出）；每行有导出（存到相册，或走系统分享面存到
 * “文件”）与删除（点两下确认）。 */
export default function DownloadManager() {
  const [files, setFiles] = useState<DownloadedFile[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState("");
  const [info, setInfo] = useState("");
  const [busy, setBusy] = useState<string | null>(null);
  const [confirmPath, setConfirmPath] = useState<string | null>(null);
  const [exportPath, setExportPath] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    setLoading(true); setError(""); setInfo(""); setConfirmPath(null); setExportPath(null);
    try { setFiles(await listDownloads()); }
    catch (e) { setError(e instanceof Error ? e.message : "无法读取下载列表"); }
    finally { setLoading(false); }
  }, []);
  useEffect(() => { void refresh(); }, [refresh]);

  const play = async (f: DownloadedFile) => {
    setError(""); setInfo("");
    try { await playDownloadedFile(f); }
    catch (e) { setError(e instanceof Error ? e.message : "播放失败"); }
  };
  const exportToPhotos = async (f: DownloadedFile) => {
    setBusy(f.path); setError(""); setInfo("");
    try { await exportDownloadToPhotos(f.path); setExportPath(null); setInfo("已保存到相册"); }
    catch (e) { setError(e instanceof Error ? e.message : "保存到相册失败"); }
    finally { setBusy(null); }
  };
  const exportToFiles = async (f: DownloadedFile) => {
    setBusy(f.path); setError(""); setInfo("");
    try { await shareDownloadedFile(f.path); setExportPath(null); }
    catch { setError("分享失败"); }
    finally { setBusy(null); }
  };
  const remove = async (f: DownloadedFile) => {
    if (confirmPath !== f.path) { setConfirmPath(f.path); return; }
    setConfirmPath(null); setBusy(f.path); setError(""); setInfo("");
    try { await deleteDownload(f.path); await refresh(); }
    catch (e) { setError(e instanceof Error ? e.message : "删除失败"); }
    finally { setBusy(null); }
  };

  const total = files.reduce((n, f) => n + f.bytes, 0);
  return <div className="px-3.5 py-3">
    <div className="flex items-center justify-between gap-2">
      <div className="text-[12.5px] text-on">{files.length ? `${files.length} 个 · ${formatSize(total)}` : "暂无下载"}</div>
      <button className="pressable disabled:opacity-50 text-[12px] text-primary flex items-center gap-1" disabled={loading} onClick={() => void refresh()}><IRefresh size={14}/>{loading ? "读取中…" : "刷新"}</button>
    </div>
    {error && <div className="mt-2"><Notice error={error}/></div>}
    {info && <p className="mt-2 text-[12px] text-primary">{info}</p>}
    {files.length > 0 && <ul className="mt-2 divide-y divide-s3">
      {files.map(f => <li key={f.path} className="py-2.5">
        <div className="flex items-center gap-3">
          <button className="shrink-0 w-8 h-8 rounded-full bg-s3 flex items-center justify-center text-primary pressable" aria-label={`播放 ${f.name}`} onClick={() => void play(f)}><IPlayF size={14}/></button>
          <button className="flex-1 min-w-0 text-left pressable" aria-label={`播放 ${f.name}`} onClick={() => void play(f)}>
            <div className="text-[12.5px] text-on truncate">{f.name}</div>
            <div className="text-[10.5px] text-mut">{formatSize(f.bytes)}{f.modified ? ` · ${formatDate(f.modified)}` : ""} · 点按播放</div>
          </button>
          <button className="pressable disabled:opacity-50 shrink-0 w-8 h-8 rounded-full flex items-center justify-center text-primary" aria-label={`导出 ${f.name}`} aria-expanded={exportPath === f.path} disabled={busy === f.path} onClick={() => setExportPath(exportPath === f.path ? null : f.path)}><ISendF size={15}/></button>
          <button className="pressable disabled:opacity-50 shrink-0 w-8 h-8 rounded-full flex items-center justify-center text-mut" aria-label={`删除 ${f.name}`} disabled={busy === f.path} onClick={() => void remove(f)}>{confirmPath === f.path ? <span className="text-[11px] text-primary">确认</span> : <ITrash size={15}/>}</button>
        </div>
        {exportPath === f.path && <div className="mt-2 ml-11 flex gap-2">
          <button className="pressable disabled:opacity-50 px-4 py-1.5 rounded-full text-[12.5px] font-semibold" disabled={busy === f.path} onClick={() => void exportToPhotos(f)}>保存到相册</button>
          <button className="pressable disabled:opacity-50 px-4 py-1.5 rounded-full text-[12.5px] font-semibold" disabled={busy === f.path} onClick={() => void exportToFiles(f)}>保存到文件</button>
        </div>}
      </li>)}
    </ul>}
    {!files.length && !loading && <p className="text-[11px] text-mut mt-2">在播放页点“下载”保存的视频会列在这里，点按文件名可直接播放，导出可存到相册或文件。</p>}
  </div>;
}

function formatSize(bytes: number): string {
  if (bytes >= 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
  if (bytes >= 1024) return `${(bytes / 1024).toFixed(0)} KB`;
  return `${bytes} B`;
}

function formatDate(modified: number): string {
  const d = new Date(modified * 1000);
  return `${d.getMonth() + 1}/${d.getDate()} ${d.getHours().toString().padStart(2, "0")}:${d.getMinutes().toString().padStart(2, "0")}`;
}
