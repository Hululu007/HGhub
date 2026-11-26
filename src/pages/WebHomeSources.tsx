import { useEffect, useState } from "react";
import { useHost } from "../plugins/HostContext";
import { pluginError } from "../plugins/client";
import { BrowseFrame, CategoryOverview, Notice, type OpenMedia } from "../plugins/BrowsePages";
import { usePluginRequests } from "../plugins/usePluginRequests";
import { openWebhome } from "../plugins/webhome";
import { IChevR } from "../icons";

const CAT_STATUS_LABEL: Record<string, string> = { failed: "运行失败", incompatible: "不兼容" };

/** 底部栏 Node源面板：webhome 分组列出已装的 WebHome 源，点行打开首页，点另一行切换；猫源分组是猫源的资源浏览（与之前分类里的猫源同一套视图：选资源 → 分类 → 资源列表），订阅管理（添加/启停/删除/配置）只在插件中心的“猫源”分栏。 */
export default function WebHomeSources({ manage, open }: { manage: () => void; open: OpenMedia }) {
  const { sources, plugins } = useHost();
  const request = usePluginRequests();
  const webhome = sources.filter((p) => p.meta.webhome);
  const cats = sources.filter((p) => p.meta.runtime === "cat" && p.methods.includes("catalog.getCategories"));
  // 停用的猫源不在这里出现：只在插件中心管理，Node源只列能用的和
  // 出了问题的（运行失败/不兼容）；停用的连状态块都不留。
  const catsUnavailable = plugins.filter((p) => p.meta.runtime === "cat" && (p.status === "failed" || p.status === "incompatible") && !cats.some((a) => a.meta.id === p.meta.id));
  const [node, setNode] = useState<{ running: boolean; version?: string } | null>(null);
  useEffect(() => { let live = true; void request<{ node: { running: boolean; version?: string } }>("cat.list").then((r) => { if (live) setNode(r.node); }).catch(() => {}); return () => { live = false; }; }, [request]);
  const [opened, setOpened] = useState("");
  const [opening, setOpening] = useState("");
  const [error, setError] = useState("");
  const openHome = (id: string) => {
    setError("");
    setOpening(id);
    void openWebhome(id)
      .then(() => setOpened(id))
      .catch((e) => setError(pluginError(e)))
      .finally(() => setOpening(""));
  };
  return (
    <BrowseFrame title="Node源" tools={<button onClick={manage}>管理插件<IChevR size={14} /></button>}>
      <Notice error={error} />
      {node && <p className="hpp-muted">{node.running ? `Node 运行中${node.version ? ` · ${node.version}` : ""}` : "Node 已停止"}</p>}
      <section aria-label="webhome">
        <div className="hpp-section-title"><h2>webhome</h2></div>
        {webhome.length === 0 ? (
          <div className="hpp-empty">
            <h2>还没有 WebHome 源</h2>
            <button className="primary" onClick={manage}>管理插件</button>
          </div>
        ) : (
          <div className="refined-source-list">
            {webhome.map((p) => {
              const ready = p.enabled && p.status === "ready";
              return (
                <button key={p.meta.id} className="refined-source-row" style={{ width: "100%", textAlign: "left", opacity: ready ? undefined : 0.45 }}
                  disabled={!ready || opening !== ""} onClick={() => openHome(p.meta.id)} aria-label={p.meta.name}>
                  <span className="refined-glyph">{p.meta.name.slice(0, 1)}</span>
                  <span>
                    <strong>{p.meta.name}</strong>
                    <small>v{p.meta.version}{!p.enabled ? " · 未启用" : p.status !== "ready" ? " · 未就绪" : ""}</small>
                  </span>
                  {opened === p.meta.id && <span className="refined-source-check">当前打开</span>}
                </button>
              );
            })}
          </div>
        )}
      </section>
      <section aria-label="猫源">
        <div className="hpp-section-title"><h2>猫源</h2></div>
        {!cats.length && !catsUnavailable.length ? (
          <div className="hpp-empty">
            <h2>还没有猫源</h2>
            <button className="primary" onClick={manage}>管理插件</button>
          </div>
        ) : (
          <>
            {cats.map((p) => (
              <section key={p.meta.id} aria-label={p.meta.name}>
                <div className="hpp-section-title"><h2>{p.meta.name}</h2></div>
                <CategoryOverview plugin={p} open={open} />
              </section>
            ))}
            {catsUnavailable.map((p) => (
              <div key={p.meta.id} className="hpp-empty">
                <h2>{p.meta.name} · {CAT_STATUS_LABEL[p.status] ?? p.status}</h2>
                <p>{p.status === "disabled" ? "该源已在插件中心停用，启用后即可在这里使用。" : p.error ? pluginError(p.error) : "加载失败，原因未知。"}</p>
                <button className="primary" onClick={manage}>管理插件</button>
              </div>
            ))}
          </>
        )}
      </section>
    </BrowseFrame>
  );
}
