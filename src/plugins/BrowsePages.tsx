import { isMobile } from "../environment";
import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useHost } from "./HostContext";
import { pluginError, type InstalledPlugin } from "./client";
import { usePluginRequests } from "./usePluginRequests";
import type { Category, Filter, FilterOption, MediaItem, Page, Section } from "./protocol";
import MediaCards, {MediaRow} from "./MediaCards";
import type { OpenSection } from "./SectionPage";
import { mergeSearch, type SearchHit } from "./searchModel";
import { PageLoadMore, usePagedPlugin } from "./usePagedPlugin";
import SearchPosters from "./SearchPosters";
import { CAT_ICONS, IGrid, IChevR, IBack, ISearch, IClose, ISettings, IFireF, IFilter } from "../icons";

// Cache fulfilled metadata only. A view owns its in-flight request and may cancel it.
const categoryTrees = new Map<string, { revision: string; tree: Category[] }>();

export type OpenMedia = (pluginId:string,mediaId:string)=>void;
export function BrowseFrame({title,children,tools}:{title:string;children:React.ReactNode;tools?:React.ReactNode}) {
  return <div className="hpp-page"><header className="hpp-header"><div><h1>{title}</h1></div><div className="hpp-header-tools">{tools}</div></header><div className="hpp-scroll"><div className="hpp-content">{children}</div></div></div>;
}
export function Notice({error}:{error:string}){return error?<p role="alert" className="hpp-error">{error}</p>:null;}
export function EmptySources({manage}:{manage:()=>void}) {return <div className="hpp-empty"><h2>从一个插件开始</h2><p>导入你选择的数据源，发现影视、短剧与更多内容。</p><button className="primary" onClick={manage}>管理插件</button></div>;}
function Scope({kind,plugins}:{kind:"home"|"search";plugins:InstalledPlugin[]}) {
  const {prefs,setPrefs}=useHost(),mode=kind==="home"?prefs.homeMode:prefs.searchMode,ids=kind==="home"?prefs.homeIds:prefs.searchIds;
  const update=(mode:string,ids:string[])=>setPrefs(kind==="home"?{homeMode:mode as typeof prefs.homeMode,homeIds:ids}:{searchMode:mode as typeof prefs.searchMode,searchIds:ids});
  return <div className="hpp-scope"><div className="refined-scope-segments" aria-label="搜索范围"><span>搜索范围</span>{([["all","综合"],["single","单个来源"],["multiple","多个来源"]] as const).map(([value,label])=><button key={value} aria-pressed={mode===value} onClick={()=>update(value,ids.length?ids:plugins.slice(0,1).map(p=>p.meta.id))}>{label}</button>)}</div><label className="refined-scope-select">{kind==="home"?"首页来源":"搜索范围"}<select aria-label={kind==="home"?"首页来源":"搜索范围"} value={mode} onChange={e=>update(e.target.value,ids.length?ids:plugins.slice(0,1).map(p=>p.meta.id))}><option value="all">综合</option><option value="single">单个插件</option><option value="multiple">多个插件</option></select></label>{mode!=="all"&&plugins.map(p=><label key={p.meta.id}><input type={mode==="single"?"radio":"checkbox"} name={`${kind}-scope`} checked={ids.includes(p.meta.id)} onChange={e=>update(mode,mode==="single"?[p.meta.id]:e.target.checked?[...ids,p.meta.id]:ids.filter(id=>id!==p.meta.id))}/>{p.meta.name}</label>)}</div>;
}
export function PluginRecommendations({plugin,open,openSection,onBack}:{plugin:InstalledPlugin;open:OpenMedia;openSection:OpenSection;onBack:()=>void}) {
  const [refresh,setRefresh]=useState(0);
  return <BrowseFrame title={plugin.meta.name + " · 推荐分区"} tools={<><button onClick={()=>setRefresh(n=>n+1)}>刷新推荐</button><button onClick={onBack}>返回</button></>}><HomeSource plugin={plugin} open={open} openSection={openSection} refreshToken={refresh}/></BrowseFrame>;
}
function HomeSource({plugin:p,open,openSection,refreshToken}:{plugin:InstalledPlugin;open:OpenMedia;openSection:OpenSection;refreshToken:number}) {
  const list=usePagedPlugin<Section>({plugin:p,method:"home.getSections",limit:20}),lastRefresh=useRef(refreshToken);
  useEffect(()=>{if(lastRefresh.current!==refreshToken){lastRefresh.current=refreshToken;list.refresh();}},[refreshToken]);
  return <section className="hpp-home-source" aria-label={p.meta.name+"的推荐"}><Notice error={list.error||list.warning}/>{list.error&&<button onClick={list.retry}>重试 {p.meta.name}</button>}{list.busy&&!list.page&&<div className="hpp-skeleton" aria-label="正在加载推荐"/>}
    {list.page?.items.filter(s=>s.items.length>0).map(s=><HomeSection key={s.id} plugin={p} section={s} open={open} openSection={openSection}/>)}
    {!list.busy&&!list.error&&list.page&&!list.page.items.some(s=>s.items.length>0)&&<p className="hpp-muted">暂无推荐内容</p>}
    {list.page?.next&&<PageLoadMore next={list.page.next} busy={list.busy} error={list.error} more={list.more} label={"更多推荐分区 · "+p.meta.name}/>}
  </section>;
}
function HomeSection({plugin:p,section:s,open,openSection}:{plugin:InstalledPlugin;section:Section;open:OpenMedia;openSection:OpenSection}) {
  return <section className={`hpp-section hpp-layout-${s.layout}`}><div className="hpp-section-title"><h2>{s.title||"推荐"}<small>{p.meta.name}</small></h2>{s.more&&p.methods.includes("home.listSection")&&<button onClick={()=>openSection(p.meta.id,s.id,s.title)}>查看更多</button>}</div><MediaCards pluginId={p.meta.id} items={s.items} landscape={s.layout==="banner"||s.imageAspect==="landscape"} open={i=>open(p.meta.id,i.id)}/></section>;
}
export function FilterControls({filters,values,change}:{filters:Filter[];values:Record<string,string[]>;change:(v:Record<string,string[]>)=>void}) {
  return <div className="hpp-filters browse-filters">{filters.map(f=><fieldset key={f.id}><legend>{f.label}</legend>{f.options.map(o=><label key={o.id}><input type={f.multiple?"checkbox":"radio"} name={`filter-${f.id}`} checked={(values[f.id]||[]).includes(o.id)} onChange={e=>change({...values,[f.id]:f.multiple?e.target.checked?[...(values[f.id]||[]),o.id]:(values[f.id]||[]).filter(id=>id!==o.id):[o.id]})}/>{o.label}</label>)}<button type="button" aria-pressed={!values[f.id]?.length} onClick={()=>change({...values,[f.id]:[]})}>不限</button></fieldset>)}</div>;
}
const CATEGORY_STATUS_LABEL: Record<"failed" | "disabled" | "incompatible", string> = { failed: "运行失败", disabled: "已停用", incompatible: "不兼容" };
// 分类标签里嵌着 http(s) 链接的不是内容分类，是源自带的链接入口：
// 猫源配置站的 /home 只回一个 class，type_name 就是本机 /website 地址
// （端口随每次加载变）。点按直接在应用内浏览器打开，不调 catalog.list
// ——调了也只会拿到扫码配置/点击配置/弹幕服务三张动作卡片，点开详情是空的。
const CATEGORY_URL_RE = /https?:\/\/[^\s"']+/;
function categoryUrl(c: Category): string | null {
  return c.label.match(CATEGORY_URL_RE)?.[0] ?? null;
}
function openCategoryUrl(url: string) {
  void invoke("browser_open", { url }).catch(() => {});
}
// 分类片源按类型归类（与插件中心“已安装”分组同一口径，见 src/pages/Plugins.tsx）。
// 猫源不进分类：已移到 Node 源面板（底部栏），见 src/pages/WebHomeSources.tsx。
// 顺序按用户说法：插件、Python、HTML。
type CategoryGroupKey = "plugin" | "python" | "html";
const CATEGORY_GROUP_TITLE: Record<CategoryGroupKey, string> = { plugin: "插件", python: "Python", html: "HTML" };
function categoryGroupOf(p: InstalledPlugin): CategoryGroupKey {
  if (p.meta.runtime === "python") return "python";
  if (p.meta.webhome?.localPage) return "html";
  return "plugin";
}
export function PluginCategory({open,manage,openContent}:{open:OpenMedia;manage:()=>void;openContent?:(kind:"live"|"articles"|"feed"|"recommendations",pluginId:string)=>void}) {
  const {sources,plugins}=useHost(),available=sources.filter(p=>p.meta.runtime!=="cat"&&["catalog.getCategories","home.getSections","live.resolve","articles.list","feed.list"].some(m=>p.methods.includes(m))),[selected,setSelected]=useState("");
  // 不可用但不应静默消失的内容源：失败/停用/不兼容，带状态列进分组里。
  // 纯 WebHome 页源（只有 homePage）本来就不进分类，不列；待加载（unloaded）无错可报，也不列。
  // 猫源也不进分类（已移到 Node 源面板），这里一并排除。
  const unavailable=plugins.filter(p=>p.meta.runtime!=="cat"&&p.meta.kind!=="resolver" && !(p.meta.webhome?.homePage && !p.meta.webhome?.localPage) && (p.status==="failed"||p.status==="disabled"||p.status==="incompatible") && !available.some(a=>a.meta.id===p.meta.id));
  const groups=(["plugin","python","html"] as const).map(key=>({ key, title: CATEGORY_GROUP_TITLE[key], items: [...available, ...unavailable].filter(p=>categoryGroupOf(p)===key).map(p=>({ p, selectable: available.some(a=>a.meta.id===p.meta.id) })) })).filter(g=>g.items.length);
  const listed=[...available, ...unavailable];
  const current=listed.find(p=>p.meta.id===selected)||available[0]||unavailable[0];
  const currentSelectable=!!current && available.some(a=>a.meta.id===current.meta.id);
  return <div className={"hpp-page hpp-category-page refined-page"+(isMobile?" refined-mobile":"")}><header className="shrink-0 px-4 pb-2" style={{paddingTop:"calc(var(--sat,0px) + 14px)"}}><h1 className="text-[24px] font-bold text-on leading-none">分类</h1></header><CategorySourceTree groups={groups} selectedId={current?.meta.id} select={setSelected}/><div className="hpp-scroll"><div className="hpp-content">
    {current && currentSelectable && openContent && <nav className="hpp-content-entries" aria-label="其他内容">{([['recommendations','home.getSections','推荐分区'],['live','live.resolve','直播'],['articles','articles.list','图文'],['feed','feed.list','信息流']] as const).filter(([,method])=>current.methods.includes(method)).map(([kind,,label])=><button key={kind} aria-label={label+" · "+current.meta.name} onClick={()=>openContent(kind,current.meta.id)}><span className="category-entry-icon"><IGrid size={18}/></span><span><strong>{label}</strong><small>{current.meta.name}</small></span><IChevR size={16}/></button>)}</nav>}
    {current?.methods.includes("catalog.getCategories")?<CategoryOverview key={current.meta.id+current.revision} plugin={current} open={open}/>:current&&!currentSelectable?<SourceStatusPanel plugin={current} manage={manage}/>:!current?<EmptySources manage={manage}/>:<p className="hpp-muted">选择上方内容入口开始浏览。</p>}
  </div></div></div>;
}
function SourceStatusPanel({plugin,manage}:{plugin:InstalledPlugin;manage:()=>void}) {
  const label=CATEGORY_STATUS_LABEL[plugin.status as "failed"|"disabled"|"incompatible"] ?? plugin.status;
  return <div className="hpp-empty"><h2>{plugin.meta.name} · {label}</h2>{plugin.status==="disabled"?<p>该源已在插件中心停用，启用后即可在分类里使用。</p>:<p>{plugin.error?pluginError(plugin.error):"加载失败，原因未知。"}</p>}<button className="primary" onClick={manage}>管理插件</button></div>;
}
// 分类片源选择器：树状。平时只显示当前插件（点它展开/收起）；右上角
// 搜索图标展开按名称搜插件，筛选图标只留可用插件。分组点开才出插件。
function CategorySourceTree({groups,selectedId,select}:{groups:{title:string;items:{p:InstalledPlugin;selectable:boolean}[]}[];selectedId?:string;select:(id:string)=>void}) {
  const all=groups.flatMap(g=>g.items);
  const current=all.find(i=>i.p.meta.id===selectedId)?.p;
  const currentGroup=groups.find(g=>g.items.some(i=>i.p.meta.id===selectedId));
  const [open,setOpen]=useState(false);
  const [openGroups,setOpenGroups]=useState<Record<string,boolean>>(()=>currentGroup?{[currentGroup.title]:true}:{});
  const [showSearch,setShowSearch]=useState(false);
  const [query,setQuery]=useState("");
  const [onlyAvailable,setOnlyAvailable]=useState(false);
  const inputRef=useRef<HTMLInputElement>(null);
  useEffect(()=>{if(showSearch)inputRef.current?.focus();},[showSearch]);
  useEffect(()=>{const g=groups.find(g=>g.items.some(i=>i.p.meta.id===selectedId));if(g)setOpenGroups(old=>({...old,[g.title]:true}));},[selectedId]);
  if(!groups.length)return null;
  const q=query.trim().toLowerCase();
  const visible=groups.map(g=>({...g,items:g.items.filter(({p,selectable})=>(!onlyAvailable||selectable)&&(!q||p.meta.name.toLowerCase().includes(q)))})).filter(g=>g.items.length);
  const searching=!!q;
  const currentSelectable=!!current&&all.some(i=>i.p.meta.id===current.meta.id&&i.selectable);
  const currentStatus=!current||currentSelectable?null:(CATEGORY_STATUS_LABEL[current.status as "failed"|"disabled"|"incompatible"] ?? current.status);
  const pick=(id:string)=>{select(id);setOpen(false);setQuery("");setShowSearch(false);};
  return <div className="hpp-category-source-tree">
    <div className="hpp-source-tree-head">
      <button className="hpp-source-tree-current" aria-expanded={open} onClick={()=>setOpen(v=>!v)}>
        <span className="category-source-glyph" aria-hidden="true">{current?current.meta.name.slice(0,1):"源"}</span>
        <span className="hpp-source-tree-current-name">{current?current.meta.name:"选择片源"}</span>
        {currentStatus&&<small className={"hpp-status "+current?.status}>{currentStatus}</small>}
        <IChevR size={16} className="hpp-source-tree-chev"/>
      </button>
      <div className="hpp-source-tree-tools">
        <button aria-label="搜索插件" aria-pressed={showSearch} onClick={()=>{setShowSearch(v=>!v);setOpen(true);}}><ISearch size={17}/></button>
        <button aria-label="只显示可用插件" aria-pressed={onlyAvailable} onClick={()=>setOnlyAvailable(v=>!v)}><IFilter size={17}/></button>
      </div>
    </div>
    {showSearch&&<div className="hpp-source-tree-search"><ISearch size={15}/><input ref={inputRef} aria-label="按名称搜索插件" placeholder="搜索插件名称…" value={query} onChange={e=>setQuery(e.target.value)}/>{query&&<button aria-label="清除搜索" onClick={()=>setQuery("")}><IClose size={13}/></button>}</div>}
    {open&&<div className="hpp-source-tree-panel">
      {!visible.length?<p className="hpp-muted">没有找到相关插件</p>:visible.map(g=>{
        const expanded=searching||!!openGroups[g.title];
        return <div key={g.title} className="hpp-source-tree-group">
          <button className="hpp-source-tree-group-row" aria-expanded={expanded} onClick={()=>setOpenGroups(old=>({...old,[g.title]:!old[g.title]}))}><IChevR size={14} className="hpp-source-tree-chev"/><strong>{g.title}</strong><small>{g.items.length} 个</small></button>
          {expanded&&g.items.map(({p,selectable})=><button key={p.meta.id} aria-pressed={selectedId===p.meta.id} className={"hpp-source-tree-item"+(selectedId===p.meta.id?" selected":"")} onClick={()=>pick(p.meta.id)}><span className="category-source-glyph" aria-hidden="true">{p.meta.name.slice(0,1)}</span><span className="hpp-source-tree-item-name">{p.meta.name}</span>{!selectable&&<small className={"hpp-status "+p.status}>{CATEGORY_STATUS_LABEL[p.status as "failed"|"disabled"|"incompatible"] ?? p.status}</small>}</button>)}
        </div>;
      })}
    </div>}
  </div>;
}
export function CategoryOverview({plugin,open}:{plugin:InstalledPlugin;open:OpenMedia}) {
  const request=usePluginRequests(),[tree,setTree]=useState<Category[]>([]),[selected,setSelected]=useState<Category|null>(null),[error,setError]=useState(""),[busy,setBusy]=useState(true),[attempt,setAttempt]=useState(0),[sitesOpen,setSitesOpen]=useState(false);
  useEffect(()=>{let live=true;setBusy(true);setError("");const cached=categoryTrees.get(plugin.meta.id);const load=cached?.revision===plugin.revision?Promise.resolve(cached.tree):request<Category[]>("call",{id:plugin.meta.id,method:"catalog.getCategories",args:plugin.meta.runtime==="cat"?{shallow:true}:{}});void load.then(tree=>{if(live){categoryTrees.set(plugin.meta.id,{revision:plugin.revision,tree});setTree(tree);if(tree.length===1&&Array.isArray(tree[0].children))setSelected(tree[0]);}}).catch(e=>{if(live)setError(pluginError(e));}).finally(()=>{if(live)setBusy(false);});return()=>{live=false;request.cancelAll();};},[plugin.meta.id,plugin.revision,attempt,request]);
  // 顶层节点带子节点时（猫源按站分组后的站节点），概览不再把所有子分类
  // 的预览一次全拉出来：先选资源，切到某个资源后它的分类才出来。
  // 手机端左侧目录是隐藏的，所以在内容区放一份资源列表；桌面端用左侧目录。
  // 站节点以 children 字段在不在区分（浅层列表里 children 是空数组）。
  const hasParents=tree.some(c=>Array.isArray(c.children));
  const pickEntry=(c:Category)=>{const url=categoryUrl(c);if(url){openCategoryUrl(url);return;}const kids=c.children??[];if(kids.length===1){const childUrl=categoryUrl(kids[0]);if(childUrl){openCategoryUrl(childUrl);return;}}setSelected(c);};
  // 进入资源分类页后，右滑返回资源列表。只在已选资源时启用；不限从左边缘
  // 起手（分类详情里没有横向滚动的内容，不会误触）；横向占主导就触发，
  // 不要求竖向几乎不动——拇指右滑常带一点下弧。
  // v0.0.110 诊断定案：日志里两次右滑都只有 [swipe] start，没有 move/end/cancel——
  // React 的触摸回调是被动的（passive），调 preventDefault 无效，浏览器在起手后
  // 几 px 内就把手势接管去纵滑，touchmove 再也不到 JS。所以改原生监听：
  // touchmove 用 {passive:false}，横向占主导（dx>10）就 preventDefault 抢回手势，
  // dx>30 即返回；竖向占主导时不拦，页面照常滚动。v0.0.112 只修好一半：
  // 从屏幕中间起手的右滑能回，靠左缘起手的没反应——监听挂在布局 div 上，
  // 而 .hpp-content 两侧各有 16px 留白，左缘起手落在留白里，根本到不了监听
  // （唯一成功的那笔 start x=17，正好在 16px 线外 1px）。修法见 refined-pages.css：
  // 布局 div 用负 margin 盖住两侧留白。[swipe] 诊断行先留着，下一版还要验。
  // v0.0.112 诊断：IMG_6437 的左缘慢速右滑仍只有 start——快竖滑有 end 行，
  // 慢横滑连 touchmove 都没有。主窗口没开原生边缘返回手势（只 WebHome 有），
  // 真凶是布局 div 的 touch-action: pan-y：浏览器见横向手势不在允许方向，
  // 直接截断触摸序列（且不发 touchcancel）。改为 pan-x pan-y，让横向
  // touchmove 也到 JS，由 preventDefault 判定；竖滑仍不拦，照常滚动。
  const layoutRef=useRef<HTMLDivElement|null>(null);
  const swipe=useRef<{x:number;y:number;t:number;done:boolean;mx:number;my:number}|null>(null);
  const selectedRef=useRef(selected);
  useEffect(()=>{selectedRef.current=selected;});
  useEffect(()=>{
    const el=layoutRef.current; if(!el) return;
    const log=(m:string)=>{void invoke("debug_log",{msg:`[swipe] ${m}`}).catch(()=>{});};
    const onStart=(e:TouchEvent)=>{if(!selectedRef.current)return;const t=e.touches[0];swipe.current={x:t.clientX,y:t.clientY,t:performance.now(),done:false,mx:0,my:0};log(`start x=${Math.round(t.clientX)} y=${Math.round(t.clientY)}`);};
    const onMove=(e:TouchEvent)=>{const s=swipe.current;if(!s||s.done||!selectedRef.current)return;const t=e.touches[0],dx=t.clientX-s.x,dy=t.clientY-s.y;s.mx=Math.max(s.mx,Math.abs(dx));s.my=Math.max(s.my,Math.abs(dy));if(dx>10&&dx>Math.abs(dy))e.preventDefault();if(dx>30&&dx>Math.abs(dy)){s.done=true;log(`move back dx=${Math.round(dx)} dy=${Math.round(dy)}`);setSelected(null);}};
    const onEnd=(e:TouchEvent)=>{const s=swipe.current;swipe.current=null;if(!s||s.done||!selectedRef.current)return;const t=e.changedTouches[0],dx=t.clientX-s.x,dy=t.clientY-s.y,dt=Math.max(1,performance.now()-s.t);const back=dx>Math.abs(dy)&&(dx>80||(dx>40&&dx/dt>.5));if(s.mx>20||s.my>20)log(`end dx=${Math.round(dx)} dy=${Math.round(dy)} mx=${Math.round(s.mx)} my=${Math.round(s.my)} -> ${back?'back':'no'}`);if(back)setSelected(null);};
    const onCancel=()=>{const s=swipe.current;swipe.current=null;if(s)log(`cancel mx=${Math.round(s.mx)} my=${Math.round(s.my)}`);};
    el.addEventListener('touchstart',onStart,{passive:true});
    el.addEventListener('touchmove',onMove,{passive:false});
    el.addEventListener('touchend',onEnd);
    el.addEventListener('touchcancel',onCancel);
    return()=>{el.removeEventListener('touchstart',onStart);el.removeEventListener('touchmove',onMove);el.removeEventListener('touchend',onEnd);el.removeEventListener('touchcancel',onCancel);};
  },[]);
  const content=selected?<><button className="hpp-back" onClick={()=>setSelected(null)}><IBack size={17}/>全部分类</button><h2>{selected.label}</h2><CategorySource key={selected.id} plugin={plugin} open={open} initialCategory={selected.id} siteKey={Array.isArray(selected.children)?selected.id:undefined}/></>:<><Notice error={error}/>{error&&<button onClick={()=>setAttempt(v=>v+1)}>重试分类</button>}{busy?<div className="hpp-skeleton" aria-label="正在加载分类"/>:hasParents?<>{isMobile?<div className="hpp-category-sites" aria-label="选择资源"><button className="hpp-category-sites-toggle" aria-expanded={sitesOpen} onClick={()=>setSitesOpen(v=>!v)}><span><strong>选择资源</strong><small>{tree.length} 个资源</small></span><IChevR size={14}/></button>{sitesOpen&&tree.map(site=><button key={site.id} onClick={()=>pickEntry(site)}><span><strong>{site.label}</strong><small>{site.children?.map(child=>child.label).join(" · ")}</small></span><IChevR size={14}/></button>)}</div>:<p className="hpp-muted">从左侧分类目录选择一个资源，查看它的分类。</p>}</>:tree.map(category=><CategoryPreview key={category.id} plugin={plugin} category={category} open={open} all={()=>pickEntry(category)}/>)}{!busy&&!error&&!tree.length&&<div className="hpp-empty"><IGrid size={36}/><h2>这里还没有分类</h2><p>片源暂未提供分类内容。</p></div>}{!!tree.length&&<p className="hpp-end-note">— 已展示全部分类 —</p>}</>;
  return <div ref={layoutRef} className="refined-category-layout" style={{touchAction:"pan-x pan-y"}}><aside className="refined-category-directory"><h2>分类目录</h2><nav aria-label="分类目录"><button aria-pressed={!selected} onClick={()=>setSelected(null)}><IGrid size={18}/>全部概览</button>{tree.map(c=><button key={c.id} aria-pressed={selected?.id===c.id} onClick={()=>pickEntry(c)}><span><strong>{c.label}</strong><small>{c.children?.map(child=>child.label).join(" · ")}</small></span><IChevR size={14}/></button>)}</nav></aside><div className="refined-category-results">{content}</div></div>;
}
function CategoryPreview({plugin,category,open,all}:{plugin:InstalledPlugin;category:Category;open:OpenMedia;all:()=>void}) {
  const list=usePagedPlugin<MediaItem>({plugin,method:"catalog.list",args:{categoryId:category.children?.[0]?.id||category.id},limit:8});
  const Icon=CAT_ICONS[category.label]||IGrid;
  return <section className="hpp-category-preview legacy-category-preview mb-5 a-rise" aria-label={category.label}>
    <button onClick={all} className="legacy-category-heading w-full flex items-center gap-2.5 mb-2.5 text-left pressable group"><span className="w-[36px] h-[36px] rounded-(--r-sm) tonal flex items-center justify-center shrink-0 text-primary"><Icon size={19} strokeWidth={1.9}/></span><span className="flex-1 min-w-0"><span className="block text-[15.5px] font-semibold text-on leading-tight">{category.label}</span><span className="block text-[10.5px] text-mut mt-0.5 truncate">{category.children?.map(c=>c.label).join(" · ")||plugin.meta.name}</span></span><span className="flex items-center gap-0.5 text-[11.5px] text-mut">全部<IChevR size={14}/></span></button>
    <Notice error={list.error}/>{list.error&&<button onClick={list.retry}>重试 {category.label}</button>}
    {list.busy&&!list.page?<div className="hpp-preview-skeleton">{Array.from({length:6},(_,i)=><span className="hpp-skeleton" key={i}/>)}</div>:<div className="hpp-preview-strip scroll-x flex gap-2.5 snap-x"><MediaRow pluginId={plugin.meta.id} items={list.page?.items||[]} open={item=>open(plugin.meta.id,item.id)}/></div>}
    {!list.busy&&!list.error&&!list.page?.items.length&&<><p className="hpp-muted">暂无内容</p><button onClick={list.retry}>重试 {category.label}</button></>}
  </section>;
}
function CategorySource({plugin:p,open,initialCategory,siteKey}:{plugin:InstalledPlugin;open:OpenMedia;initialCategory?:string;siteKey?:string}) {
  const metadata=usePluginRequests(),[tree,setTree]=useState<Category[]>([]),[category,setCategory]=useState(""),[error,setError]=useState(""),[filters,setFilters]=useState<Filter[]>([]),[sorts,setSorts]=useState<FilterOption[]>([]),[values,setValues]=useState<Record<string,string[]>>({}),[sort,setSort]=useState("");
  const list=usePagedPlugin<MediaItem>({plugin:p,method:"catalog.list",args:{categoryId:category,filters:values,sort:sort||undefined},enabled:!!category});
  // 带 siteKey 时只取该站的分类（浅层树里没有子节点，不能复用概览的缓存）。
  const cacheKey=siteKey?p.meta.id+"/"+siteKey:p.meta.id;
  useEffect(()=>{let live=true;const cached=categoryTrees.get(cacheKey);const load=cached?.revision===p.revision?Promise.resolve(cached.tree):metadata<Category[]>("call",{id:p.meta.id,method:"catalog.getCategories",args:siteKey?{site:siteKey}:{}});void load.then(r=>{if(live){categoryTrees.set(cacheKey,{revision:p.revision,tree:r});setTree(r);const selected=r.find(c=>c.id===initialCategory),firstContent=r.find(c=>!categoryUrl(c));const firstUsable=selected?.children?.find(ch=>!categoryUrl(ch))?.id,firstContentUsable=firstContent?.children?.find(ch=>!categoryUrl(ch))?.id;const initialInTree=r.some(c=>c.id===initialCategory)||r.some(c=>c.children?.some(ch=>ch.id===initialCategory));setCategory(firstUsable||(initialInTree?initialCategory:"")||firstContentUsable||firstContent?.children?.[0]?.id||firstContent?.id||"");}}).catch(e=>{if(live)setError(pluginError(e));});return()=>{live=false;};},[p.meta.id,p.revision,metadata,siteKey]);
  const pickCategory=(c:Category)=>{const url=categoryUrl(c);if(url){openCategoryUrl(url);return;}setCategory(c.id);};
  useEffect(()=>{let live=true;setValues({});setSort("");const c=tree.flatMap(c=>[c,...c.children||[]]).find(c=>c.id===category);setFilters(c?.filters||[]);setSorts(c?.sorts||[]);if(category&&p.methods.includes("catalog.getFilters"))void metadata<{filters?:Filter[];sorts?:FilterOption[]}>("call",{id:p.meta.id,method:"catalog.getFilters",args:{categoryId:category}}).then(r=>{if(live){setFilters(r.filters||[]);setSorts(r.sorts||[]);}}).catch(e=>{if(live)setError(pluginError(e));});return()=>{live=false;};},[category]);
  // 详情里的分类树只展示当前站的子分类，不把所有站的分类一次全铺出来。
  const scope=tree.find(c=>c.id===initialCategory)??tree.find(c=>c.children?.some(ch=>ch.id===initialCategory));
  const shown=scope?.children?.length?scope.children:tree;
  return <><div className="hpp-category-tree">{shown.map(c=><div key={c.id}><button aria-pressed={category===c.id} className={category===c.id?"selected":""} onClick={()=>pickCategory(c)}>{c.label}</button>{c.children?.map(child=><button key={child.id} aria-pressed={category===child.id} className={category===child.id?"selected":""} onClick={()=>pickCategory(child)}>{child.label}</button>)}</div>)}</div><FilterControls filters={filters} values={values} change={setValues}/>{!!sorts.length&&<label>排序 <select aria-label="分类排序" value={sort} onChange={e=>setSort(e.target.value)}><option value="">默认</option>{sorts.map(s=><option value={s.id} key={s.id}>{s.label}</option>)}</select></label>}<Notice error={error||list.error||list.warning}/>{list.error&&<button onClick={list.retry}>重试分类</button>}{list.busy&&!list.page?<div className="hpp-skeleton"/>:<MediaCards pluginId={p.meta.id} items={list.page?.items||[]} open={i=>open(p.meta.id,i.id)}/>}{category&&!list.busy&&!list.error&&!list.page?.items.length&&<><p className="hpp-muted">暂无内容</p><button onClick={list.retry}>重试分类</button></>}<PageLoadMore next={list.page?.next} busy={list.busy} error={list.error} more={list.more}/></>;
}
export function PluginSearch({open,manage}:{open:OpenMedia;manage:()=>void}) {
  const {sources,prefs}=useHost(),request=usePluginRequests(),metadata=usePluginRequests(),suggestRequest=usePluginRequests(),[query,setQuery]=useState(""),[submitted,setSubmitted]=useState(""),[pages,setPages]=useState<Record<string,Page<MediaItem>>>({}),[errors,setErrors]=useState<Record<string,string>>({}),[busy,setBusy]=useState<Record<string,boolean>>({}),[tab,setTab]=useState("all"),[choice,setChoice]=useState<SearchHit[]|null>(null),[suggest,setSuggest]=useState<string[]>([]),[trending,setTrending]=useState<Record<string,string[]>>({}),[filters,setFilters]=useState<Filter[]>([]),[values,setValues]=useState<Record<string,string[]>>({}),epoch=useRef(0);
  const locks=useRef(new Map<string,number>()), consumed=useRef(new Map<string,Set<string>>()), failedCursors=useRef(new Map<string,string|undefined>()), searchFilters=useRef<Record<string,string[]>>({});
  const [warnings,setWarnings]=useState<Record<string,string>>({});
  const [viewMode,setViewMode]=useState<"grid"|"detail">("grid");
  const [showScope,setShowScope]=useState(false),inputRef=useRef<HTMLInputElement>(null);
  const [history,setHistory]=useState<string[]>(()=>{try{const saved=JSON.parse(localStorage.getItem("hpp.searchHistory")||"[]");return Array.isArray(saved)?[...new Set<string>(saved.filter((v:unknown):v is string=>typeof v==="string"&&!!v.trim()))].slice(0,20):[];}catch{return [];}});
  useEffect(()=>{try{localStorage.setItem("hpp.searchHistory",JSON.stringify(history));}catch{/* Search remains usable if storage is unavailable. */}},[history]);
  useEffect(()=>{if(!/Android|iPhone|iPad/i.test(navigator.userAgent))inputRef.current?.focus();},[]);

  const available=sources.filter(p=>p.methods.includes("search.search")),chosen=prefs.searchMode==="all"?available:prefs.searchIds.flatMap(id=>available.find(p=>p.meta.id===id)||[]).slice(0,prefs.searchMode==="single"?1:undefined),ids=chosen.map(p=>p.meta.id+p.revision).join("|");
  useEffect(()=>{let live=true;setTrending({});for(const p of chosen)if(p.methods.includes("search.trending"))void metadata<string[]>("call",{id:p.meta.id,method:"search.trending",args:{}}).then(r=>{if(live)setTrending(old=>({...old,[p.meta.id]:r}));}).catch(()=>{});return()=>{live=false;};},[ids]);
  useEffect(()=>{let live=true;setFilters([]);setValues({});const p=prefs.searchMode==="single"?chosen[0]:undefined;if(p?.methods.includes("search.getFilters"))void metadata<Filter[]>("call",{id:p.meta.id,method:"search.getFilters",args:{}}).then(r=>{if(live)setFilters(r);}).catch(()=>{});return()=>{live=false;};},[ids,prefs.searchMode]);
  useEffect(()=>{let live=true;setSuggest([]);const p=prefs.searchMode==="single"?chosen[0]:undefined;const timer=window.setTimeout(()=>{if(query.trim()&&p?.methods.includes("search.suggest"))void suggestRequest<string[]>("call",{id:p.meta.id,method:"search.suggest",args:{query}}).then(r=>{if(live)setSuggest(r);}).catch(()=>{});},300);return()=>{live=false;clearTimeout(timer);suggestRequest.cancelAll();};},[query,ids,prefs.searchMode]);
  useEffect(()=>{request.cancelAll();epoch.current++;setPages({});setErrors({});setBusy({});setSubmitted("");setTab(prefs.searchMode==="all"?"all":chosen[0]?.meta.id||"");return()=>{epoch.current++;request.cancelAll();};},[ids,prefs.searchMode]);
  const load=async(p:InstalledPlugin,q:string,n:number,cursor?:string)=>{
    if(n!==epoch.current||locks.current.get(p.meta.id)===n||cursor!==undefined&&consumed.current.get(p.meta.id)?.has(cursor))return;
    locks.current.set(p.meta.id,n);failedCursors.current.set(p.meta.id,cursor);
    setBusy(old=>({...old,[p.meta.id]:true}));setErrors(old=>({...old,[p.meta.id]:""}));
    try{
      const r=await request<Page<MediaItem>>("call",{id:p.meta.id,method:"search.search",args:{query:q,filters:searchFilters.current,cursor,limit:40}});
      if(n!==epoch.current)return;
      const seen=cursor===undefined?new Set<string>():consumed.current.get(p.meta.id)||new Set<string>();
      if(cursor!==undefined)seen.add(cursor);consumed.current.set(p.meta.id,seen);
      const repeated=!!r.next&&seen.has(r.next);
      setWarnings(old=>({...old,[p.meta.id]:repeated?"来源返回了重复分页游标，已停止继续加载。":""}));
      setPages(old=>{
        const previous=cursor===undefined?[]:old[p.meta.id]?.items||[],ids=new Set(previous.map(i=>i.id));
        return {...old,[p.meta.id]:{...r,next:repeated?null:r.next,items:[...previous,...r.items.filter(i=>{if(ids.has(i.id))return false;ids.add(i.id);return true;})]}};
      });
    }catch(e){if(n===epoch.current&&(e as {code?:string}).code!=="CANCELLED")setErrors(old=>({...old,[p.meta.id]:pluginError(e)}));}
    finally{if(locks.current.get(p.meta.id)===n)locks.current.delete(p.meta.id);if(n===epoch.current)setBusy(old=>({...old,[p.meta.id]:false}));}
  };
  const search=(q=query)=>{q=q.trim();if(!q||!chosen.length)return;setQuery(q);setHistory(old=>[q,...old.filter(h=>h!==q)].slice(0,20));request.cancelAll();const n=++epoch.current;locks.current.clear();consumed.current.clear();failedCursors.current.clear();searchFilters.current=values;setSubmitted(q);setPages({});setErrors({});setWarnings({});setBusy({});setChoice(null);setSuggest([]);for(const p of chosen)void load(p,q,n);};
  useEffect(()=>{if(submitted)search(submitted);},[JSON.stringify(values)]);
  const hits=chosen.flatMap(p=>(pages[p.meta.id]?.items||[]).map(item=>({pluginId:p.meta.id,item}))),groups=mergeSearch(hits);
  const visibleGroups=tab==="all"?groups:hits.filter(h=>h.pluginId===tab).map(h=>[h]);
  const activeSources=tab==="all"?chosen:chosen.filter(p=>p.meta.id===tab);
  const loading=activeSources.some(p=>busy[p.meta.id]);
  const clear=()=>{request.cancelAll();suggestRequest.cancelAll();epoch.current++;setQuery("");setSubmitted("");setPages({});setErrors({});setWarnings({});setBusy({});setChoice(null);setSuggest([]);inputRef.current?.focus();};
  const openGroup=(group:SearchHit[])=>group.length>1?setChoice(group):open(group[0].pluginId,group[0].item.id);
  return <div className={"refined-page refined-search legacy-search-page absolute inset-0 bg-bg flex flex-col"+(isMobile?" refined-mobile":"")}>
    <header className="refined-search-heading"><h1>聚合搜索</h1><span>{chosen.length} 个参与搜索的来源</span></header>
    <div className="refined-search-console shrink-0 px-4 pb-3 flex items-center gap-2.5" style={{paddingTop:"calc(var(--sat, 0px) + 12px)"}}>
      <form className="hpp-search legacy-search-form flex-1 flex items-center gap-2 h-11 rounded-(--r-ctl) bg-s3 px-3.5 focus-within:bg-s4 transition-colors" onSubmit={e=>{e.preventDefault();search();inputRef.current?.blur();}}>
        <button type="submit" aria-label="搜索" disabled={!query.trim()||!chosen.length} className="text-mut shrink-0 pressable"><ISearch size={17}/></button>
        <input ref={inputRef} aria-label="搜索所有插件" placeholder="搜索作品、作者、题材…" value={query} onChange={e=>setQuery(e.target.value)} className="flex-1 min-w-0 bg-transparent text-[13.5px] text-on placeholder-dim outline-none"/>
        {query&&<button type="button" aria-label="清除搜索" onClick={clear} className="w-5 h-5 rounded-full bg-s4 flex items-center justify-center text-mut pressable"><IClose size={11}/></button>}
        <button className="refined-search-submit" type="submit" disabled={!query.trim()||!chosen.length}>搜索片源</button>
      </form>
      <button type="button" aria-label="搜索来源与筛选" aria-expanded={showScope} onClick={()=>setShowScope(v=>!v)} className="w-11 h-11 shrink-0 rounded-(--r-sm) flex items-center justify-center bg-primary text-on-primary pressable elev-2"><ISettings size={19}/></button>
    </div>
    {isMobile&&<button className="search-scope-trigger" aria-expanded={showScope} onClick={()=>setShowScope(v=>!v)}><ISettings size={15}/><span>搜索范围</span><strong>{prefs.searchMode==="all"?"综合 · 全部 "+chosen.length+" 个来源":"已选 "+chosen.length+" 个来源"}</strong><IChevR size={14}/></button>}
    <div className="refined-search-scroll flex-1 min-h-0 scroll-y px-4 pb-6">
      <div className={`legacy-search-options refined-scope ${showScope?"is-open":""}`}><Scope kind="search" plugins={available}/><FilterControls filters={filters} values={values} change={setValues}/><button onClick={manage} className="text-primary text-xs pressable">管理插件</button></div>
      {!!suggest.length&&query!==submitted&&<div className="flex flex-wrap gap-2 mb-3" aria-label="搜索建议">{suggest.map(s=><button className="px-3 h-8 rounded-(--r-ctl) bg-s3 text-xs text-on pressable" key={s} onClick={()=>search(s)}>{s}</button>)}</div>}
      {!submitted?<div className="refined-search-idle"><div className="refined-search-recents">
        {!!history.length&&<section className="mt-1 a-rise" aria-label="搜索历史">
          <div className="flex items-center justify-between"><span className="text-[13px] font-bold text-on">搜索历史</span><button onClick={()=>setHistory([])} className="text-[11px] text-dim pressable">清空</button></div>
          <div className="flex flex-wrap gap-2 mt-2.5">{history.map(h=><button key={h} onClick={()=>search(h)} className="px-3.5 h-8 rounded-(--r-ctl) bg-s3 text-[12px] text-on/80 pressable">{h}</button>)}</div>
        </section>}
        {chosen.filter(p=>trending[p.meta.id]?.length).map(p=><section key={p.meta.id} className="mt-5 rounded-(--r-md) bg-s2 p-3.5 a-rise"><div className="flex items-center justify-between gap-2"><span className="flex items-center gap-1.5 text-[13px] font-semibold text-on/90"><IFireF size={15} className="text-primary"/>热门搜索</span><span className="text-[11px] text-mut">{p.meta.name}</span></div><div className="flex flex-wrap gap-2 mt-3">{trending[p.meta.id].map(s=><button key={s} onClick={()=>search(s)} className="px-3 h-7 rounded-(--r-ctl) bg-s3 text-[12px] text-on/85 pressable">{s}</button>)}</div></section>)}
        </div><aside className="refined-search-sources"><div className="refined-section-heading"><h2>参与搜索的来源</h2><button onClick={manage}>管理插件<IChevR size={14}/></button></div><div className="refined-source-list">{chosen.map(p=><div className="refined-source-row" key={p.meta.id}><span className="refined-glyph">{p.meta.name.slice(0,1)}</span><span><strong>{p.meta.name}</strong><small>v{p.meta.version}</small></span><span className="refined-source-check">✓</span></div>)}</div>
        {!available.length?<EmptySources manage={manage}/>:<p className="text-[11.5px] text-dim leading-relaxed mt-6">{chosen.length?"输入关键词后按回车搜索，也可以点搜索历史或热门词直接查看。":"请在右上角选择搜索片源。"}</p>}
      </aside></div>:<>
        {chosen.length>1&&<nav className="legacy-search-tabs flex gap-2 overflow-x-auto pb-2" aria-label="搜索结果来源"><button aria-pressed={tab==="all"} className={tab==="all"?"selected":""} onClick={()=>{setTab("all");setChoice(null);}}>全部</button>{chosen.map(p=><button key={p.meta.id} aria-pressed={tab===p.meta.id} className={tab===p.meta.id?"selected":""} onClick={()=>{setTab(p.meta.id);setChoice(null);}}>{p.meta.name}{busy[p.meta.id]?" · 加载中":` · ${pages[p.meta.id]?.items.length||0}`}</button>)}</nav>}
        <div className="refined-results-toolbar"><div className="text-[11.5px] text-mut mt-1 mb-2.5" aria-live="polite">「{submitted}」共 <span className="text-primary font-bold">{visibleGroups.length}</span> 条结果{loading?" · 搜索中…":""}</div><div className="refined-view-toggle" aria-label="结果显示方式"><button aria-pressed={viewMode==="grid"} onClick={()=>setViewMode("grid")}>海报网格</button><button aria-pressed={viewMode==="detail"} onClick={()=>setViewMode("detail")}>详细卡片</button></div></div>
        {choice?<div className="hpp-source-choice legacy-search-options"><div className="flex items-center justify-between mb-3"><h2 className="text-sm font-bold">选择内容来源</h2><button onClick={()=>setChoice(null)} className="text-primary text-xs">返回搜索</button></div>{choice.map(h=><button className="block w-full text-left p-3 mb-2 bg-s3 rounded-(--r-ctl) text-xs" key={h.pluginId+h.item.id} onClick={()=>open(h.pluginId,h.item.id)}>{sources.find(p=>p.meta.id===h.pluginId)?.meta.name} · {h.item.title}</button>)}</div>:
          <SearchPosters viewMode={viewMode} groups={visibleGroups} names={Object.fromEntries(sources.map(p=>[p.meta.id,p.meta.name]))} open={openGroup}/>}
        {!visibleGroups.length&&(loading?<div className="flex justify-center py-12"><div className="w-8 h-8 spinner" aria-label="正在搜索"/></div>:<div className="py-12 text-center text-mut"><ISearch size={26} className="mx-auto mb-3"/><p className="text-sm">没有找到相关内容</p><p className="text-xs text-dim mt-2">换个关键词或片源试试</p></div>)}
        {activeSources.map(p=><section key={p.meta.id}><Notice error={errors[p.meta.id]||warnings[p.meta.id]||""}/>{errors[p.meta.id]&&<button className="text-primary text-xs py-3" onClick={()=>void load(p,submitted,epoch.current,failedCursors.current.get(p.meta.id))}>{tab==="all"?`重试 ${p.meta.name}`:"重试搜索"}</button>}{pages[p.meta.id]?.next&&<PageLoadMore next={pages[p.meta.id].next} busy={!!busy[p.meta.id]} error={errors[p.meta.id]||""} more={()=>void load(p,submitted,epoch.current,pages[p.meta.id].next||undefined)} label={`加载更多 · ${p.meta.name}`}/>}</section>)}
      </>}
    </div>
  </div>;
}
