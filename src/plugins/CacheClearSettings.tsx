import { useRef, useState } from "react";
import { ITrash } from "../icons";
import { pluginError } from "./client";
import { clearWebhomeData, type ClearWebhomeDataResult } from "./webhome";

const clearKeyLabels: Record<string, string> = { wo_domain_cache: "本站域名缓存", wo_monitor_snapshot: "监控快照", wo_site_domains: "手动设置的域名" };

// 全 App 缓存清理（v0.0.84；v0.0.85 起入口从代理对话框移出，设置页
// 独立「缓存清理」区块；流程与文案不变）：三个域名缓存键 + WebView
// 全部浏览数据 + 全部插件的 Rust 请求 Cookie 罐与响应缓存 + webhome:
// 前缀的存储键 + 媒体预加载缓存 + __pycache__ 与 *.pyc + Python 边车
// 运行时缓存（边车在运行时才清）+ 插件导入暂存文件，再重载页面；
// 最后清日志（ext.log + debug.log）。行结构照抄设置页的 Setting 行。
export default function CacheClearSettings() {
  const pending = useRef(false);
  const [busy, setBusy] = useState(false), [error, setError] = useState("");
  const [clearResult, setClearResult] = useState<ClearWebhomeDataResult | null>(null);
  const clearCache = async () => {
    if(pending.current)return;
    if(!window.confirm("会把全 App 的运行时缓存全部清掉：WebHome 页面的三个域名缓存（监控快照、本站域名缓存、手动设置的域名，将恢复默认）、WebView 的全部浏览数据（页面缓存、Cookie、登录状态、IndexedDB 等）、全部插件的请求 Cookie 与网络响应缓存、WebHome 在插件存储里的缓存键、媒体预加载缓存、插件目录下的 Python 字节码缓存（__pycache__ 与 .pyc，只删字节码）、Python 边车里已加载蜘蛛的运行时缓存（边车在运行时才清）、插件导入暂存文件；最后清日志（全部插件的运行日志与 App 自己的调试日志，日志从空开始，下次出问题时新日志更干净、更好定位）。清理后页面会重新加载，页面里的登录要重登一次。不动的东西：App 的设置（代理模式、代理地址等）、导入的插件与插件源文件（.py、.js、HTML 源）、插件存储里自己的数据、下载好的视频。确定要全部清掉吗？"))return;
    pending.current=true;setBusy(true);setError("");setClearResult(null);
    try { setClearResult(await clearWebhomeData()); }
    catch(e){ const msg=pluginError(e); setError(msg==="WEBHOME_NOT_OPEN"?"请先打开一次 WebHome（任意站点），再回来清理。":msg); }
    finally {pending.current=false;setBusy(false);}
  };
  return <>
    <div className="flex items-center gap-3 px-3.5 py-3"><span className="shrink-0 w-8 h-8 rounded-full bg-s3 flex items-center justify-center text-primary"><ITrash size={16}/></span><div className="flex-1 min-w-0"><div className="text-[12.5px] text-on">清理全部缓存</div><div className="text-[10.5px] text-mut">清掉全 App 的运行时缓存：域名缓存、WebView 浏览数据、全部插件的请求 Cookie 与响应缓存、媒体预加载、Python 字节码与边车缓存、导入暂存，最后清日志。不动设置、插件与插件源文件、下载的视频。</div></div><div className="shrink-0 flex"><button className="text-primary text-[12.5px]" disabled={busy} onClick={()=>void clearCache()}>{busy?"清理中…":"清理"}</button></div></div>
    {busy&&<p role="status" className="px-3.5 py-3 text-[12.5px]">正在清理全部缓存…</p>}
    {error&&<p role="alert" className="hpp-danger px-3.5 py-3 text-[12.5px]">{error}</p>}
    {clearResult&&<p role="status" className="px-3.5 py-3 text-[12.5px] leading-relaxed">已清理：域名缓存{clearResult.cleared.length?clearResult.cleared.map(k=>clearKeyLabels[k]||k).join("、"):"无"}；本来就没有：{clearResult.absent.length?clearResult.absent.map(k=>clearKeyLabels[k]||k).join("、"):"无"}；{clearResult.plugins} 个插件的请求 Cookie 清掉了 {clearResult.jar_cleared} 条、响应缓存丢了 {clearResult.response_cache_dropped} 条、WebHome 缓存键删了 {clearResult.webhome_keys_deleted} 个；媒体预加载缓存丢了 {clearResult.media_entries_dropped} 条；Python 字节码缓存删了 {clearResult.pycache_dirs} 个目录、{clearResult.pycache_files} 个零散 .pyc 文件；Python 边车{clearResult.py_running?`已加载的 ${clearResult.py_spiders_total} 个蜘蛛里清掉了 ${clearResult.py_spiders_cleared} 个的运行时缓存`:"没在运行，运行时缓存本就是空的"}；导入暂存清掉了 {clearResult.staging_files} 个文件；WebView 全部浏览数据已清除。日志已清空（插件运行日志 {clearResult.log_entries_dropped} 行{clearResult.debug_log_cleared?"、App 调试日志文件也已清空":""}），清完后日志从空开始，下次出问题时新日志更干净、更好定位。{clearResult.warnings.length?`注意：${clearResult.warnings.join("；")}。`:""}页面已重新加载，登录态已掉，请重登后再打开 WebHome 点一次分类。</p>}
  </>;
}
