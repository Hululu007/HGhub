import { invoke } from "@tauri-apps/api/core";
import { useEffect, useState } from "react";
import { pluginError } from "./client";
import { useHost } from "./HostContext";
import { usePluginRequests } from "./usePluginRequests";

interface CatSourceRecord {
  id: string; name: string; sourceType: "url" | "zip"; url: string | null; md5: string;
  enabled: boolean; updatedAt: number; status?: "ready" | "unloaded" | "failed" | "incompatible" | "disabled";
}
interface CatList { node: { running: boolean; version?: string }; sources: CatSourceRecord[] }
const catStatuses: Record<string, string> = { ready: "运行中", unloaded: "待加载", failed: "运行失败", incompatible: "不兼容", disabled: "已停用" };

export default function CatSources({ reload }: { reload: () => Promise<void> }) {
  const request = usePluginRequests();
  const host = useHost();
  const [node, setNode] = useState<CatList["node"] | null>(null);
  const [sources, setSources] = useState<CatSourceRecord[]>([]);
  const [url, setUrl] = useState("");
  const [busy, setBusy] = useState(false), [error, setError] = useState(""), [notice, setNotice] = useState("");
  const [confirmRemove, setConfirmRemove] = useState<string | null>(null);
  const load = async () => {
    const result = await request<CatList>("cat.list");
    setNode(result.node); setSources(result.sources);
  };
  useEffect(() => { void load().catch(e => setError(pluginError(e))); }, []);
  const run = async (work: () => Promise<string | void>, refresh = true) => {
    setBusy(true); setError(""); setNotice("");
    try {
      const message = await work();
      if (refresh) { await load(); await reload(); }
      if (message) setNotice(message);
    } catch (e) { setError(pluginError(e)); } finally { setBusy(false); }
  };
  const pickZip = async () => {
    const paths = await invoke<string[]>("pick_cat_zip");
    return paths[0];
  };
  return <div className="plugin-cat-sources">
    <section className="hpp-subscriptions" aria-label="Node 运行状态"><div className="hpp-section-title"><h2>{node?.running ? `Node 运行中${node.version ? ` · ${node.version}` : ""}` : "Node 已停止"}</h2></div>
      <p className="hpp-muted">Node 单实例，停用猫源请用下方开关。</p>
    </section>
    <section className="hpp-import" aria-label="添加猫源"><h2>添加猫源</h2>
      <form className="hpp-distribution-location" onSubmit={e => { e.preventDefault(); if (!url.trim()) return; const target = url.trim(); void run(async () => { await request("cat.import_url", { url: target }); setUrl(""); return "猫源已添加"; }); }}>
        <input aria-label="猫源订阅地址" placeholder="订阅 URL，如 https://…/index.js.md5" value={url} onChange={e => setUrl(e.target.value)} />
        <button disabled={busy || !url.trim()}>添加订阅</button>
      </form>
      <div className="hpp-distribution-buttons"><button disabled={busy} onClick={() => void run(async () => { const path = await pickZip(); if (!path) return; await request("cat.import_zip", { path }); return "猫源已添加"; })}>导入 ZIP</button></div>
    </section>
    <section className="hpp-subscriptions" aria-label="已添加的猫源"><div className="hpp-section-title"><h2>已添加的猫源</h2><span>{sources.length} 个</span></div>
      {sources.length ? sources.map(s => {
        const plugin = host.plugins.find(p => p.meta.id === s.id);
        const status = s.status || "unloaded";
        return <div className="hpp-subscription" key={s.id}><div><strong>{s.name}</strong><small>{s.sourceType === "url" ? s.url || "" : "ZIP 导入"}</small><p>{catStatuses[status] || status}{status === "failed" && plugin?.error?.message ? ` · ${plugin.error.message}` : ""}</p></div>
          <div className="hpp-distribution-buttons">
            <button className="plugin-enable-switch" role="switch" aria-checked={s.enabled} aria-label={"启用 " + s.name} disabled={busy} onClick={() => void run(async () => { await request("enable", { id: s.id, enabled: !s.enabled }); })}><span /></button>
            {s.sourceType === "url" ? <button disabled={busy} onClick={() => void run(async () => { const result = await request<{ changed: boolean }>("cat.check_update", { id: s.id }); return result.changed ? "已更新到最新版本" : "已是最新版本"; })}>检查更新</button>
              : <button disabled={busy} onClick={() => void run(async () => { const path = await pickZip(); if (!path) return; await request("cat.reimport_zip", { id: s.id, path }); return "已重新导入"; })}>重新导入 ZIP</button>}
            <button disabled={busy} onClick={() => void run(async () => { const paths = await invoke<string[]>("pick_plugin_files"); if (!paths[0]) return; await request("cat.import_config", { id: s.id, path: paths[0] }); return "配置已导入"; })}>导入配置</button>
            <button className="danger" disabled={busy} onClick={() => { if (confirmRemove !== s.id) { setConfirmRemove(s.id); return; } setConfirmRemove(null); void run(async () => { await request("remove", { id: s.id }); }); }}>{confirmRemove === s.id ? `确认删除「${s.name}」？` : "删除"}</button>
          </div>
        </div>;
      }) : <p className="hpp-muted">还没有添加猫源，贴订阅地址或导入 ZIP 文件添加。</p>}
    </section>
    {error && <div className="hpp-error" role="alert">{error}</div>}
    {notice && <p role="status" className="hpp-muted">{notice}</p>}
  </div>;
}
