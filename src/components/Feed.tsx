import React, { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { setMobileOrientation } from "../orientation";
import { setStatusBarHidden } from "../statusBar";
import { isMobile, platform } from "../environment";
import { fmtTime, formatResolution } from "../format";
import { useStore } from "../store";
import { PlaybackWindowActions } from "./DesktopChrome";
import { Sheet } from "./ui";
import VideoSurface, { type VideoHandle } from "./VideoSurface";
import PlayerControls from "./PlayerControls";
import PlaybackLock from "./PlaybackLock";
import { registerBackHandler } from "../back";
import { useVerticalSwipe } from './useVerticalSwipe';
import type { PreparedMedia } from "../plugins/client";
import { closeNativePlayer, onNativePlayerClosed, onNativePlayerEnded, onNativePlayerError, onNativePlayerTime, openNativePlayer } from "../plugins/nativePlayer";
import { playbackEngineFallbackChain, playbackEngineLabel, type PlaybackEngine } from "../plugins/playbackEngine";
import { IMore, IHeart, IHeartF, IPip, IBack, IBoltF, IChevR, IChevronDown, IChevronUp, IClose, ICommentF, ICompress, IExpand, IInfo, IList, ILockF, IPauseF, IPlayF, IRotate, ISearch, ISettings, IVol, IVolMute } from "../icons";

export type PlayerPanel = "detail" | "episodes" | "comments" | "playback";
type SurfacePanel = PlayerPanel | "more" | "info";
export interface FeedPresentation {
  playIntent?: number; mode?: "preview" | "full"; onOpenDetail?: () => void;
  title?: string; author?: string; description?: string; score?: number; poster?: string;
  episodeLabel?: string; qualityLabel?: string; episodeCount?: number; favorite?: boolean; onFavorite?: () => void;
  panels?: Partial<Record<PlayerPanel, ReactNode>>; social?: ReactNode;
  onPanelChange?: (panel: PlayerPanel) => void;
  onBack?: () => void; onNext?: () => void; onPrevious?: () => void; onNextEpisode?: () => void; onPreviousEpisode?: () => void;
  queueIndex?: number; total?: number;
  previousPreview?: { title: string; poster?: string }; nextPreview?: { title: string; poster?: string };
}
export interface FeedProps extends FeedPresentation {
  media: PreparedMedia | null; position: number; active?: boolean; preparing?: boolean; error?: string; onRetry?: () => void;
  onPosition: (t: number, duration?: number) => void; onFailure: () => void;
  onPlaying?: () => void; onStopped?: () => void; onEnded?: () => void;
  onBuffer?: (seconds: number) => void; onWaiting?: () => void;
}
/** Restored from 5f88442 Feed: shared desktop chrome and mobile gestures.
 * Plugin adapters own metadata, sessions and persistence; this component owns presentation. */
export default function Feed({media, position, active=true, preparing=false, error="", onRetry, onPosition, onFailure, onPlaying, onStopped, onEnded, onBuffer, onWaiting,
 title="", author="", description="", score, poster:posterUrl, episodeLabel="", qualityLabel, episodeCount=0, favorite=false, onFavorite,
 playIntent=0, mode="full", onOpenDetail, panels={}, social, onPanelChange, onBack, onNext, onPrevious, onNextEpisode, onPreviousEpisode, queueIndex=0, total=1, previousPreview, nextPreview}: FeedProps) {
  const store=useStore(), {muted,setMuted,volume,setVolume,playbackEngine,setPlaybackEngine}=store;
  const activeRef = useRef(active); activeRef.current = active;
  const isDesktop=!isMobile, preview=mode==="preview";
  const boxRef=useRef<HTMLDivElement>(null), videoRef=useRef<VideoHandle>(null);
  const lastWheel=useRef(0);
  const playKey=media ? `${media.sessionId}:${media.version}` : "pending", playUrl=media?.url;
  const [hint,setHint]=useState("");
  const toast=(message:string,_kind?:string)=>setHint(message);
  useEffect(()=>{if(!hint)return;const t=window.setTimeout(()=>setHint(""),2200);return()=>window.clearTimeout(t);},[hint]);
  const [subtitle,setSubtitle]=useState(-1);
  useEffect(()=>setSubtitle(media?.playback.subtitles?.filter(t=>t.format==="vtt").findIndex(t=>t.default)??-1),[playKey]);
  const [time, setTime] = useState(0);
  const [duration, setDuration] = useState(0);
  const [vsize, setVsize] = useState({ w: 0, h: 0 });
  const [buffered, setBuffered] = useState(0);
  const [playback, setPlayback] = useState<{ key: string; phase: 'loading' | 'playing' | 'paused' }>({ key: playKey, phase: 'loading' });
  const phase = preparing || playback.key !== playKey ? 'loading' : playback.phase;
  const paused = phase === 'paused', waiting = phase === 'loading';
  const setPhase = (phase: typeof playback.phase) => setPlayback({ key: playKey, phase });
  /* 分辨率角标：起播后显示 3 秒自动消失；切集重开一个 3 秒窗口。
     “原生播放”入口不在这里，见更多播放选项菜单。 */
  const [overlayBadgesVisible, setOverlayBadgesVisible] = useState(false);
  const overlayHideTimer = useRef(0);
  useEffect(() => { setOverlayBadgesVisible(false); window.clearTimeout(overlayHideTimer.current); }, [playKey]);
  useEffect(() => {
    if (phase !== "playing" || vsize.w <= 0 || vsize.h <= 0) { setOverlayBadgesVisible(false); window.clearTimeout(overlayHideTimer.current); return; }
    setOverlayBadgesVisible(true);
    window.clearTimeout(overlayHideTimer.current);
    overlayHideTimer.current = window.setTimeout(() => setOverlayBadgesVisible(false), 3000);
    return () => window.clearTimeout(overlayHideTimer.current);
  }, [phase, vsize.w, vsize.h]);
  const [controlsLocked, setControlsLocked] = useState(false);
  const [rate, setRate] = useState(1);
  const [err, setErr] = useState("");
  /* 取播放地址失败后手动重试：递增即重新触发拉取 */
  const [panel, setPanel] = useState<SurfacePanel | null>(null);
  useEffect(() => { if (panel && panel !== "more" && panel !== "info") onPanelChange?.(panel); }, [panel, onPanelChange]);
  const resumeAfterOverlay = useRef(false);
  useEffect(() => {
    if (!active) resumeAfterOverlay.current = !paused;
    else if (resumeAfterOverlay.current) { resumeAfterOverlay.current = false; videoRef.current?.play(); }
  }, [active]);
  useEffect(() => { if (playIntent && active) videoRef.current?.play(); }, [playIntent]);
  useEffect(() => { setPanel(null); if (active) store.setClean(false); wake(); }, [mode]);
  /* 续播：拿到时长后再 seek 一次（只对进入时那一条生效） */
  const theater = store.theater;
  const setTheater = store.setTheater;
  const fs = store.fs;
  const setFs = store.setFs;
  const clean = store.clean;
  const setClean = store.setClean;

  /* 首页清屏信息浮层（仅 iOS 预览卡片）：清屏时双击唤醒/关闭；15 秒无操作
     自动消失（复制地址或点旋转会续 15 秒，3 秒不够点到复制）；切集、
     退出清屏时关闭。 */
  const [infoOverlay, setInfoOverlay] = useState(false);
  const infoOverlayTimer = useRef(0);
  const pokeInfoOverlay = () => {
    if (!infoOverlay) return;
    window.clearTimeout(infoOverlayTimer.current);
    infoOverlayTimer.current = window.setTimeout(() => setInfoOverlay(false), 15000);
  };
  useEffect(() => { setInfoOverlay(false); }, [playKey]);
  useEffect(() => { if (!clean) setInfoOverlay(false); }, [clean]);
  useEffect(() => {
    if (!infoOverlay) { window.clearTimeout(infoOverlayTimer.current); return; }
    infoOverlayTimer.current = window.setTimeout(() => setInfoOverlay(false), 15000);
    return () => window.clearTimeout(infoOverlayTimer.current);
  }, [infoOverlay]);

  /* 追踪手机视口是否已处于物理/系统横屏状态（window.innerWidth > window.innerHeight） */
  const [vpLandscape, setVpLandscape] = useState(
    () => typeof window !== "undefined" && window.innerWidth > window.innerHeight,
  );

  /* 单击清屏 vs 双击暂停计时器 */
  const lastTapRef = useRef<{ time: number; timer: number | null }>({ time: 0, timer: null });
  const clearTapTimer = () => {
    if (lastTapRef.current.timer) {
      window.clearTimeout(lastTapRef.current.timer);
      lastTapRef.current.timer = null;
    }
  };

  /* 桌面：鼠标闲置 2.8s 自动隐藏播放器 UI（沉浸） */
  const [idle, setIdle] = useState(false);
  const idleTimer = useRef<number | undefined>(undefined);
  const wake = useCallback(() => {
    setIdle(false);
    window.clearTimeout(idleTimer.current);
    idleTimer.current = window.setTimeout(() => setIdle(true), 2800);
  }, []);
  useEffect(() => {
    if (!isDesktop) return undefined;
    wake();
    return () => window.clearTimeout(idleTimer.current);
  }, [isDesktop, wake]);
  useEffect(() => {
    if (paused) { setIdle(false); window.clearTimeout(idleTimer.current); }
  }, [paused]);
  const [fullscreen, setFullscreen] = useState(false);

  /* 详情页角标显隐：跟控制条同显同隐（移动端看清屏/锁定，桌面看闲置），
     起播/切集后的 3 秒窗口内也短暂显示一次。 */
  const detailUiVisible = !preview && active && !controlsLocked && !clean && (isDesktop ? (!idle || paused || !!panel) : true);
  const detailBadgesVisible = detailUiVisible || overlayBadgesVisible;

  /* —— 移动端手势 & 竖屏横片缩放 —— */
  const [holdSpeed, setHoldSpeed] = useState(false);
  const [speedLock, setSpeedLock] = useState(false);
  const [lockArmed, setLockArmed] = useState(false);
  const holdRef = useRef(false);
  const lockArmedRef = useRef(false);
  const [scrub, setScrub] = useState<{ pct: number; target: number } | null>(null);
  const scrubTarget = useRef<number | null>(null);
  /* 竖屏下横屏视频适应模式 + 双指无级缩放/平移 */
  const [portraitFit, setPortraitFit] = useState<"contain" | "cover">("contain");
  const [zoom, setZoom] = useState({ scale: 1, x: 0, y: 0, gesturing: false });
  const swipe = useVerticalSwipe(!isDesktop && active && !controlsLocked && !panel && zoom.scale <= 1.05, preview ? onPrevious : onPreviousEpisode, preview ? onNext : onNextEpisode);
  const pinchRef = useRef<{
    dist0: number; midX0: number; midY0: number; scale0: number; x0: number; y0: number;
  } | null>(null);
  /* 进度条拖动 */
  const barScrub = useRef(false);
  const [barPct, setBarPct] = useState<number | null>(null);
  const gesture = useRef<{
    x: number; y: number; startX: number; startY: number; t: number; timer: number | null;
    mode: "idle" | "hold" | "scrub" | "pan"; base: number; width: number; playing: boolean;
  } | null>(null);

  /* 倍速：长按 / 锁定 → 2.0x，否则用基准倍速 */
  const effRate = holdSpeed || speedLock ? 2 : rate;
  /* 进度条显示百分比（拖动时用拖动值） */
  const barProgress = barPct ?? (duration ? time / duration : 0);
  /* 是否为横屏视频（16:9 等宽大于高） */
  const isLandscapeVideo = vsize.w > vsize.h && vsize.h > 0;

  /* 借鉴 stclock1988/hghub-ios (PlayerView.swift keepPlayingThroughTransition)：
     iOS 在横竖屏几何旋转或全屏视图层级切换期间，系统底层 AVPlayer / WebKit 视频层可能被短暂挂起暂停。
     在过渡窗口期内分批重申 play()，确保旋转/全屏切换不中断播放。 */
  const keepPlayingTimers = useRef<number[]>([]);
  const keepPlayingThroughTransition = useCallback(() => {
    keepPlayingTimers.current.forEach((id) => window.clearTimeout(id));
    keepPlayingTimers.current = [];
    const el = videoRef.current?.el();
    const shouldPlay = el ? !el.paused : !paused;
    if (!shouldPlay) return;
    for (const delay of [50, 150, 300, 500, 800, 1200]) {
      const tid = window.setTimeout(() => {
        const v = videoRef.current?.el();
        if (v && v.paused && !v.ended) {
          videoRef.current?.play();
        }
      }, delay);
      keepPlayingTimers.current.push(tid);
    }
  }, [paused]);

  useEffect(() => () => {
    keepPlayingTimers.current.forEach((id) => window.clearTimeout(id));
  }, []);

  /* 监听手机屏幕横竖屏变化：物理横置手机时自动进入全屏，竖回时自动退出全屏 */
  useEffect(() => {
    if (isDesktop) return undefined;
    const onViewportChange = () => {
      const isLand = window.innerWidth > window.innerHeight;
      setVpLandscape((prev) => {
        if (prev !== isLand && active && !preview) {
          setFs(isLand);
          keepPlayingThroughTransition();
        }
        return isLand;
      });
    };
    window.addEventListener("resize", onViewportChange);
    window.addEventListener("orientationchange", onViewportChange);
    return () => {
      window.removeEventListener("resize", onViewportChange);
      window.removeEventListener("orientationchange", onViewportChange);
    };
  }, [isDesktop, active, preview, setFs, keepPlayingThroughTransition]);

  /* 横屏播放时隐藏 iOS 状态栏：时间、电量、网速等系统信息不再压在视频
     上方；回竖屏或离开播放页时恢复（cleanup 里恢复，卸载时也恢复）。
     只在 iOS 上调原生命令（setStatusBarHidden），其他平台没有这条
     命令；原生侧失败时静默，不影响播放。 */
  useEffect(() => {
    if (platform !== "ios") return undefined;
    if (!(vpLandscape && active)) return undefined;
    void setStatusBarHidden(true).catch(() => {});
    return () => { void setStatusBarHidden(false).catch(() => {}); };
  }, [platform, vpLandscape, active]);

  /* 进入 / 退出全屏：方向由上面的播放方向 effect 按分辨率自动处理，这里只切全屏状态 */
  const enterFs = useCallback(() => {
    keepPlayingThroughTransition();
    setClean(false);
    setFs(true);
  }, [setClean, setFs, keepPlayingThroughTransition]);

  const exitFs = useCallback(() => {
    keepPlayingThroughTransition();
    setClean(false);
    setFs(false);
  }, [setClean, setFs, keepPlayingThroughTransition]);

  /* 播放方向：默认按视频分辨率自动选横/竖屏（横屏视频→横屏，竖屏视频→竖屏），
     不再只在全屏时才转；旋转按钮可手动覆盖，当前视频内不再自动改回，
     切集后回到自动。自动失败不弹提示（移动端浏览器等环境本就不支持），
     只有手动旋转失败时才提示。 */
  const [orientationOverride, setOrientationOverride] = useState<boolean | null>(null);
  useEffect(() => { setOrientationOverride(null); }, [playKey]);
  useEffect(() => {
    if (isDesktop || preview || !active || !media) return;
    const known = vsize.w > 0 && vsize.h > 0;
    const target = orientationOverride ?? (known ? isLandscapeVideo : null);
    if (target === null) return;
    void setMobileOrientation(target).catch(() => {});
  }, [isDesktop, preview, active, media, vsize, isLandscapeVideo, orientationOverride]);

  /* 手动旋转：切到当前方向的反面，并覆盖本集的自动选择。
     “当前”必须在点按时读实时视口尺寸（window.innerWidth >
     window.innerHeight），不能读 vpLandscape state：程序触发旋转后
     resize 事件可能不来、或来时尺寸还是旧的，state 就一直停在竖屏，
     导致每次点都继续要求横屏、切不回竖屏（v0.0.23 录像里每次都提示
     “已切换到横屏”就是这个症状）。vpLandscape 只留给布局用。 */
  const rotateOrientation = useCallback(() => {
    keepPlayingThroughTransition();
    const next = !(window.innerWidth > window.innerHeight);
    setOrientationOverride(next);
    void setMobileOrientation(next)
      .then(() => setHint(next ? "已切换到横屏" : "已切换到竖屏"))
      .catch(() => setHint("无法切换屏幕方向，请重试"));
  }, [keepPlayingThroughTransition]);

  /* 影片信息里的格式：media.format 是 hls 还是 progressive；progressive 时
     从播放地址后缀推导（.m3u8 → HLS、.mp4 → MP4），拿不到的只写未知，
     不编造编码信息。 */
  const mediaFormatLabel = (() => {
    if (!media) return "未知";
    if (media.format === "hls") return "HLS";
    const ext = playUrl?.split(/[?#]/)[0].match(/\.([a-z0-9]+)$/i)?.[1]?.toLowerCase();
    if (!ext) return "未知";
    return ({ m3u8: "HLS", mp4: "MP4", mkv: "MKV", webm: "WebM", mov: "MOV", m4v: "M4V", ts: "TS" } as Record<string, string>)[ext] ?? ext.toUpperCase();
  })();

  /* 复制当前视频的真实播放地址（media.url）：WKWebView 里
     navigator.clipboard 需用户手势（点按即满足），不可用时走
     textarea + execCommand 兜底；失败给提示，不静默。 */
  const copyPlayUrl = async () => {
    if (!playUrl) return;
    const done = () => { toast("已复制，切出应用后地址会失效"); pokeInfoOverlay(); };
    try { await navigator.clipboard.writeText(playUrl); done(); return; } catch { /* 走兜底 */ }
    try {
      const ta = document.createElement("textarea");
      ta.value = playUrl;
      ta.style.position = "fixed"; ta.style.opacity = "0";
      document.body.appendChild(ta);
      ta.select();
      document.execCommand("copy");
      document.body.removeChild(ta);
      done();
    } catch { toast("复制失败"); }
  };


  const go=(delta:number)=>{ if(!active || controlsLocked || panel) return; if(preview) delta>0 ? onNext?.() : onPrevious?.(); else delta>0 ? onNextEpisode?.() : onPreviousEpisode?.(); };
  const onTimeTick=(t:number)=>{ if(!barScrub.current && gesture.current?.mode!=="scrub")setTime(t); onPosition(t,videoRef.current?.el()?.duration); };
  const onDurationTick=(d:number)=>setDuration(d);

  /* 原生播放器（iOS）：只在用户选了非 WebView 引擎时出现。VideoSurface
     本身不动；原生全屏接管后先暂停 WebView 避免双声，关闭/失败时把
     最新进度回写并回落 WebView 继续播。引擎存在 store 的 playbackEngine
     （localStorage hpp.playbackEngine），设置页 ApplicationSettings 已接选择器。 */
  const nativePosition = useRef(0);
  const nativeCallbacks = useRef({ onPosition, onEnded });
  nativeCallbacks.current = { onPosition, onEnded };
  const fallbackToWebView = (position: number) => {
    nativePosition.current = position;
    setTime(position);
    videoRef.current?.seek(position);
    videoRef.current?.play();
  };
  useEffect(() => {
    if (platform !== "ios") return;
    let listeners: Array<{ unregister: () => Promise<void> }> = [];
    let live = true;
    void Promise.all([
      onNativePlayerTime((e) => { nativePosition.current = e.position; setTime(e.position); nativeCallbacks.current.onPosition(e.position, e.duration); }),
      onNativePlayerEnded((e) => { nativePosition.current = e.position; nativeCallbacks.current.onEnded?.(); }),
      onNativePlayerError((e) => { toast(e.message || "原生播放失败，已回落 WebView"); fallbackToWebView(nativePosition.current); }),
      onNativePlayerClosed((e) => { fallbackToWebView(e.position); }),
    ]).then((u) => { if (live) listeners = u; else u.forEach((l) => void l.unregister()); });
    return () => { live = false; listeners.forEach((l) => void l.unregister()); };
  }, []);
  const openNative = async () => {
    if (!media || platform !== "ios") return;
    const chain = playbackEngineFallbackChain(playbackEngine, media.format);
    let lastError = "";
    for (const engine of chain) {
      try {
        await openNativePlayer({ engine, url: media.url, sessionId: media.sessionId, position: time, rate, volume, muted, title });
        videoRef.current?.pause();
        return;
      } catch (error) {
        lastError = error instanceof Error ? error.message : String(error);
      }
    }
    toast(lastError || "原生播放器不可用");
  };
  useEffect(() => () => { void closeNativePlayer().catch(() => {}); }, []);
  useEffect(()=>{setTime(position);setDuration(0);setErr("");setZoom({scale:1,x:0,y:0,gesturing:false});},[playKey]);
  useEffect(()=>()=>{clearTapTimer();if(activeRef.current){store.setClean(false);store.setFs(false);void setMobileOrientation(false).catch(()=>{});}},[]);
  useEffect(()=>{videoRef.current?.rate(effRate);},[effRate,playKey]);
  useEffect(()=>{if(!panel || !active)return;return registerBackHandler(()=>{setPanel(null);return true;},10);},[panel,active]);
  useEffect(()=>{const sync=()=>setFullscreen(!!document.fullscreenElement);document.addEventListener("fullscreenchange",sync);return()=>document.removeEventListener("fullscreenchange",sync);},[]);
  const toggleFull=()=>{const el=boxRef.current?.parentElement?.parentElement;if(document.fullscreenElement)void document.exitFullscreen();else void el?.requestFullscreen().catch(()=>toast("当前环境无法进入全屏"));};
  useEffect(()=>{
    if(!active || controlsLocked)return;
    const key=(e:KeyboardEvent)=>{
      if(e.defaultPrevented || document.querySelector("dialog[open]") || (e.target as HTMLElement).closest("input,textarea,select,button,[contenteditable=true]"))return;
      if(panel){if(e.key==="Escape"){e.preventDefault();setPanel(null);}return;}
      switch(e.key.toLowerCase()){
        case "arrowdown":e.preventDefault();go(1);break;case "arrowup":e.preventDefault();go(-1);break;
        case " ":e.preventDefault();videoRef.current?.toggle();break;
        case "arrowleft":e.preventDefault();videoRef.current?.seek(time-5);break;case "arrowright":e.preventDefault();videoRef.current?.seek(time+5);break;
        case "pageup":if(!preview){e.preventDefault();onPreviousEpisode?.();}break;case "pagedown":if(!preview){e.preventDefault();onNextEpisode?.();}break;
        case "m":setMuted(v=>!v);break;case "t":break;case "f":isDesktop?toggleFull():(fs?exitFs():enterFs());break;
        case "c":if(panels.comments)setPanel("comments");break;case "e":if(preview)onOpenDetail?.();else if(panels.episodes)setPanel("episodes");break;
      }
      wake();
    };window.addEventListener("keydown",key);return()=>window.removeEventListener("keydown",key);
  },[active,controlsLocked,panel,time,onNext,onPrevious,fs,preview,onNextEpisode,onPreviousEpisode]);
  const showDetails = () => { if (!isDesktop && fs) { setPanel('detail'); return; } setPanel(null); if (onOpenDetail) onOpenDetail(); else setPanel("detail"); };
  const panelContent=panel && <><header className="legacy-panel-heading"><h2>{{detail:"作品详情",episodes:"选集",comments:"评论",playback:"画质与线路",more:"更多播放选项",info:"影片信息"}[panel]}</h2><button aria-label="关闭面板" onClick={()=>setPanel(null)}><IClose size={18}/></button></header><div className="legacy-panel-body">
    {panel === "more" ? <div className="hg-more-menu">
      <div className="hg-more-shortcuts">
        <button onClick={showDetails}><IList size={22}/>作品详情</button>
        <button onClick={()=>setPanel("info")}><IInfo size={22}/>影片信息</button>
        {onFavorite && <button onClick={onFavorite} aria-label={favorite?"取消本地收藏":"本地收藏"} aria-pressed={favorite}>{favorite ? <IHeartF size={22}/> : <IHeart size={22}/>}收藏</button>}
        {panels.playback && <button onClick={()=>setPanel("playback")}><ISettings size={22}/>画质与线路</button>}
        {panels.comments && <button onClick={()=>setPanel("comments")}><ICommentF size={22}/>评论</button>}
      </div>
      <label>播放速度<select aria-label="播放速度" value={rate} onChange={e=>setRate(Number(e.target.value))}>{[.5,.75,1,1.25,1.5,2].map(v=><option key={v} value={v}>{v}×{v===1?" · 正常":""}</option>)}</select></label>
      <label>音量<input aria-label="音量" type="range" min="0" max="1" step=".01" value={muted?0:volume} onChange={e=>{setMuted(false);setVolume(Number(e.target.value));}}/></label>
      {platform === "ios" && <label>播放内核<select aria-label="播放内核" value={playbackEngine} onChange={e=>setPlaybackEngine(e.target.value as PlaybackEngine)}>{(["webview","avplayer","mdk","ksplayer","mpv","auto"] as const).map(engine=><option key={engine} value={engine}>{playbackEngineLabel(engine)}</option>)}</select></label>}
      {platform === "ios" && playbackEngine !== "webview" && <button onClick={()=>{setPanel(null);void openNative();}}><IPlayF size={19}/>原生播放</button>}
      <button onClick={()=>{setPanel(null);void togglePip();}}><IPip size={19}/>画中画</button>
      <button onClick={()=>{setPanel(null);lockControls();}}><ILockF size={18}/>锁定播放控制</button>
      <details><summary>操作说明</summary><p>{isDesktop ? "空格 播放/暂停 · ← → 快退/快进 · M 静音 · F 全屏 · E 选集" : "单击显示/隐藏控制 · 双击播放/暂停 · 横滑调整进度 · 长按临时倍速 · 双指缩放画面"}</p></details>
      {onBack&&<button onClick={onBack}><IBack size={19}/>退出播放</button>}
    </div> : panel === "info" ? <div className="flex flex-col gap-3">
      <div className="flex items-center justify-between gap-3"><span className="text-[13px] text-mut">分辨率</span><span className="text-[13px] font-semibold text-on tabular-nums">{vsize.w > 0 && vsize.h > 0 ? formatResolution(vsize.w, vsize.h) : "分辨率未知"}</span></div>
      <div className="flex items-center justify-between gap-3"><span className="text-[13px] text-mut">格式</span><span className="text-[13px] font-semibold text-on">{mediaFormatLabel}</span></div>
      <div className="flex flex-col items-start gap-2"><span className="text-[13px] text-mut">资源播放地址</span><span className="text-[12px] text-on break-all">{playUrl ?? "暂无播放地址"}</span><button onClick={() => { void copyPlayUrl(); }} disabled={!playUrl} className="mt-1 px-4 py-1.5 rounded-full text-[12.5px] font-semibold pressable disabled:opacity-40">复制播放地址</button></div>
    </div> : panels[panel]}
    {panel==="playback"&&media?.playback.subtitles?.some(t=>t.format==="vtt")&&<label className="flex flex-col gap-2 mt-4">字幕<select aria-label="字幕" value={subtitle} onChange={e=>{const n=Number(e.target.value);setSubtitle(n);const v=videoRef.current?.el();if(v)Array.from(v.textTracks).forEach((t,i)=>{t.mode=i===n?"showing":"disabled";});}}><option value={-1}>字幕关闭</option>{media.playback.subtitles.filter(t=>t.format==="vtt").map((t,i)=><option key={t.url} value={i}>{t.label||t.language||`字幕 ${i+1}`}</option>)}</select></label>}
  </div></>;
  /* —— 双指捏合缩放 & 平移（Pinch-to-Zoom & Pan） —— */
  const cancelSingleGesture = () => {
    swipe.cancel();
    clearTapTimer();
    const g = gesture.current;
    if (g?.timer) window.clearTimeout(g.timer);
    gesture.current = null;
    if (holdRef.current) {
      holdRef.current = false;
      setHoldSpeed(false);
    }
    lockArmedRef.current = false;
    setLockArmed(false);
    setScrub(null);
    scrubTarget.current = null;
  };

  const onTouchStart = (e: React.TouchEvent<HTMLDivElement>) => {
    if (isDesktop || controlsLocked || panel) return;
    if (e.touches.length === 2) {
      cancelSingleGesture();
      const t0 = e.touches[0];
      const t1 = e.touches[1];
      const dist0 = Math.max(12, Math.hypot(t1.clientX - t0.clientX, t1.clientY - t0.clientY));
      const midX0 = (t0.clientX + t1.clientX) / 2;
      const midY0 = (t0.clientY + t1.clientY) / 2;
      pinchRef.current = {
        dist0,
        midX0,
        midY0,
        scale0: zoom.scale,
        x0: zoom.x,
        y0: zoom.y,
      };
    }
  };

  const onTouchMove = (e: React.TouchEvent<HTMLDivElement>) => {
    if (isDesktop || controlsLocked) return;
    if (e.touches.length === 2 && pinchRef.current) {
      const p = pinchRef.current;
      const t0 = e.touches[0];
      const t1 = e.touches[1];
      const dist = Math.max(12, Math.hypot(t1.clientX - t0.clientX, t1.clientY - t0.clientY));
      const midX = (t0.clientX + t1.clientX) / 2;
      const midY = (t0.clientY + t1.clientY) / 2;
      const nextScale = Math.max(0.85, Math.min(3.5, p.scale0 * (dist / p.dist0)));
      const rawDx = midX - p.midX0;
      const rawDy = midY - p.midY0;
      const dx = rawDx;
      const dy = rawDy;
      const maxX = Math.max(0, (nextScale - 1) * window.innerWidth * 0.55);
      const maxY = Math.max(0, (nextScale - 1) * window.innerHeight * 0.55);
      const nextX = Math.max(-maxX, Math.min(maxX, p.x0 + dx));
      const nextY = Math.max(-maxY, Math.min(maxY, p.y0 + dy));
      setZoom({ scale: nextScale, x: nextX, y: nextY, gesturing: true });
    }
  };

  const onTouchEnd = (e: React.TouchEvent<HTMLDivElement>) => {
    if (isDesktop) return;
    if (pinchRef.current && e.touches.length < 2) {
      pinchRef.current = null;
      setZoom((prev) =>
        prev.scale < 1.08
          ? { scale: 1, x: 0, y: 0, gesturing: false }
          : { ...prev, gesturing: false },
      );
    }
  };

  /* —— 长按 2x（上滑锁定） / 横拖定位 / 单击清屏 / 双击暂停 —— */
  const onDown = (e: React.PointerEvent<HTMLDivElement>) => {
    if (isDesktop || controlsLocked || !active || panel || swipe.locked.current || !e.isPrimary || pinchRef.current || gesture.current || e.button !== 0) return;
    if ((e.target as HTMLElement).closest('button,a,input,select,textarea,[role=dialog]')) return;
    const el = e.currentTarget;
    const span = el.clientWidth || 1;
    try { el.setPointerCapture(e.pointerId); } catch { /* ignore */ }
    const g = {
      x: e.clientX, y: e.clientY, startX: e.clientX, startY: e.clientY,
      t: Date.now(), base: time, width: span,
      playing: videoRef.current?.el()?.paused === false,
      mode: "idle" as "idle" | "hold" | "scrub" | "pan",
      timer: window.setTimeout(() => {
        const cur = gesture.current;
        if (!cur || cur.mode !== "idle" || pinchRef.current) return;
        clearTapTimer();
        cur.mode = "hold";
        holdRef.current = true;
        setHoldSpeed(true);
        setLockArmed(false);
        lockArmedRef.current = false;
        try { navigator.vibrate?.(25); } catch { /* ignore */ }
      }, 380) as unknown as number,
    };
    gesture.current = g;
  };

  const onMove = (e: React.PointerEvent<HTMLDivElement>) => {
    if (pinchRef.current) {
      cancelSingleGesture();
      return;
    }
    const g = gesture.current;
    if (!g || isDesktop || !e.isPrimary) return;
    const rawDx = e.clientX - g.x;
    const rawDy = e.clientY - g.y;
    const dx = rawDx;
    const dy = rawDy;

    /* 长按中：上滑 64px 进入「松开即锁定」 */
    if (g.mode === "hold") {
      const upDist = g.startY - e.clientY;
      const armed = upDist > 64;
      if (armed !== lockArmedRef.current) {
        lockArmedRef.current = armed;
        setLockArmed(armed);
      }
      return;
    }

    if (g.mode === "idle") {
      if (Math.abs(dx) > 16 && Math.abs(dx) > Math.abs(dy) * 1.2) {
        clearTapTimer();
        if (g.timer) window.clearTimeout(g.timer);
        g.mode = "scrub";
      } else if (Math.abs(dy) > 16) {
        clearTapTimer();
        if (g.timer) window.clearTimeout(g.timer);
        g.mode = "pan";
        swipe.move(dy, boxRef.current?.clientHeight || window.innerHeight);
      }
      return;
    }

    if (g.mode === 'pan') swipe.move(dy, boxRef.current?.clientHeight || window.innerHeight);
    if (g.mode === "scrub") {
      const total = duration || 0;
      if (!total) return;
      const target = Math.max(0, Math.min(total - 0.2, g.base + (dx / g.width) * total));
      scrubTarget.current = target;
      setScrub({ pct: (target / total) * 100, target });
    }
  };

  const onUp = (e?: React.PointerEvent<HTMLDivElement>, cancelled = false) => {
    if (pinchRef.current) return;
    const g = gesture.current;
    gesture.current = null;
    if (!g) return;
    if (g.timer) window.clearTimeout(g.timer);

    if (g.mode === "hold") {
      holdRef.current = false;
      setHoldSpeed(false);
      if (!cancelled && lockArmedRef.current) {
        setSpeedLock(true);
        toast("已锁定 2.0x 倍速 · 点顶部标签解除", "info");
      }
      lockArmedRef.current = false;
      setLockArmed(false);
      return;
    }
    if (g.mode === "scrub") {
      const target = scrubTarget.current;
      setScrub(null);
      scrubTarget.current = null;
      if (!cancelled && target != null) {
        setErr("");
        setTime(target);
        videoRef.current?.seek(target, "mobile_gesture");
      }
      if (g.playing && active) videoRef.current?.play();
      return;
    }

    if (g.mode === 'pan') {
      if (!cancelled && e) swipe.finish(e.clientY - g.startY, Date.now() - g.t, boxRef.current?.clientHeight || window.innerHeight);
      else swipe.cancel();
      return;
    }
    if (cancelled || g.mode !== "idle") return;
    const moveDist = e ? Math.hypot(e.clientX - g.x, e.clientY - g.y) : 0;
    if (Date.now() - g.t < 260 && moveDist < 16) {
      const now = Date.now();
      if (now - lastTapRef.current.time < 280) {
        // 双击屏幕：仅首页预览卡片且已清屏时，唤醒/关闭信息浮层，不暂停播放；
        // 其他情况（非清屏、播放详情页）仍切换暂停 / 播放。单击的 250ms
        // 延迟计时器在第二下点按时已被 clearTapTimer 取消，所以一次双击
        // 不会触发两次单击的清屏切换。
        clearTapTimer();
        lastTapRef.current.time = 0;
        if (preview && clean) setInfoOverlay((v) => !v);
        else videoRef.current?.toggle();
      } else {
        // 单击屏幕：切换清屏模式（消除或恢复所有界面控件，只显示视频内容）
        lastTapRef.current.time = now;
        clearTapTimer();
        lastTapRef.current.timer = window.setTimeout(() => {
          lastTapRef.current.timer = null;
          setClean((c) => !c);
        }, 250) as unknown as number;
      }
    }
  };

  useEffect(() => {
    if (!active || controlsLocked) onUp(undefined, true);
    // Cancel pending gestures when the page is hidden or controls are locked.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [active, controlsLocked]);

  useEffect(() => () => {
    if (gesture.current?.timer) window.clearTimeout(gesture.current.timer);
    gesture.current = null;
  }, [playKey]);

  const lockControls = () => { setPanel(null); setControlsLocked(true); };

  /* 长按倍速 / 双指缩放 / 全屏期间禁止外层列表原生滚动 */
  useEffect(() => {
    if (isDesktop) return undefined;
    const el = boxRef.current;
    if (!el) return undefined;
    const fn = (e: TouchEvent) => {
      if ((e.target as HTMLElement).closest('[role=dialog]')) return;
      if (holdRef.current || pinchRef.current || fs || e.touches.length >= 2) {
        e.preventDefault();
      }
    };
    el.addEventListener("touchmove", fn, { passive: false });
    return () => el.removeEventListener("touchmove", fn);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [isDesktop, fs]);

  /* —— 进度条拖动：系统旋转后的视口仍直接使用 X 轴坐标 —— */
  const barPctAt = (barEl: HTMLElement | null, clientX: number, _clientY: number) => {
    const r = barEl?.getBoundingClientRect();
    if (!r) return null;
    const span = r.width;
    if (!span) return null;
    const raw = (clientX - r.left) / span;
    return Math.max(0, Math.min(1, raw));
  };
  const barSeekTo = (barEl: HTMLElement | null, clientX: number, clientY: number) => {
    const pct = barPctAt(barEl, clientX, clientY);
    if (pct == null) return null;
    setBarPct(pct);
    return pct;
  };
  const onBarDown = (e: React.PointerEvent<HTMLDivElement>) => {
    e.stopPropagation();
    barScrub.current = true;
    const el = e.currentTarget as HTMLElement;
    try { el.setPointerCapture(e.pointerId); } catch { /* ignore */ }
    barSeekTo(el, e.clientX, e.clientY);
  };
  const onBarMove = (e: React.PointerEvent<HTMLDivElement>) => {
    if (!barScrub.current) return;
    e.stopPropagation();
    barSeekTo(e.currentTarget as HTMLElement, e.clientX, e.clientY);
  };
  const onBarUp = (e: React.PointerEvent<HTMLDivElement>) => {
    if (!barScrub.current) return;
    barScrub.current = false;
    e.stopPropagation();
    const pct = barPctAt(e.currentTarget as HTMLElement, e.clientX, e.clientY) ?? barPct;
    setBarPct(null);
    if (pct != null && duration > 0) {
      const target = Math.max(0, Math.min(duration - 0.15, pct * duration));
      setErr("");
      setTime(target);
      videoRef.current?.seek(target, "mobile_bar");
    }
  };

  const togglePip = async () => {
    try {
      const v = videoRef.current?.el();
      if (document.pictureInPictureElement) await document.exitPictureInPicture();
      else if (v && document.pictureInPictureEnabled) await v.requestPictureInPicture();
      else toast("当前环境不支持画中画", "info");
    } catch { toast("画中画开启失败", "info"); }
  };

  const effectiveFit: "contain" | "cover" = isDesktop
    ? "contain"
    : isLandscapeVideo
      ? (fs || vpLandscape ? "contain" : portraitFit)
      : (vpLandscape ? "contain" : "cover");

  return (
    <div className="hg-swipe-viewport absolute inset-0 overflow-hidden bg-black">
      {!isDesktop && swipe.offset !== 0 && [previousPreview, nextPreview].map((item, i) => <div key={i} aria-hidden="true" className="hg-swipe-neighbor" style={{transform:'translate3d(0,' + (swipe.offset + (i ? swipe.height : -swipe.height)) + 'px,0)',transition:swipe.settling?'transform 220ms cubic-bezier(.2,.7,.2,1)':'none'}}>
        {(item?.poster || posterUrl) && <img src={item?.poster || posterUrl} alt=""/>}<strong>{item?.title || (i ? '加载下一条…' : '上一条')}</strong>
      </div>)}
    <div
      style={{touchAction:!isDesktop&&!panel?'none':undefined,transform:swipe.offset ? 'translate3d(0,' + swipe.offset + 'px,0)' : undefined,transition:swipe.settling?'transform 220ms cubic-bezier(.2,.7,.2,1)':'none'}}
      className={`hg-player-ui mode-${mode} absolute inset-0 overflow-hidden bg-black player-root ${controlsLocked ? "is-locked" : ""} ${isDesktop ? "player-surface" : ""} ${theater ? "is-theater" : ""} ${!preview && isDesktop && idle && !paused && !panel ? "is-idle" : ""}`}
      onTouchStart={onTouchStart} onTouchMove={onTouchMove} onTouchEnd={onTouchEnd} onTouchCancel={onTouchEnd}
      onPointerDown={onDown} onPointerMove={e => { if (isDesktop) wake(); else onMove(e); }}
      onPointerUp={e=>onUp(e,false)} onPointerCancel={e=>onUp(e,true)} onLostPointerCapture={e=>onUp(e,true)}>
      <div className="absolute inset-0" inert={controlsLocked}>
      {/* 氛围背景（全屏时隐藏，避免透出封面底图） */}
      {preview && !fs && (
        <div className="absolute inset-0 scale-110 blur-3xl opacity-25 pointer-events-none">
          {posterUrl && <img src={posterUrl} alt="" className="w-full h-full object-cover" />}
        </div>
      )}

      <div ref={boxRef} onWheel={e=>{
        if(!preview || !isDesktop || controlsLocked || panel || fs || Math.abs(e.deltaY)<35 || Date.now()-lastWheel.current<700 || (e.target as HTMLElement).closest("button,input,select"))return;
        lastWheel.current=Date.now();go(e.deltaY>0?1:-1);
      }}
        className="absolute inset-y-0 left-0 overflow-hidden"
        style={{ right: isDesktop && panel ? 372 : 0, transition: "right .3s cubic-bezier(.2,0,0,1)" }}>
        {[playKey].map((vid) => {
          const isActive = true;
          return (
            <section
              key="surface"
              style={!isActive && fs ? { display: "none" } : undefined}
              className={`relative w-full h-full snap-start snap-always ${fs && isActive ? "" : "overflow-hidden"}`}>
              {isActive && playUrl ? (
                <div className={!isDesktop && fs ? "fs-immersive" : "absolute inset-0"}>
                  <div
                    className="absolute inset-0 overflow-hidden"
                    style={{ touchAction: "none" }}>
                    <VideoSurface
                      ref={videoRef}
                      media={media!} position={position}
                      poster={fs ? undefined : posterUrl}
                      autoPlay
                      active={active}
                      
                      muted={muted}
                      volume={volume}
                      rate={effRate}
                      fit={effectiveFit}
                      scale={zoom.scale}
                      panX={zoom.x}
                      panY={zoom.y}
                      isGesturing={zoom.gesturing}
                      onClick={() => { if (isDesktop) videoRef.current?.toggle(); }}
                      onTime={onTimeTick}
                      onDuration={onDurationTick}
                      onSize={(w, h) => setVsize({ w, h })}
                      onPlaying={() => { setPhase('playing'); setErr(""); onPlaying?.(); }}
                      onPause={() => { setPhase('paused'); onStopped?.(); }}
                      onWaiting={() => { setPhase('loading'); onWaiting?.(); }}
                      onEnded={onEnded} onBuffered={setBuffered}
                      onBuffer={onBuffer}
                      onError={() => { setErr("播放失败"); onFailure(); }}
                    />
                  </div>
                  {/* 分辨率角标（仅 iOS 预览卡片）：起播后显示 3 秒自动消失；位置在标题栏下方（顶边 + 安全区 + 96px），不贴顶边。
                      信息浮层打开时隐藏，避免与浮层左边的分辨率 pill 在同一位置重叠。
                      详情页用下面那组左右角标，不走这里。 */}
                  {platform === "ios" && preview && playUrl && vsize.w > 0 && vsize.h > 0 && overlayBadgesVisible && !infoOverlay && (
                    <div className="absolute z-20 pointer-events-none rounded-full px-3 py-2 text-[13px] font-bold hg-ink tabular-nums backdrop-blur-md"
                      style={{ top: "calc(var(--sat, 0px) + 96px)", left: "var(--sal, 12px)", background: "rgba(0,0,0,.55)", boxShadow: "inset 0 0 0 1px rgba(255,255,255,.12)" }}>
                      {formatResolution(vsize.w, vsize.h)}
                    </div>
                  )}
                  {/* 详情页角标（仅 iOS）：分辨率在左、当前播放内核在右，与 3 秒角标
                      同款式，位置同样在标题栏下方（顶边 + 安全区 + 96px），左右
                      按横向安全区留白。显隐跟控制条同显同隐，起播/切集后的 3 秒
                      窗口内也短暂显示一次。 */}
                  {platform === "ios" && !preview && playUrl && vsize.w > 0 && vsize.h > 0 && detailBadgesVisible && (
                    <div className="absolute inset-x-0 z-20 pointer-events-none" style={{ top: "calc(var(--sat, 0px) + 96px)" }}>
                      <div className="flex items-start justify-between" style={{ paddingLeft: "var(--sal, 12px)", paddingRight: "var(--sar, 12px)" }}>
                        <span className="rounded-full px-3 py-2 text-[13px] font-bold hg-ink tabular-nums backdrop-blur-md"
                          style={{ background: "rgba(0,0,0,.55)", boxShadow: "inset 0 0 0 1px rgba(255,255,255,.12)" }}>
                          {formatResolution(vsize.w, vsize.h)}
                        </span>
                        <span className="rounded-full px-3 py-2 text-[13px] font-bold hg-ink backdrop-blur-md"
                          style={{ background: "rgba(0,0,0,.55)", boxShadow: "inset 0 0 0 1px rgba(255,255,255,.12)" }}>
                          {playbackEngineLabel(playbackEngine)}
                        </span>
                      </div>
                    </div>
                  )}
                  {/* 首页清屏信息浮层（仅 iOS 预览卡片）：清屏时双击唤醒。左为分辨率
                      （与 3 秒角标同一位置），右对称显示当前播放内核；下方只有
                      “复制播放地址”按钮（点按复制真实播放地址，不显示 URL 本身）
                      与横竖屏切换按钮。竖屏时两行左右按横向安全区（--sal/--sar）
                      留白；横屏时左边两行改取固定 230px：实测本机 WKWebView 里
                      横屏的 env(safe-area-inset-left) 是 0，--sal 只剩 12px，
                      角标仍贴在左边缘、落在刘海/黑边区（v0.0.24 以为横屏安全区
                      约 44px 是错的，见 v0.0.29 真机截图测量）。230px 是用户
                      横屏截图上红框的左沿位置，角标行从该位置起、落在视频区内。
                      容器本身不接指针事件，只有复制按钮与旋转按钮可点，其余点按
                      仍走视频手势（单击退出清屏会关掉浮层）。 */}
                  {platform === "ios" && preview && clean && infoOverlay && playUrl && (
                    <div className="absolute inset-x-0 z-20 pointer-events-none" style={{ top: "calc(var(--sat, 0px) + 96px)" }}>
                      <div className="flex items-start justify-between" style={{ paddingLeft: vpLandscape ? 230 : "var(--sal, 12px)", paddingRight: "var(--sar, 12px)" }}>
                        <span className="rounded-full px-3 py-2 text-[13px] font-bold hg-ink tabular-nums backdrop-blur-md"
                          style={{ background: "rgba(0,0,0,.55)", boxShadow: "inset 0 0 0 1px rgba(255,255,255,.12)" }}>
                          {vsize.w > 0 && vsize.h > 0 ? formatResolution(vsize.w, vsize.h) : "分辨率未知"}
                        </span>
                        <span className="rounded-full px-3 py-2 text-[13px] font-bold hg-ink backdrop-blur-md"
                          style={{ background: "rgba(0,0,0,.55)", boxShadow: "inset 0 0 0 1px rgba(255,255,255,.12)" }}>
                          {playbackEngineLabel(playbackEngine)}
                        </span>
                      </div>
                      <div className="mt-2 flex items-start gap-2" style={{ paddingLeft: vpLandscape ? 230 : "var(--sal, 12px)", paddingRight: "var(--sar, 12px)" }}>
                        <button onClick={() => { void copyPlayUrl(); }} aria-label="复制播放地址"
                          className="pointer-events-auto shrink-0 rounded-full px-3 py-2 backdrop-blur-md pressable"
                          style={{ background: "rgba(0,0,0,.55)", boxShadow: "inset 0 0 0 1px rgba(255,255,255,.12)" }}>
                          <span className="text-[13px] font-bold hg-ink">复制播放地址</span>
                        </button>
                        <button onClick={() => { rotateOrientation(); pokeInfoOverlay(); }} aria-label="切换横竖屏"
                          className="pointer-events-auto shrink-0 w-10 h-10 rounded-full backdrop-blur-md flex items-center justify-center pressable"
                          style={{ background: "rgba(0,0,0,.55)", boxShadow: "inset 0 0 0 1px rgba(255,255,255,.12)" }}>
                          <IRotate size={19} className="hg-ink" />
                        </button>
                      </div>
                    </div>
                  )}
                  {/* 横拖定位浮层 */}
                  {!isDesktop && scrub && (
                    <div className="absolute inset-0 z-30 flex flex-col items-center justify-center pointer-events-none" style={{ background: "rgba(0,0,0,.25)" }}>
                      <span className="text-[26px] font-bold hg-ink tabular-nums hg-ov-shadow">{fmtTime(scrub.target)}</span>
                      <span className="text-[11px] hg-ink-4 mt-1 tabular-nums">/ {fmtTime(duration)}</span>
                      <div className="mt-4 w-[62%] h-[3px] rounded-full overflow-hidden" style={{ background: "rgba(255,255,255,.25)" }}>
                        <div className="h-full bg-white" style={{ width: `${scrub.pct}%` }} />
                      </div>
                    </div>
                  )}

                  {/* 暂停 / 缓冲 / 报错恢复 */}
                  {paused && !err && (
                    <div className="absolute inset-0 z-10 flex items-center justify-center pointer-events-none">
                      <button aria-label="播放" onClick={()=>videoRef.current?.play()} className="pointer-events-auto w-[66px] h-[66px] rounded-full flex items-center justify-center backdrop-blur-md"
                        style={{ background: "rgba(0,0,0,.55)", boxShadow: "inset 0 0 0 1px rgba(255,255,255,.12)" }}>
                        <IPlayF size={28} className="hg-ink ml-0.5" />
                      </button>
                    </div>
                  )}
                  {waiting && !err && !paused && (
                    <div role="status" aria-label="正在加载视频" className="absolute inset-0 z-10 flex items-center justify-center pointer-events-none"><div className="w-9 h-9 spinner" /></div>
                  )}
                  {err && (
                    <div className="absolute inset-0 z-30 flex items-center justify-center pointer-events-none">
                      <div className="rounded-xl p-5 flex flex-col items-center gap-3 text-[13px] hg-ink-2 pointer-events-auto" style={{ background: "rgba(0,0,0,.74)" }}>
                        <span>视频暂时无法加载</span>
                        <span className="text-[10.5px] hg-ink-4 max-w-[240px] text-center break-all">{error || err}</span>
                        <button className="px-4 py-1.5 rounded-full hg-chip-strong hg-link text-[12.5px] font-semibold pressable"
                          onClick={() => { setErr(""); onRetry?.(); }}>
                          重新加载
                        </button>
                      </div>
                    </div>
                  )}

                  {/* 移动端：倍速状态 / 锁定提示 */}
                  {!isDesktop && (!clean || holdSpeed) && (holdSpeed || speedLock || rate !== 1) && (
                    <button
                      onClick={() => {
                        if (speedLock) { setSpeedLock(false); toast("已解除倍速锁定", "info"); }
                        else if (rate !== 1 && !holdSpeed) setRate(1);
                      }}
                      className={`absolute left-1/2 -translate-x-1/2 z-30 flex items-center gap-1.5 px-3.5 h-8 rounded-full backdrop-blur-md text-[12px] font-bold elev-2 pressable ${speedLock ? "bg-primary text-on-primary" : "hg-chip hg-gold"}`}
                      style={{ top: vpLandscape ? 14 : "calc(var(--sat, 0px) + 54px)" }}>
                      <IBoltF size={13} />
                      {effRate}x {speedLock ? "已锁定 · 点按解除" : holdSpeed ? "倍速中 · 松开恢复" : "倍速"}
                    </button>
                  )}
                  {!isDesktop && holdSpeed && (
                    <div
                      className={`absolute left-1/2 -translate-x-1/2 z-30 flex items-center gap-2 px-4 h-10 rounded-xl backdrop-blur-md elev-2 transition-all duration-200 ${lockArmed ? "bg-primary text-on-primary scale-110" : "hg-chip-strong hg-ink-2"}`}
                      style={{ top: "38%" }}>
                      <ILockF size={16} />
                      <span className="text-[12.5px] font-bold">{lockArmed ? "松开锁定 2.0x" : "上滑到此锁定倍速"}</span>
                    </div>
                  )}

                  {/* 移动端：沉浸全屏控制栏（清屏 clean 模式下隐藏，单击屏幕可呼出/隐藏） */}
                  {!isDesktop && fs && !clean && !controlsLocked && (
                    <>
                      <div className="absolute right-4 z-40 flex items-center gap-2"
                        style={{ top: vpLandscape ? 14 : "calc(var(--sat, 0px) + 8px)" }}>
                        <button onClick={lockControls} aria-label="锁定播放控制" title="锁定播放控制" aria-pressed={false}
                          className="hg-chip-strong w-9 h-9 rounded-full backdrop-blur-md flex items-center justify-center pressable elev-1">
                          <ILockF size={17} />
                        </button>
                        <button aria-label="选集" onClick={()=>setPanel("episodes")} className="hg-chip-strong w-9 h-9 rounded-full flex items-center justify-center"><IList size={18}/></button>
                        <button aria-label="更多" onClick={()=>setPanel("more")} className="hg-chip-strong w-9 h-9 rounded-full flex items-center justify-center"><IMore size={18}/></button>
                        <button onClick={exitFs} aria-label="退出全屏"
                          className="hg-chip-strong w-9 h-9 rounded-full backdrop-blur-md flex items-center justify-center pressable elev-1">
                          <ICompress size={18} />
                        </button>
                      </div>
                      {zoom.scale > 1.05 && (
                        <button
                          onClick={() => setZoom({ scale: 1, x: 0, y: 0, gesturing: false })}
                          className="hg-chip-strong absolute left-4 z-40 px-3 h-8 rounded-full backdrop-blur-md text-[11.5px] font-semibold flex items-center gap-1 pressable elev-1"
                          style={{ top: vpLandscape ? 14 : "calc(var(--sat, 0px) + 8px)" }}>
                          还原缩放 ({zoom.scale.toFixed(1)}x)
                        </button>
                      )}
                      <div className="absolute inset-x-0 z-30 px-4" style={{ bottom: vpLandscape ? 46 : "calc(var(--sab, 0px) + 52px)", height: 26 }}>
                        <div className="relative h-full" style={{ touchAction: "none" }}
                          onPointerDown={onBarDown} onPointerMove={onBarMove} onPointerUp={onBarUp} onPointerCancel={onBarUp}>
                          <div className={`absolute inset-x-0 bottom-[10px] rounded-full transition-all ${barPct !== null ? "h-[5px]" : "h-[3px]"}`}
                            style={{ background: "rgba(255,255,255,.3)" }}>
                            <div className="h-full rounded-full bg-white" style={{ width: `${barProgress * 100}%` }} />
                          </div>
                          {barPct !== null && (
                            <div className="absolute left-1/2 -translate-x-1/2 -top-8 px-2.5 py-1 rounded-md hg-chip-strong backdrop-blur-md text-[12px] font-bold tabular-nums whitespace-nowrap">
                              {fmtTime(barPct * (duration || 0))} / {fmtTime(duration)}
                            </div>
                          )}
                        </div>
                      </div>
                      <div className="absolute inset-x-0 z-40 flex items-center gap-3 px-4"
                        style={{ bottom: vpLandscape ? 10 : "calc(var(--sab, 0px) + 10px)" }}>
                        <button onClick={() => videoRef.current?.toggle()}
                          className="hg-chip shrink-0 w-9 h-9 rounded-full flex items-center justify-center pressable">
                          {paused ? <IPlayF size={18} /> : <IPauseF size={17} />}
                        </button>
                        <span className="shrink-0 text-[11.5px] font-semibold hg-ink-2 tabular-nums">{fmtTime(time)}</span>
                        <button
                          onClick={() => {
                            if (speedLock) { setSpeedLock(false); return; }
                            const seq = [1, 1.25, 1.5, 2];
                            const idx = seq.indexOf(rate);
                            setRate(seq[(idx + 1) % seq.length]);
                          }}
                          className={`ml-auto shrink-0 flex items-center gap-1 px-2.5 h-8 rounded-full text-[12px] font-bold pressable ${effRate !== 1 ? "bg-primary text-on-primary" : "hg-chip hg-ink-2"}`}>
                          <IBoltF size={12} />{effRate}x
                        </button>
                        <span className="shrink-0 text-[11.5px] hg-ink-4 tabular-nums">{fmtTime(duration)}</span>
                      </div>
                    </>
                  )}
                </div>
              ) : (
                <div className="absolute inset-0">
                  {posterUrl && <img src={posterUrl} alt="" className="w-full h-full object-contain"/>}{!error && <div className="absolute inset-0 flex items-center justify-center text-white/60 text-xs">正在准备播放…</div>}
                  <div className="absolute inset-0" style={{ background: "rgba(0,0,0,.25)" }} />
                </div>
              )}

              {/* 取不到播放地址时播放器不会创建，错误提示要放在播放器外面，否则只会停在封面 */}
              {isActive && !playUrl && !!error && (
                <div className="absolute inset-0 z-30 flex items-center justify-center pointer-events-none">
                  <div className="rounded-xl p-5 flex flex-col items-center gap-3 text-[13px] hg-ink-2 pointer-events-auto" style={{ background: "rgba(0,0,0,.74)" }}>
                    <span>视频暂时无法加载</span>
                    <span className="text-[10.5px] hg-ink-4 max-w-[240px] text-center break-all">{error || err}</span>
                    <button className="px-4 py-1.5 rounded-full hg-chip-strong hg-link text-[12.5px] font-semibold pressable"
                      onClick={() => { setErr(""); setPhase('loading'); onRetry?.(); }}>
                      重新加载
                    </button>
                  </div>
                </div>
              )}

              {/* 渐变压暗，保证文字可读（清屏模式下不加压暗遮罩，只显示纯视频内容） */}
              {preview && !fs && !clean && (
                <div className="absolute inset-0 pointer-events-none"
                  style={{ background: "linear-gradient(0deg, rgba(8,11,15,.86) 0, rgba(11,14,20,.18) 26%, transparent 48%)" }} />
              )}

              {/* 竖屏下观看横屏视频：快捷「放大铺满 / 完整显示 / 全屏横屏」与双指缩放复位胶囊 */}
              {isActive && !fs && !clean && !isDesktop && ((!preview && isLandscapeVideo) || zoom.scale > 1.05) && (
                <div
                  className="absolute left-1/2 -translate-x-1/2 z-20 flex items-center gap-2"
                  style={{ top: "64%" }}>
                  {!preview && isLandscapeVideo && (
                    <>
                      <button
                        type="button"
                        onClick={() => {
                          if (zoom.scale > 1.05) {
                            setZoom({ scale: 1, x: 0, y: 0, gesturing: false });
                            setPortraitFit("contain");
                          } else {
                            setPortraitFit((f) => (f === "contain" ? "cover" : "contain"));
                          }
                        }}
                        className="hg-chip-strong backdrop-blur-md px-3 h-7 rounded-full text-[11.5px] font-semibold flex items-center gap-1.5 pressable elev-1">
                        {zoom.scale > 1.05
                          ? `还原缩放 (${zoom.scale.toFixed(1)}x)`
                          : portraitFit === "contain"
                            ? "放大铺满"
                            : "完整适应"}
                      </button>
                      <button
                        type="button"
                        onClick={enterFs}
                        className="hg-chip-strong backdrop-blur-md px-3 h-7 rounded-full text-[11.5px] font-semibold flex items-center gap-1 pressable elev-1">
                        <IExpand size={12} />横屏全屏
                      </button>
                    </>
                  )}
                  {(!isLandscapeVideo || preview) && zoom.scale > 1.05 && (
                    <button
                      type="button"
                      onClick={() => setZoom({ scale: 1, x: 0, y: 0, gesturing: false })}
                      className="hg-chip-strong backdrop-blur-md px-3 h-7 rounded-full text-[11.5px] font-semibold flex items-center gap-1 pressable elev-1">
                      还原缩放 ({zoom.scale.toFixed(1)}x)
                    </button>
                  )}
                </div>
              )}

              {preview && !clean && <>
                <header className="hg-preview-heading"><div><span className="hg-preview-selected">为你推荐</span><span className="hg-preview-source">{author}</span></div><button aria-label="搜索" onClick={()=>store.setTab("search")}><ISearch size={21}/></button></header>
                <div className="hg-preview-actions">
                  <button aria-label={muted?"取消静音":"静音"} onClick={()=>setMuted(v=>!v)}><span>{muted?<IVolMute size={24}/>:<IVol size={24}/>}</span><small>{muted?"静音":"声音"}</small></button>
                  {onFavorite&&<button onClick={onFavorite} aria-label={favorite?"取消本地收藏":"本地收藏"} aria-pressed={favorite}><span>{favorite?<IHeartF size={25}/>:<IHeart size={25}/>}</span><small>{favorite?"已收藏":"收藏"}</small></button>}
                  <button aria-label="作品详情" onClick={showDetails}><span className="hg-info-glyph">ⓘ</span><small>详情</small></button>
                </div>
                <div className="hg-preview-copy"><h1>{title}</h1><div className="hg-preview-meta"><span>{author}</span>{score!==undefined&&<strong>评分 {score.toFixed(1)}</strong>}</div><button onClick={showDetails} className="hg-preview-synopsis"><span>{description || "发现下一部值得看的故事"}</span><b>展开</b></button></div>
                <div className="hg-preview-footer"><button className="hg-preview-watch" onClick={showDetails}><IPlayF size={17}/><span>观看完整作品{episodeCount ? ` · 共 ${episodeCount} 集` : ""}</span><IChevR size={20}/></button>
                  <input aria-label="预览进度" type="range" min={0} max={duration||1} step=".1" disabled={!duration} value={Math.min(time,duration||1)} onChange={e=>{videoRef.current?.seek(Number(e.target.value));setTime(Number(e.target.value));}} style={{"--progress":`${barProgress*100}%`} as React.CSSProperties}/>
                </div>
                {isDesktop&&<div className="hg-preview-pager"><button aria-label="上一个" disabled={!onPrevious} onClick={()=>go(-1)}><IChevronUp size={20}/></button><button aria-label="下一个" disabled={!onNext} onClick={()=>go(1)}><IChevronDown size={20}/></button><small>{queueIndex+1} / {total}</small></div>}
              </>}
              {!preview && !clean && !fs && !controlsLocked && (title || onBack) && <header className="hg-watch-heading"
                onMouseDown={e=>{if(isDesktop&&e.button===0&&!(e.target as HTMLElement).closest("button,input,select,a")){e.preventDefault();const win=getCurrentWindow();void (e.detail===2?win.toggleMaximize():win.startDragging()).catch(()=>{});}}}>
                {onBack&&<button aria-label="返回" onClick={onBack}><IBack size={22}/></button>}
                <div className="hg-watch-title"><h1>{title}</h1><p>{episodeLabel ? `${/^第/.test(episodeLabel)?episodeLabel:`第 ${episodeLabel} 集`}${episodeCount?` / 共 ${episodeCount} 集`:""}` : author}{qualityLabel ? ` · ${qualityLabel}` : ""}</p></div>
                {!isDesktop&&<><button aria-label={muted?"取消静音":"静音"} onClick={()=>setMuted(v=>!v)}>{muted?<IVolMute size={23}/>:<IVol size={23}/>}</button>{panels.episodes&&<button aria-label="选集" onClick={()=>setPanel("episodes")}><IList size={24}/></button>}<button aria-label="更多" onClick={()=>setPanel("more")}><IMore size={24}/></button></>}
                {isDesktop&&store.watching&&<PlaybackWindowActions/>}
              </header>}
            </section>
          );
        })}
      </div>

      {/* 桌面控制条 */}
      {!preview && (isDesktop || (!fs && !clean)) && (
        <PlayerControls
          paused={paused} time={time} duration={duration} buffered={buffered} rate={rate}
          muted={muted} volume={volume} theater={theater} fullscreen={fullscreen}
          visible={active && (!isDesktop || !idle || paused || !!panel)}
          seekEnabled={active && !controlsLocked}
          seekKey={playKey}
          nextDisabled={!onNextEpisode}
          nextTitle="下一集"
          onToggle={() => videoRef.current?.toggle()}
          onSeek={(t, source) => { videoRef.current?.seek(t, source); setTime(t); }}
          onRate={(r) => { setRate(r); videoRef.current?.rate(r); }}
          onMute={() => setMuted((m) => !m)}
          onVolume={(v) => setVolume(v)}
          favorite={favorite} onFavorite={onFavorite}
          onPip={togglePip}
          onLock={lockControls}
          onTheater={undefined}
          onFullscreen={isDesktop ? toggleFull : enterFs}
          onRotate={rotateOrientation}
          onNext={onNextEpisode} onPrevious={onPreviousEpisode}
          mobile={!isDesktop} onEpisodes={panels.episodes?()=>setPanel("episodes"):undefined} onMore={()=>setPanel("more")}
        />
      )}

      {/* 影片信息面板横屏时内容右移 330px：横屏下弹层是全宽的，内容贴左沿、
          用户要求移到截图红框位置（红框左沿约 330px）；只动 info 面板，
          其他面板与竖屏不受影响。 */}
      {isDesktop ? <div className={`desktop-dock-panel ${panel ? "is-open" : ""}`}><div className="desktop-dock-inner">{panelContent}</div></div> : <Sheet open={!!panel && active} z="z-[60]" onClose={() => setPanel(null)}><div role="dialog" aria-modal="true" aria-label="播放面板" className="hg-player-sheet flex flex-col" style={panel === "info" && vpLandscape ? { paddingLeft: 330 } : undefined}>{panelContent}</div></Sheet>}
      </div>
      {hint&&<div role="status" className="absolute top-1/3 left-1/2 -translate-x-1/2 z-50 px-4 py-2 rounded-full bg-black/70 text-white text-xs pointer-events-none">{hint}</div>}
      {controlsLocked && <PlaybackLock active={active} onUnlock={() => { setControlsLocked(false); wake(); }} />}
    </div>
    </div>
  );
}
