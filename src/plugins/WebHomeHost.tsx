import { useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import { useStore } from "../store";
import Feed from "../components/Feed";
import { pluginError, releaseMedia, type PreparedMedia } from "./client";
import { usePluginRequests } from "./usePluginRequests";
import { registerBackHandler } from "../back";

/** WebHome 主窗口侧（08 §10）：收 Rust 转发的页面动作。
 * 直链播放在主窗口用同一套播放器（Feed）播，详情/直播/收藏/设置走现有导航。 */
export default function WebHomeHost() {
  const app = useStore();
  const appRef = useRef(app);
  appRef.current = app;
  const request = usePluginRequests();
  const [direct, setDirect] = useState<{ media: PreparedMedia; title?: string } | null>(null);
  const [directKey, setDirectKey] = useState(0);
  const [directActive, setDirectActive] = useState(true);
  const [playIntent, setPlayIntent] = useState(0);
  const [seekTo, setSeekTo] = useState<number | undefined>(undefined);
  const [directError, setDirectError] = useState("");
  const [toast, setToast] = useState("");
  const directRef = useRef(direct);
  directRef.current = direct;
  const lastPos = useRef({ t: 0, d: 0 });
  const lastPush = useRef(0);

  const pushState = (playing: boolean, force = false) => {
    const now = Date.now();
    if (!force && now - lastPush.current < 5000) return;
    lastPush.current = now;
    void invoke("webhome_player_state", {
      playing,
      positionSec: lastPos.current.t,
      durationSec: lastPos.current.d,
    }).catch(() => {});
  };

  const closeDirect = () => {
    const current = directRef.current;
    if (!current) return;
    releaseMedia(current.media.sessionId);
    setDirect(null);
    setDirectError("");
    setDirectActive(true);
    void invoke("webhome_player_state", { playing: false, positionSec: 0, durationSec: 0 }).catch(() => {});
  };

  const retryDirect = () => {
    const current = directRef.current;
    if (!current) return;
    setDirectError("");
    void request<PreparedMedia>("media.refresh", {
      sessionId: current.media.sessionId,
      version: current.media.version,
    })
      .then(result => {
        setDirect({ media: result, title: current.title });
        setDirectKey(k => k + 1);
        setPlayIntent(i => i + 1);
      })
      .catch(e => setDirectError(pluginError(e)));
  };

  useEffect(() => {
    if (!toast) return;
    const t = window.setTimeout(() => setToast(""), 2200);
    return () => window.clearTimeout(t);
  }, [toast]);

  useEffect(() => {
    if (!direct) return;
    return registerBackHandler(() => {
      closeDirect();
      return true;
    }, 20);
  }, [direct]);

  useEffect(() => {
    let disposed = false;
    const unlistens: (() => void)[] = [];
    const on = async <T,>(event: string, fn: (payload: T) => void) => {
      try {
        const un = await listen<T>(event, e => {
          if (!disposed) fn(e.payload);
        });
        unlistens.push(un);
      } catch {
        /* 非 Tauri 环境无事件总线，WebHome 入口本就不可用。 */
      }
    };
    void (async () => {
      await on<{ media: PreparedMedia; title?: string }>("webhome-play", ({ media, title }) => {
        setDirect({ media, title });
        setDirectActive(true);
        setPlayIntent(i => i + 1);
        setDirectError("");
      });
      await on<{ pluginId: string; detail: { id: string } }>("webhome-open-vod", ({ pluginId, detail }) => {
        appRef.current.push({ kind: "media", pluginId, mediaId: detail.id });
      });
      await on<{ message: string }>("webhome-toast", ({ message }) => setToast(message));
      await on<{ pluginId: string }>("webhome-open-live", ({ pluginId }) => {
        appRef.current.push({ kind: "live", pluginId });
      });
      await on("webhome-open-keep", () => appRef.current.setTab("profile"));
      await on("webhome-open-setting", () => appRef.current.push({ kind: "settings" }));
      await on<{ action: string; seconds?: number | null }>("webhome-player-control", ({ action, seconds }) => {
        if (!directRef.current) return;
        if (action === "stop") {
          closeDirect();
          return;
        }
        if (action === "pause") {
          setDirectActive(false);
          pushState(false, true);
          return;
        }
        if (action === "play") {
          setDirectActive(true);
          setPlayIntent(i => i + 1);
          return;
        }
        if (action === "toggle") {
          setDirectActive(a => {
            if (a) {
              pushState(false, true);
              return false;
            }
            setPlayIntent(i => i + 1);
            return true;
          });
          return;
        }
        if (action === "seek" && typeof seconds === "number") {
          setSeekTo(seconds);
          window.setTimeout(() => setSeekTo(undefined), 500);
        }
      });
    })();
    return () => {
      disposed = true;
      unlistens.forEach(u => u());
    };
  }, []);

  return (
    <>
      {toast && (
        <div role="status" className="hg-hint absolute top-1/3 left-1/2 -translate-x-1/2 z-[60] px-4 py-2 rounded-full bg-black/70 text-white text-xs pointer-events-none">
          {toast}
        </div>
      )}
      {direct && (
        <div className="absolute inset-0 z-50 bg-black">
          <Feed
            key={`${direct.media.sessionId}:${direct.media.version}:${directKey}`}
            media={direct.media}
            position={0}
            title={direct.title ?? ""}
            mode="full"
            active={directActive}
            playIntent={playIntent}
            seekTo={seekTo}
            error={directError}
            onRetry={retryDirect}
            onBack={closeDirect}
            onEnded={closeDirect}
            onPlaying={() => pushState(true, true)}
            onStopped={() => pushState(false, true)}
            onPosition={(t, d) => {
              lastPos.current = { t, d: d ?? 0 };
              pushState(true);
            }}
            onFailure={() => setDirectError("播放失败")}
          />
        </div>
      )}
    </>
  );
}
