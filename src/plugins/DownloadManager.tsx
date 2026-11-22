import { useCallback, useEffect, useState } from "react";
import { deleteDownload, listDownloads, shareDownloadedFile, type DownloadedFile } from "../download";
import { IDownload, IRefresh, ITrash } from "../icons";
import { Notice } from "./BrowsePages";

/** 设置里的下载管理：列出文稿/Downloads 里的已完成下载，可分享（导出）与删除。
 * 分享弹系统分享面（可存到“文件”、发到微信等）；删除点两下确认。 */
export default function DownloadManager() {
  const [files, setFiles] = useState<DownloadedFile[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState<string | null>(null);
  const [confirmPath, setConfirmPath] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    setLoading(true); setError(""); setConfirmPath(null);
    try { setFiles(await listDownloads()); }
    catch (e) { setError(e instanceof Error ? e.message : "无法读取下载列表"); }
    finally { setLoading(false); }
  }, []);
  useEffect(() => { void refresh(); }, [refresh]);

  const share = async (f: DownloadedFile) => {
    setBusy(f.path); setError("");
    try { await shareDownloadedFile(f.path); }
    catch { setError("分享失败"); }
    finally { setBusy(null); }
  };
  const remove = async (f: DownloadedFile) => {
    if (confirmPath !== f.path) { setConfirmPath(f.path); return; }
    setConfirmPath(null); setBusy(f.path); setError("");
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
    {files.length > 0 && <ul className="mt-2 divide-y divide-s3">
      {files.map(f => <li key={f.path} className="flex items-center gap-3 py-2.5">
        <span className="shrink-0 w-8 h-8 rounded-full bg-s3 flex items-center justify-center text-primary"><IDownload size={16}/></span>
        <div className="flex-1 min-w-0">
          <div className="text-[12.5px] text-on truncate">{f.name}</div>
          <div className="text-[10.5px] text-mut">{formatSize(f.bytes)}{f.modified ? ` · ${formatDate(f.modified)}` : ""}</div>
        </div>
        <button className="pressable disabled:opacity-50 text-[12px] text-primary shrink-0" disabled={busy === f.path} onClick={() => void share(f)}>分享</button>
        <button className="pressable disabled:opacity-50 shrink-0 w-8 h-8 rounded-full flex items-center justify-center text-mut" aria-label={`删除 ${f.name}`} disabled={busy === f.path} onClick={() => void remove(f)}>{confirmPath === f.path ? <span className="text-[11px] text-primary">确认</span> : <ITrash size={15}/>}</button>
      </li>)}
    </ul>}
    {!files.length && !loading && <p className="text-[11px] text-mut mt-2">在播放页点“下载”保存的视频会列在这里，可分享导出或删除。</p>}
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
