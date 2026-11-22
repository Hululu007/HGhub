import { useCallback, useEffect, useMemo, useState } from "react";
import { deleteDownload, exportDownloadToPhotos, listDownloads, playDownloadedFile, serveDownloadForSenPlayer, shareDownloadedFile, type DownloadedFile } from "../download";
import { openInSenPlayer } from "../senplayer";
import { platform } from "../environment";
import { IChevR, IChevronDown, IPlayF, IRefresh, ISendF, IShare, ITrash, ITrendUp } from "../icons";
import { Notice } from "./BrowsePages";

/** 设置与更多播放选项里的下载管理：列出文稿/Downloads 里的已完成下载。
 * 多集资源按文件夹归类：同一资源的各集都在以资源名命名的文件夹里，
 * 文件夹可折叠；单个资源文件不建文件夹，直接散放在列表里。
 * 点按文件名直接在本机播（AVPlayer 全屏；HLS 拼合的 .ts 本机播不了，
 * 点按会提示改用导出）；每行有导出（存到相册，或走系统分享面存到
 * “文件”）、分享（直接弹系统分享面）、SenPlayer（仅 iOS，弹系统
 * “用…打开”菜单，点 SenPlayer 就把文件交给它播）与删除（点两下确认）。 */
/** Tauri invoke 失败时抛的是字符串本身，不是 Error 实例——直接读
 * e.message 会读不到，只剩兜底的“播放失败”；先把字符串原样报出来，
 * 下次真机报错才能看到到底是哪一步失败。 */
function errText(e: unknown, fallback: string): string {
  if (typeof e === "string" && e) return e;
  if (e instanceof Error && e.message) return e.message;
  return fallback;
}

export default function DownloadManager() {
  const [files, setFiles] = useState<DownloadedFile[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState("");
  const [info, setInfo] = useState("");
  const [busy, setBusy] = useState<string | null>(null);
  const [confirmPath, setConfirmPath] = useState<string | null>(null);
  const [exportPath, setExportPath] = useState<string | null>(null);
  // 默认全部展开；点文件夹标题可折叠，记下折叠了哪些。
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());

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
    catch (e) { setError(errText(e, "播放失败")); }
  };
  const exportToPhotos = async (f: DownloadedFile) => {
    setBusy(f.path); setError(""); setInfo("");
    try { await exportDownloadToPhotos(f.path); setExportPath(null); setInfo("已保存到相册"); }
    catch (e) { setError(errText(e, "保存到相册失败")); }
    finally { setBusy(null); }
  };
  const exportToFiles = async (f: DownloadedFile) => {
    setBusy(f.path); setError(""); setInfo("");
    try { await shareDownloadedFile(f.path); setExportPath(null); }
    catch (e) { setError(errText(e, "分享失败")); }
    finally { setBusy(null); }
  };
  // 分享：与导出里的“保存到文件”是同一个系统分享面，这里不经选择器、
  // 点图标就直接弹出来（发微信、存到“文件”、用 SenPlayer 打开都从这走）。
  const share = async (f: DownloadedFile) => {
    setBusy(f.path); setError(""); setInfo("");
    try { await shareDownloadedFile(f.path); }
    catch (e) { setError(errText(e, "分享失败")); }
    finally { setBusy(null); }
  };
  // SenPlayer：直接跳过去播。文件先挂到回环服务上（serve_download），
  // 再按在线播放同一条 Scheme 跳 SenPlayer，不弹“用…打开”菜单。
  // 回环取流快，短剧一集几 MB，SenPlayer 几秒内就能取完；文件特别大、
  // 取到一半进程被挂起时仍会断，那时用分享按钮的系统分享面兜底。
  const openSenPlayer = async (f: DownloadedFile) => {
    setBusy(f.path); setError(""); setInfo("");
    try { await openInSenPlayer(await serveDownloadForSenPlayer(f.path), f.name); }
    catch (e) { setError(errText(e, "SenPlayer 打开失败")); }
    finally { setBusy(null); }
  };
  const remove = async (f: DownloadedFile) => {
    if (confirmPath !== f.path) { setConfirmPath(f.path); return; }
    setConfirmPath(null); setBusy(f.path); setError(""); setInfo("");
    try { await deleteDownload(f.path); await refresh(); }
    catch (e) { setError(errText(e, "删除失败")); }
    finally { setBusy(null); }
  };

  // 文件夹分组：按 folder 归类，按文件夹里最新文件的修改时间倒序；
  // 没 folder 的是单个资源文件，直接散放。
  const folders = useMemo(() => {
    const map = new Map<string, DownloadedFile[]>();
    for (const f of files) if (f.folder) {
      const list = map.get(f.folder) ?? [];
      list.push(f);
      map.set(f.folder, list);
    }
    return [...map.entries()].map(([name, items]) => ({
      name,
      items,
      total: items.reduce((n, f) => n + f.bytes, 0),
      latest: Math.max(...items.map(f => f.modified ?? 0)),
    })).sort((a, b) => b.latest - a.latest);
  }, [files]);
  const singles = files.filter(f => !f.folder);
  const total = files.reduce((n, f) => n + f.bytes, 0);
  const rowProps = { busy, confirmPath, exportPath, onPlay: play, onExportPhotos: exportToPhotos, onExportFiles: exportToFiles, onShare: share, onSenPlayer: openSenPlayer, onRemove: remove, onToggleExport: (p: string) => setExportPath(exportPath === p ? null : p) };

  return <div className="px-3.5 py-3">
    <div className="flex items-center justify-between gap-2">
      <div className="text-[12.5px] text-on">{files.length ? `${files.length} 个 · ${formatSize(total)}` : "暂无下载"}</div>
      <button className="pressable disabled:opacity-50 text-[12px] text-primary flex items-center gap-1" disabled={loading} onClick={() => void refresh()}><IRefresh size={14}/>{loading ? "读取中…" : "刷新"}</button>
    </div>
    {error && <div className="mt-2"><Notice error={error}/></div>}
    {info && <p className="mt-2 text-[12px] text-primary">{info}</p>}
    {folders.length > 0 && <div className="mt-2 divide-y divide-s3">
      {folders.map(folder => {
        const open = !collapsed.has(folder.name);
        return <div key={folder.name} className="py-2.5">
          <button className="w-full flex items-center gap-2 text-left pressable" aria-expanded={open} aria-label={`${open ? "折叠" : "展开"}文件夹 ${folder.name}`} onClick={() => setCollapsed(prev => {
            const next = new Set(prev);
            if (open) next.add(folder.name); else next.delete(folder.name);
            return next;
          })}>
            {open ? <IChevronDown size={15} className="text-mut shrink-0" /> : <IChevR size={15} className="text-mut shrink-0" />}
            <span className="flex-1 min-w-0 text-[13px] font-semibold text-on truncate">{folder.name}</span>
            <span className="text-[11px] text-mut shrink-0">{folder.items.length} 集 · {formatSize(folder.total)}</span>
          </button>
          {open && <ul className="mt-1 ml-5 divide-y divide-s3">
            {folder.items.map(f => <FileRow key={f.path} f={f} {...rowProps} />)}
          </ul>}
        </div>;
      })}
    </div>}
    {singles.length > 0 && <ul className="mt-2 divide-y divide-s3">
      {singles.map(f => <FileRow key={f.path} f={f} {...rowProps} />)}
    </ul>}
    {!files.length && !loading && <p className="text-[11px] text-mut mt-2">在播放页点“下载”保存的视频会列在这里：多集资源按资源名收进一个文件夹，单个资源直接列出；点按文件名可直接播放，导出可存到相册或文件，分享按钮直接弹系统分享面。</p>}
  </div>;
}

type RowProps = {
  f: DownloadedFile;
  busy: string | null;
  confirmPath: string | null;
  exportPath: string | null;
  onPlay: (f: DownloadedFile) => void;
  onExportPhotos: (f: DownloadedFile) => void;
  onExportFiles: (f: DownloadedFile) => void;
  onShare: (f: DownloadedFile) => void;
  onSenPlayer: (f: DownloadedFile) => void;
  onRemove: (f: DownloadedFile) => void;
  onToggleExport: (path: string) => void;
};

function FileRow({ f, busy, confirmPath, exportPath, onPlay, onExportPhotos, onExportFiles, onShare, onSenPlayer, onRemove, onToggleExport }: RowProps) {
  return <li className="py-2.5">
    <div className="flex items-center gap-3">
      <button className="shrink-0 w-8 h-8 rounded-full bg-s3 flex items-center justify-center text-primary pressable" aria-label={`播放 ${f.name}`} onClick={() => onPlay(f)}><IPlayF size={14}/></button>
      <button className="flex-1 min-w-0 text-left pressable" aria-label={`播放 ${f.name}`} onClick={() => onPlay(f)}>
        <div className="text-[12.5px] text-on truncate">{f.name}</div>
        <div className="text-[10.5px] text-mut">{formatSize(f.bytes)}{f.modified ? ` · ${formatDate(f.modified)}` : ""} · 点按播放</div>
      </button>
      <button className="pressable disabled:opacity-50 shrink-0 w-8 h-8 rounded-full flex items-center justify-center text-primary" aria-label={`导出 ${f.name}`} aria-expanded={exportPath === f.path} disabled={busy === f.path} onClick={() => onToggleExport(f.path)}><ISendF size={15}/></button>
      <button className="pressable disabled:opacity-50 shrink-0 w-8 h-8 rounded-full flex items-center justify-center text-primary" aria-label={`分享 ${f.name}`} disabled={busy === f.path} onClick={() => onShare(f)}><IShare size={15}/></button>
      {platform === "ios" && <button className="pressable disabled:opacity-50 shrink-0 w-8 h-8 rounded-full flex items-center justify-center text-primary" aria-label={`SenPlayer 播放 ${f.name}`} disabled={busy === f.path} onClick={() => onSenPlayer(f)}><ITrendUp size={15}/></button>}
      <button className="pressable disabled:opacity-50 shrink-0 w-8 h-8 rounded-full flex items-center justify-center text-mut" aria-label={`删除 ${f.name}`} disabled={busy === f.path} onClick={() => onRemove(f)}>{confirmPath === f.path ? <span className="text-[11px] text-primary">确认</span> : <ITrash size={15}/>}</button>
    </div>
    {exportPath === f.path && <div className="mt-2 ml-11 flex gap-2">
      <button className="pressable disabled:opacity-50 px-4 py-1.5 rounded-full text-[12.5px] font-semibold" disabled={busy === f.path} onClick={() => onExportPhotos(f)}>保存到相册</button>
      <button className="pressable disabled:opacity-50 px-4 py-1.5 rounded-full text-[12.5px] font-semibold" disabled={busy === f.path} onClick={() => onExportFiles(f)}>保存到文件</button>
    </div>}
  </li>;
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
