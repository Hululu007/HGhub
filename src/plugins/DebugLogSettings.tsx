import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { IInfo } from "../icons";
import { pluginError } from "./client";

// 运行日志查看器（v0.0.89 诊断版）：只读显示 App 自己的 debug.log
// 最后 200 行（总量封顶约 50KB），复现闪退后重开 App、点刷新，
// 把最后几行发回来定位崩点。不删、不导出。
export default function DebugLogSettings() {
  const [log, setLog] = useState(""), [busy, setBusy] = useState(false), [error, setError] = useState("");
  const refresh = async () => {
    setBusy(true); setError("");
    try { setLog(await invoke<string>("read_debug_log", { lines: 200 })); }
    catch (e) { setError(pluginError(e)); }
    finally { setBusy(false); }
  };
  return <>
    <div className="flex items-center gap-3 px-3.5 py-3"><span className="shrink-0 w-8 h-8 rounded-full bg-s3 flex items-center justify-center text-primary"><IInfo size={16}/></span><div className="flex-1 min-w-0"><div className="text-[12.5px] text-on">运行日志</div><div className="text-[10.5px] text-mut">App 自己的调试日志（只读）。复现问题后重开 App，点刷新看最后几行。</div></div><div className="shrink-0 flex"><button className="text-primary text-[12.5px]" disabled={busy} onClick={()=>void refresh()}>{busy?"读取中…":"刷新"}</button></div></div>
    {error&&<p role="alert" className="hpp-danger px-3.5 py-3 text-[12.5px]">{error}</p>}
    {log?<pre className="hg-diagnose-report">{log}</pre>:!error&&<p className="px-3.5 py-3 text-[12.5px] text-mut">暂无日志。点右上角刷新读取。</p>}
  </>;
}
