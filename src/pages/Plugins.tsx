import { useEffect, useRef, useState, type ReactNode, type TouchEvent } from "react";
import { ICpu, IBack, IPlus } from "../icons";
import { pluginError, type InstalledPlugin } from "../plugins/client";
import type { Endpoint, Setting } from "../plugins/protocol";
import { usePluginRequests } from "../plugins/usePluginRequests";
import { openWebhome } from "../plugins/webhome";
import PluginForms from "../plugins/PluginForms";
import PluginDistribution from "../plugins/PluginDistribution";
import CatSources from "../plugins/CatSources";
import { useHost } from "../plugins/HostContext";
import { isMobile } from "../environment";
import { registerBackHandler } from "../back";
const statuses: Record<InstalledPlugin["status"], string> = { ready: "已就绪", unloaded: "待加载", failed: "运行失败", incompatible: "不兼容", disabled: "已停用" };
function ErrorMessage({ error }: { error: string }) { return error ? <div className="hpp-error" role="alert">{error}</div> : null; }
export default function Plugins({onBack,focusId}: {onBack:()=>void;focusId?:string}) {
  const host=useHost();
  const plugins=host.plugins.filter(p=>host.prefs.mature||!p.meta.mature);
  return <div className={`hpp-page hpp-plugins-page refined-page${isMobile ? " refined-mobile" : ""}`}><ErrorMessage error={host.error}/><PluginManagement plugins={plugins} reload={host.reload} focusId={focusId} onBack={onBack}/></div>;
}
function PluginManagement({ plugins, reload, focusId = "", onBack }: { plugins: InstalledPlugin[]; reload: () => Promise<void>; focusId?: string; onBack: () => void }) {
  const request = usePluginRequests();
  const [error, setError] = useState(""), [busy, setBusy] = useState(false);
  const [options, setOptions] = useState<string | null>(focusId || null);
  const [tab, setTab] = useState<"installed" | "import" | "cat">("installed");
  const [confirmRemove, setConfirmRemove] = useState(false);
  useEffect(() => { setConfirmRemove(false); }, [options]);
  useEffect(() => { if (focusId) { setOptions(focusId); setTab("installed"); } }, [focusId]);
  const installedPlugins = plugins.filter(p => p.meta.runtime !== "cat");
  const catCount = plugins.length - installedPlugins.length;
  const perform = async (action: string, params: Record<string, unknown>) => { setBusy(true); setError(""); try { await request(action, params); await reload(); } catch (e) { setError(pluginError(e)); } finally { setBusy(false); } };
  const selectedPlugin=plugins.find(p=>p.meta.id===options);
  useEffect(() => {
    if (isMobile && selectedPlugin && tab === "installed") return registerBackHandler(() => { setOptions(null); return true; }, 20);
  }, [selectedPlugin, tab]);
  const [filter,setFilter]=useState("");
  const [kindFilter,setKindFilter]=useState("all");
  const visible = installedPlugins.filter(p => (kindFilter==="all"||(p.meta.kind==="resolver"?"resolver":"source")===kindFilter) && (!filter || p.meta.name.toLocaleLowerCase().includes(filter.toLocaleLowerCase())));
  const [batch, setBatch] = useState(false);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [confirmBatch, setConfirmBatch] = useState(false);
  const [swipedId, setSwipedId] = useState<string | null>(null);
  const [confirmSwipeId, setConfirmSwipeId] = useState<string | null>(null);
  const [swipeDx, setSwipeDx] = useState(0);
  const [draggingId, setDraggingId] = useState<string | null>(null);
  const dragRef = useRef<{id:string;startX:number;startY:number;base:number;dragging:boolean;dx:number}|null>(null);
  const toggleSelect = (id: string) => { setSelected(old => { const next = new Set(old); if (next.has(id)) next.delete(id); else next.add(id); return next; }); };
  const enterBatch = () => { setSwipedId(null); setConfirmSwipeId(null); setSwipeDx(0); setSelected(new Set()); setConfirmBatch(false); setBatch(true); };
  const exitBatch = () => { setBatch(false); setSelected(new Set()); setConfirmBatch(false); };
  const batchRemove = async () => {
    const ids = [...selected];
    if (!ids.length) return;
    if (!confirmBatch) { setConfirmBatch(true); return; }
    setConfirmBatch(false);
    for (const id of ids) await perform("remove", {id});
    exitBatch();
  };
  const swipeWidth = (id: string) => confirmSwipeId===id ? 220 : 88;
  const rowDx = (id: string) => draggingId===id ? swipeDx : swipedId===id ? -swipeWidth(id) : 0;
  const swipeHandlers = (p: InstalledPlugin) => ({
    onTouchStart: (e: TouchEvent) => {
      if (batch) return;
      const t = e.touches[0];
      dragRef.current = {id:p.meta.id, startX:t.clientX, startY:t.clientY, base:swipedId===p.meta.id?-swipeWidth(p.meta.id):0, dragging:false, dx:0};
      if (swipedId && swipedId!==p.meta.id) setSwipedId(null);
      if (confirmSwipeId && confirmSwipeId!==p.meta.id) setConfirmSwipeId(null);
    },
    onTouchMove: (e: TouchEvent) => {
      const s = dragRef.current;
      if (!s || s.id!==p.meta.id || batch) return;
      const t = e.touches[0];
      const dx = t.clientX-s.startX, dy = t.clientY-s.startY;
      if (!s.dragging) {
        if (Math.abs(dx)>12 && Math.abs(dx)>Math.abs(dy)) { s.dragging=true; setDraggingId(p.meta.id); }
        else return;
      }
      const w = swipeWidth(p.meta.id);
      const v = Math.max(-w, Math.min(0, s.base+dx));
      s.dx=v; setSwipeDx(v);
    },
    onTouchEnd: () => {
      const s = dragRef.current;
      if (!s || s.id!==p.meta.id) return;
      if (s.dragging) {
        if (s.dx<-44) setSwipedId(p.meta.id);
        else { if (swipedId===p.meta.id) setSwipedId(null); if (confirmSwipeId===p.meta.id) setConfirmSwipeId(null); }
        setSwipeDx(0); setDraggingId(null);
      }
      dragRef.current=null;
    },
    onTouchCancel: () => { dragRef.current=null; setSwipeDx(0); setDraggingId(null); },
  });
  // “已安装”内按源类型分组（互斥，按此顺序判定）；猫源不在此列，只在“猫源”分栏管理。
  const sourceGroups = [
    { key: "python", title: "Python", kind: "source" as const, match: (p: InstalledPlugin) => p.meta.runtime === "python" },
    { key: "html", title: "HTML", kind: "source" as const, match: (p: InstalledPlugin) => !!p.meta.webhome?.localPage },
    { key: "webhome", title: "WebHome", kind: "source" as const, match: (p: InstalledPlugin) => !!p.meta.webhome?.homePage && !p.meta.webhome?.localPage },
    { key: "js", title: "其他 JS 插件", kind: "source" as const, match: (p: InstalledPlugin) => (p.meta.runtime ?? "js") === "js" && !p.meta.webhome },
  ];
  const pluginSections = [
    ...sourceGroups.map(g => ({ ...g, total: installedPlugins.filter(p => p.meta.kind !== "resolver" && g.match(p)).length, items: visible.filter(p => p.meta.kind !== "resolver" && g.match(p)) })),
    { key: "resolver", title: "解析器", kind: "resolver" as const, total: installedPlugins.filter(p => p.meta.kind === "resolver").length, items: visible.filter(p => p.meta.kind === "resolver") },
  ];
  return <><header className="hpp-header"><button aria-label="返回" onClick={()=>isMobile&&selectedPlugin&&tab==="installed"?setOptions(null):onBack()}><IBack size={20}/></button><div><h1>{isMobile&&selectedPlugin&&tab==="installed"?selectedPlugin.meta.name:"插件中心"}</h1>{!isMobile&&<p>管理内容来源、解析器、站点线路与 HPP 协议声明的插件界面</p>}</div>{isMobile&&<button className="plugin-mobile-import" aria-label="导入插件" onClick={()=>setTab("import")}><IPlus size={22}/></button>}</header><nav className="hpp-provider-tabs" aria-label="插件中心分栏" hidden={isMobile&&!!selectedPlugin&&tab==="installed"}>
    <div className="plugin-management-segments">
      <button aria-pressed={tab === "installed"} onClick={() => setTab("installed")}>{isMobile?`已安装 ${installedPlugins.length}`:`已安装插件 (${installedPlugins.length})`}</button>
      <button aria-pressed={tab === "import"} onClick={() => setTab("import")}>{isMobile?"导入与订阅":"导入与列表订阅"}</button>
      <button aria-pressed={tab === "cat"} onClick={() => setTab("cat")}>{isMobile?`猫源 ${catCount}`:`猫源 (${catCount})`}</button>
    </div>
    <button className="primary plugin-quick-import" onClick={()=>{setTab("import");requestAnimationFrame(()=>document.querySelector<HTMLInputElement>(".hpp-distribution-location input")?.focus());}}><IPlus size={15}/>快速导入</button>
  </nav><div className="hpp-scroll"><div className="hpp-content">
    <div hidden={tab !== "import"} className="plugin-import-layout"><PluginDistribution plugins={installedPlugins} reload={reload} /></div>
    <div hidden={tab !== "cat"}><CatSources reload={reload} /></div>
    <div hidden={tab !== "installed"}>
    <div className={`refined-plugin-workspace${selectedPlugin?" has-selection":""}`}><div className="refined-plugin-list"><input className="refined-plugin-filter" aria-label="筛选插件名称" placeholder="筛选插件名称…" value={filter} onChange={e=>setFilter(e.target.value)}/>
    <nav className="plugin-kind-tabs" aria-label="插件类型">{[["all","全部"],["source","内容源"],["resolver","解析器"]].map(([id,label])=><button key={id} aria-pressed={kindFilter===id} onClick={()=>setKindFilter(id)}>{label} <small>{installedPlugins.filter(p=>id==="all"||(p.meta.kind==="resolver"?"resolver":"source")===id).length}</small></button>)}</nav>
    {!!installedPlugins.length && <div className="hpp-distribution-buttons" style={{marginBottom:12}}>{!batch ? <button onClick={enterBatch}>批量管理</button> : <><button onClick={()=>setSelected(new Set(visible.map(p=>p.meta.id)))}>全选</button><button onClick={()=>setSelected(old=>{const next=new Set(old);for(const p of visible){if(next.has(p.meta.id))next.delete(p.meta.id);else next.add(p.meta.id);}return next;})}>反选</button><button className="danger" disabled={busy||!selected.size} onClick={()=>void batchRemove()}>{confirmBatch?`确认删除 ${selected.size} 个？`:`删除所选（${selected.size}）`}</button><button onClick={exitBatch}>取消</button></>}</div>}
    <ErrorMessage error={error} />

    {!installedPlugins.length && <div className="hpp-empty"><ICpu size={40}/><h2>还没有安装插件</h2><p>导入数据源开始观看，或添加解析器拓展播放能力。</p><button className="primary" onClick={() => setTab("import")}>导入插件</button></div>}
    {pluginSections.map(section => <section className="hpp-plugin-group" key={section.key}>{section.total > 0 && (kindFilter === "all" || kindFilter === section.kind) && <h2>{section.title}<small>{section.total}</small></h2>}<div className="hpp-installed">{section.items.map(p => <article className={`hpp-plugin ${options===p.meta.id?"is-selected":""}`} key={p.meta.id} style={{position:"relative", overflow:"hidden"}}>
      {!batch && <button className="danger" aria-label={`删除 ${p.meta.name}`} disabled={busy} style={{position:"absolute", right:0, top:0, bottom:0, width:swipeWidth(p.meta.id), background:"var(--p-danger)", color:"var(--p-on-danger)", border:0, borderRadius:0, fontSize:12, padding:"0 8px"}} onClick={()=>{if(confirmSwipeId!==p.meta.id){setConfirmSwipeId(p.meta.id);setSwipedId(p.meta.id);return;}setConfirmSwipeId(null);setSwipedId(null);void perform("remove",{id:p.meta.id});}}>{confirmSwipeId===p.meta.id?`确认删除「${p.meta.name}」？`:"删除"}</button>}
      <div style={{display:"flex", alignItems:"center", gap:6, width:"100%", minHeight:60, background:options===p.meta.id?"var(--p-tint)":"var(--p-s1)", transform:`translateX(${rowDx(p.meta.id)}px)`, transition:draggingId===p.meta.id?"none":"transform .18s ease", touchAction:"pan-y"}} {...(!batch?swipeHandlers(p):{})}>
      {batch && <input type="checkbox" aria-label={`选择 ${p.meta.name}`} checked={selected.has(p.meta.id)} onChange={()=>toggleSelect(p.meta.id)} />}
      <button className="hpp-plugin-heading plugin-select" aria-label={"查看 "+p.meta.name+" 的设置与日志"} aria-pressed={options===p.meta.id} onClick={()=>{if(batch){toggleSelect(p.meta.id);return;}if(swipedId===p.meta.id){setSwipedId(null);setConfirmSwipeId(null);return;}setOptions(p.meta.id);}}><span className="hpp-plugin-icon refined-glyph">{p.meta.name.slice(0,1)}</span><div><h3>{p.meta.name}</h3><p>v{p.meta.version} · {p.meta.kind==="resolver"?"解析器":"内容源"}{p.meta.runtime==="python"?" · Python":p.meta.webhome?.localPage?" · HTML":p.meta.webhome?.homePage?" · WebHome":""}</p></div><span className={`hpp-status ${p.status}`}>{statuses[p.status]}</span></button>
      <button className="plugin-enable-switch" role="switch" aria-checked={p.enabled} aria-label={"启用 "+p.meta.name} disabled={busy} onClick={()=>{if(swipedId===p.meta.id){setSwipedId(null);setConfirmSwipeId(null);return;}void perform("enable",{id:p.meta.id,enabled:!p.enabled});}}><span/></button>
      </div>
    </article>)}</div></section>)}
    </div><aside className="refined-plugin-detail">{selectedPlugin?<><div className="plugin-overview"><span className="refined-glyph">{selectedPlugin.meta.name.slice(0,1)}</span><div><h2>{selectedPlugin.meta.name} <small>v{selectedPlugin.meta.version}</small></h2><p>{selectedPlugin.meta.id} · {statuses[selectedPlugin.status]}</p><p>{selectedPlugin.meta.description}</p><small>{selectedPlugin.origin?.manifestName||selectedPlugin.source||"粘贴代码"}</small></div><button onClick={()=>setOptions(null)}>收起</button></div><ErrorMessage error={selectedPlugin.error?.message||""}/><PluginOptions key={selectedPlugin.meta.id+selectedPlugin.revision} plugin={selectedPlugin} reload={reload}><section className="plugin-maintenance"><h3>版本维护</h3>{selectedPlugin.enabled&&selectedPlugin.status!=="ready"&&<button disabled={busy} onClick={()=>void perform("retry",{id:selectedPlugin.meta.id})}>重试初始化</button>}{selectedPlugin.canRestore&&<button disabled={busy} onClick={()=>void perform("restore",{id:selectedPlugin.meta.id})}>恢复上一可用版本</button>}<button onClick={()=>setTab("import")}>插件更新与订阅</button><button className="danger" disabled={busy} onClick={()=>{if(!confirmRemove){setConfirmRemove(true);return;}setConfirmRemove(false);void perform("remove",{id:selectedPlugin.meta.id});}}>{confirmRemove?"确认移除？":"移除插件"}</button></section></PluginOptions></>:<div className="hpp-empty"><ICpu size={32}/><h2>插件详情</h2><p>从左侧选择插件，查看账号、线路、设置与日志。</p></div>}</aside></div>
    </div>
  </div></div></>;
}

function PluginOptions({ plugin: p, reload, children }: { plugin: InstalledPlugin; reload: () => Promise<void>; children?: ReactNode }) {
  const request = usePluginRequests();
  const [settings, setSettings] = useState<Record<string, unknown>>({}), [endpoints, setEndpoints] = useState<Endpoint[]>([]), [logs, setLogs] = useState<unknown[]>([]), [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const [probes, setProbes] = useState<{id:string;url:string;ok:boolean;latencyMs:number;status?:number;error?:string}[]>([]);
  const [settingsLoaded, setSettingsLoaded] = useState(false), [notice, setNotice] = useState("");
  useEffect(() => {
    void request<Record<string, unknown>>("settings.get", { id: p.meta.id }).then(values => { setSettings(values); setSettingsLoaded(true); }).catch(e => setError(pluginError(e)));
    if (p.enabled && p.methods.includes("endpoints.list")) void request<Endpoint[]>("endpoint.list", { id: p.meta.id, refresh: false }).then(setEndpoints).catch(e => setError(pluginError(e)));
  }, [p.meta.id]);
  const update = async (action: string, data: Record<string, unknown>) => {
    setBusy(true); setError(""); setNotice(""); try { await request(action, { id: p.meta.id, ...data }); await reload(); setNotice("设置已保存"); } catch (e) { setError(pluginError(e)); } finally { setBusy(false); }
  };
  return <div className="hpp-options"><ErrorMessage error={error} />
    <div className="plugin-forms-column"><PluginForms plugin={p} /></div><div className="plugin-options-column">
    {p.meta.webhome && p.enabled && p.status === "ready" && <button disabled={busy} onClick={() => { setBusy(true); setError(""); void openWebhome(p.meta.id).catch(e => setError(pluginError(e))).finally(() => setBusy(false)); }}>打开 WebHome 首页</button>}
    {p.meta.settings?.map(s => <div className="hpp-setting" key={s.key}><label htmlFor={`setting-${s.key}`}>{s.label}<small>{s.description}</small></label><SettingInput setting={s} value={settings[s.key]} disabled={busy || !settingsLoaded} onChange={v => setSettings(old => ({ ...old, [s.key]: v }))} /><button disabled={busy || !settingsLoaded} onClick={() => void update("settings.set", { key: s.key, value: settings[s.key] })}>保存</button></div>)}
    {notice && <p role="status" className="hpp-muted">{notice}</p>}
    {!!endpoints.length && <label className="hpp-setting">站点线路<select value={p.endpoint?.id || ""} disabled={busy} onChange={e => void update("endpoint.set", { endpointId: e.target.value })}>{endpoints.map(e => <option value={e.id} key={e.id}>{e.label || e.id} · {e.url}</option>)}</select></label>}
    {p.enabled && p.methods.includes("endpoints.list") && <div className="hpp-distribution-buttons"><button disabled={busy} onClick={() => {
      setBusy(true); setError(""); void request<Endpoint[]>("endpoint.list", {id:p.meta.id,refresh:true}).then(async r=>{setEndpoints(r);setProbes([]);await reload();}).catch(e=>setError(pluginError(e))).finally(()=>setBusy(false));
    }}>刷新站点线路</button><button disabled={busy} onClick={() => {
      setBusy(true);setError("");void request<typeof probes>("endpoint.probe", {id:p.meta.id}).then(setProbes).catch(e=>setError(pluginError(e))).finally(()=>setBusy(false));
    }}>探测站点线路</button></div>}
    {!!probes.length && <ul className="hpp-probes">{probes.map(result=><li key={result.id}>{endpoints.find(e=>e.id===result.id)?.label||result.id} · {result.ok?`可达 · ${result.latencyMs} ms`:result.error||`HTTP ${result.status}`}</li>)}</ul>}
    <button onClick={() => void request<unknown[]>("logs", { id: p.meta.id }).then(setLogs).catch(e => setError(pluginError(e)))}>读取运行日志</button>
    {!!logs.length && <pre className="hpp-logs">{logs.map(x => JSON.stringify(x)).join("\n")}</pre>}{children}</div>
  </div>;
}
function SettingInput({ setting: s, value, disabled, onChange }: { setting: Setting; value: unknown; disabled: boolean; onChange: (v: unknown) => void }) {
  const id = `setting-${s.key}`;
  if (s.type === "switch") return <input id={id} type="checkbox" disabled={disabled} checked={value === true} onChange={e => onChange(e.target.checked)} />;
  if (s.type === "select") return <select id={id} disabled={disabled} value={String(value ?? "")} onChange={e => onChange(e.target.value)}>{s.options?.map(o => <option key={o.value} value={o.value}>{o.label}</option>)}</select>;
  return <input id={id} type={s.type === "number" ? "number" : s.type === "password" ? "password" : "text"} disabled={disabled} placeholder={s.placeholder} min={s.min} max={s.max} value={String(value ?? "")} onChange={e => onChange(s.type === "number" ? Number(e.target.value) : e.target.value)} />;
}
