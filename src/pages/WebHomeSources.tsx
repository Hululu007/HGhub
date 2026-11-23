import { useState } from "react";
import { useHost } from "../plugins/HostContext";
import { pluginError } from "../plugins/client";
import { BrowseFrame, Notice } from "../plugins/BrowsePages";
import { openWebhome } from "../plugins/webhome";
import { IChevR } from "../icons";

/** 底部栏 Webhome 页：列出已装的 WebHome 源，点行打开首页，点另一行切换。 */
export default function WebHomeSources({ manage }: { manage: () => void }) {
  const { sources } = useHost();
  const webhome = sources.filter((p) => p.meta.webhome);
  const [opened, setOpened] = useState("");
  const [opening, setOpening] = useState("");
  const [error, setError] = useState("");
  const open = (id: string) => {
    setError("");
    setOpening(id);
    void openWebhome(id)
      .then(() => setOpened(id))
      .catch((e) => setError(pluginError(e)))
      .finally(() => setOpening(""));
  };
  return (
    <BrowseFrame title="Webhome" tools={<button onClick={manage}>管理插件<IChevR size={14} /></button>}>
      <Notice error={error} />
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
                disabled={!ready || opening !== ""} onClick={() => open(p.meta.id)} aria-label={p.meta.name}>
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
    </BrowseFrame>
  );
}
