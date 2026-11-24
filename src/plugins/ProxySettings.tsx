import { useEffect, useRef, useState } from "react";
import { IChevR, IClose, ILink } from "../icons";
import { pluginCommand, pluginError } from "./client";
import { useHost } from "./HostContext";
import { registerBackHandler } from "../back";
import { APP_VERSION } from "../environment";

type Mode = "system" | "custom" | "off";
type Config = { mode: Mode; url: string };
type Probe = { ok: boolean; ms: number; status?: number; error?: string };
const labels: Record<Mode, string> = { system: "跟随系统", custom: "自定义", off: "直连" };

export default function ProxySettings() {
  const [saved, setSaved] = useState<Config | null>(null), [open, setOpen] = useState(false);
  useEffect(() => { let live = true; void pluginCommand<Config>("proxy.get").then(v => { if(live)setSaved(v); }).catch(()=>{}); return()=>{live=false;}; }, []);
  return <><button className="hg-proxy-setting" onClick={()=>setOpen(true)} aria-haspopup="dialog"><span className="hg-proxy-icon"><ILink size={17}/></span><span><b>代理访问</b><small>{saved ? labels[saved.mode] : "设置应用使用的代理"}</small></span><IChevR size={16}/></button>
    {open&&<ProxyDialog onClose={()=>setOpen(false)} onSaved={v=>{setSaved(v);setOpen(false);}}/>}</>;
}

function ProxyDialog({ onClose, onSaved }: { onClose: () => void; onSaved: (v: Config) => void }) {
  const { sources } = useHost(), ref = useRef<HTMLDialogElement>(null), pending = useRef(false);
  const [mode, setMode] = useState<Mode>("system"), [url, setUrl] = useState("");
  const [ready, setReady] = useState(false), [busy, setBusy] = useState(""), [error, setError] = useState("");
  const [probe, setProbe] = useState<Probe | null>(null), [visible, setVisible] = useState(false);
  // 网络诊断探针（v0.0.80；v0.0.82：按对话框当前选中的模式跑，
  // 不是已保存的设置；报告头带应用版本与改写状态、顶部带结论行，
  // 结果同时写入所用插件的运行日志，见 hpp net.diagnose）。
  const [report, setReport] = useState<string | null>(null);
  const targets = sources.filter(p=>p.endpoint?.url);
  const [target, setTarget] = useState(targets[0]?.endpoint?.url || "");
  const close = () => { if(!pending.current)onClose(); };
  const load = async () => { try { const v = await pluginCommand<Config>("proxy.get");setMode(v.mode);setUrl(v.url);setReady(true);setError(""); } catch(e){setError(pluginError(e));} };
  useEffect(() => { ref.current!.showModal(); void load(); const un = registerBackHandler(()=>{close();return true;}); return()=>{un();}; }, []);
  const run = async (action: "test" | "save" | "diagnose") => {
    if(pending.current)return;pending.current=true;setBusy(action);setError("");
    try {
      if(action==="test") setProbe(await pluginCommand<Probe>("proxy.test",{mode,url:mode==="custom"?url:"",target}));
      else if(action==="diagnose") setReport(await pluginCommand<string>("net.diagnose",{mode,url:mode==="custom"?url:"",version:APP_VERSION}));
      else { const v=await pluginCommand<Config>("proxy.set",{mode,url:mode==="custom"?url:""});onSaved(v); }
    } catch(e){setError(pluginError(e));} finally {pending.current=false;setBusy("");}
  };
  const disabled = !ready || !!busy;
  return <dialog className="hg-proxy-dialog" ref={ref} aria-labelledby="proxy-title" onCancel={e=>{e.preventDefault();close();}} onClick={e=>{if(e.target===e.currentTarget)close();}}>
    <header><h2 id="proxy-title"><ILink size={20}/>代理访问</h2><button aria-label="关闭代理设置" disabled={!!busy} onClick={close}><IClose size={20}/></button></header>
    <p>仅应用于红苹果影视的插件、图片与视频请求。测试当前设置后，保存即可生效。</p>
    <div className="hg-proxy-modes" aria-label="代理模式">{(["system","custom","off"] as const).map(m=><button key={m} disabled={disabled} aria-pressed={mode===m} onClick={()=>{setMode(m);setProbe(null);setError("");}}>{labels[m]}</button>)}</div>
    {mode==="custom" ? <><label>代理地址<input aria-label="代理地址" type={visible?"text":"password"} autoComplete="off" spellCheck={false} placeholder="http://127.0.0.1:7890" value={url} disabled={disabled} onChange={e=>{setUrl(e.target.value);setProbe(null);}}/></label><button className="hg-proxy-reveal" onClick={()=>setVisible(v=>!v)}>{visible?"隐藏地址":"显示地址"}</button><p>支持 HTTP、HTTPS、SOCKS。手机上的 127.0.0.1 指手机自身；使用电脑代理请填写电脑局域网地址。</p></> : <p>{mode==="system"?"沿用系统固定代理与环境变量。VPN / TUN 由操作系统接管。":"不使用应用层代理和代理环境变量；系统 VPN / TUN 仍由操作系统接管。"}已保存的自定义地址会保留。</p>}
    <label>测试地址<input aria-label="测试地址" list="proxy-test-targets" type="url" placeholder="https://你的片源地址" value={target} disabled={disabled} onChange={e=>{setTarget(e.target.value);setProbe(null);}}/></label>
    <datalist id="proxy-test-targets">{targets.map(p=><option key={p.meta.id} value={p.endpoint!.url}>{p.meta.name}</option>)}</datalist>
    <p>测试仅检查所填地址的 HTTP 连通性，不修改已保存的代理。</p>
    {error&&<p role="alert" className="hg-proxy-error">{error}{!ready&&<button onClick={()=>void load()}>重新读取</button>}</p>}
    <p role="status">{busy==="test"?"正在测试连接…":busy==="diagnose"?"正在诊断网络…（约半分钟）":probe?`${probe.ok?"连接正常":probe.error||"连接失败"} · ${probe.ms} ms`:"保存后，新请求立即使用所选模式。"}</p>
    {report&&<><p>按当前选中的代理模式诊断（不改已保存的设置），结果同时写入所用插件的运行日志。</p><pre className="hg-diagnose-report">{report}</pre></>}
    <footer><button disabled={disabled||!target.trim()} onClick={()=>void run("test")}>测试连接</button><button disabled={disabled} onClick={()=>void run("diagnose")}>{busy==="diagnose"?"诊断中…":"网络诊断"}</button><button className="hg-watch-action" disabled={disabled} onClick={()=>void run("save")}>{busy==="save"?"保存中…":"保存代理"}</button></footer>
  </dialog>;
}
