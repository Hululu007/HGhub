# WebHome 页面（HTML 源）

源除了用能力返回数据，还可以带一个可交互的网页首页：用户在插件详情页点
“打开 WebHome 首页”，宿主在独立窗口里打开你的页面，页面通过 `window.fm` 桥
调用宿主能力。v1 的完整方法表见[协议 04 §14](../plugin-protocol/04-host-api.md)。

## 声明

两处都要写：

```js
export const meta = {
  id: "demo.web",
  name: "示例源",
  version: "1.0.0",
  protocol: 1,
  webhome: { homePage: "https://example.com/webhome" },
};

export default function (hg) {
  return {
    webhome: {},
    media: { /* … */ },
    search: { /* … */ },
  };
}
```

- `homePage` 必须是 http(s) 绝对地址；`localPage` 是直接导入的本地 HTML 文件名
  （裸文件名，不含路径，文件存在插件目录里）；两者有且仅有一个。宿主在不调用工厂的情况下读取并校验它。
- 工厂里的 `webhome: {}` 是声明式命名空间，只要求是对象，不要求任何方法。

## 直接导入 HTML

不用自己写包装插件：把一个 `.html` 文件交给导入流程，宿主按内容识别后自动生成
包装插件（v0.0.48，见[协议 01 §3](../plugin-protocol/01-plugin-file.md)）。

- **本地文件**：插件中心 → 选择插件文件，挑一个 `.html` 文件；选择器也接受 `.html`。
  HTML 存进插件目录，`meta.webhome.localPage` 记文件名；打开时宿主用回环地址提供。
- **网址**：导入页粘贴一个 HTML 页面的地址；包装的 `homePage` 就是原地址，页面留在远端，
  相对资源照常工作。
- **站点 JSON**：粘贴 `{"homePage": "...", "name": "..."}`，认作一个 WebHome 源。
- 包装的 id 取 `webhome.<host>-<词干>`（远程）或 `webhome.<文件名词干>`（本地），名字取
  页面的 `<title>`；重复导入同一文件覆盖同一个插件。
- 包装的 search/media 是最小存根：搜索返回空结果；详情把页面本身当作唯一一集；
  `resolvePlayback` 明确报错（页面本身没有可解析的播放地址）。
- 限制：只支持单个自包含的 HTML 文件；本地页面引用的相对本地资源（如旁边的 `app.js`）
  不会一起打包，不做文件夹导入。

## 从底部栏打开

底部栏的“Node源”入口分两组：webhome 组列出所有已装的 WebHome 源：点一行打开它的首页，
点另一行切到那个源（同一时间至多一个窗口，切换时先关当前窗口）；最后一次点开
的源标“当前打开”。未启用或未就绪的源置灰，点不开。还没有源时页面显示空态，
点“管理插件”去导入。猫源组是猫源的资源浏览（与之前分类里的猫源同一套视图，
见 [cat-source.md §7.1](cat-source.md)）：每个已启用的猫源先列它的资源站，
点站进分类，再点分类看资源；订阅管理（添加/启停/删除/配置中心）只在插件中心的
“猫源”分栏。

## 页面里怎么调宿主

页面加载时宿主已注入 `window.fm`（别名 `window.fongmi`、`window.webhome`）。
桥就绪后派发 `fmsdk` 事件，窗口获得/失去焦点时派发 `fmresume` / `fmpause` 事件。

```js
// 等桥就绪（通常加载时已在，直接调也行）
window.addEventListener("fmsdk", init);

async function init() {
  // 搜索
  const r = await window.fm.req("app.search", { keyword: "示例" });
  if (r.code !== 0) throw new Error(r.message);

  // 打开详情
  await window.fm.req("app.openVod", { vodId: "123", siteKey: "demo.web" });

  // 直链播放
  await window.fm.req("player.playUrl", { url: "https://example.com/a.m3u8" });

  // 提示
  await window.fm.req("ext.toast", { message: "已收藏" });
}
```

`fm.req(method, payload)` 返回 `{ code: 0, data }`，失败为 `{ code, message }`。
对象式与函数式别名都接受，例如 `fm.req("net.request", …)` 与 `fm.req("req", …)` 等价。

## 页面能做什么

- **搜、开详情、播直链**：`app.search`、`player.playVod` / `player.playVodInline`、
  `player.playUrl`。`playVod` 的 `siteKey` 必须与绑定插件一致，不支持跨站。
- **用插件的身份发请求**：`net.request` 走绑定插件的网络，共享它的 Cookie 罐与规则。
- **读写缓存、记日志、弹提示**：`cache.*`、`ext.log`、`ext.toast`。
- **读播放状态、控制播放**：`player.status`、`player.control`（`play` / `pause` /
  `toggle` / `seek` / `stop`，只对当前直链播放有效）。
- **导航**：`app.openLive` / `app.openKeep` / `app.openSetting` 在主窗口打开对应页面；
  `navigation.back` 有历史则后退，否则关闭窗口。

## v1 的限制

- 同一时间至多一个 WebHome 窗口；打开另一插件的首页会让活着的 webview 导航到新首页（窗口不重建）。
- 图片不走代理（`net.resourceUrl` 返回原地址）。
- `pan.*`（投屏）、`ui.setChrome` / `ui.restoreChrome` 不支持，调用抛 `UNSUPPORTED`。
- 直链播放走真实媒体会话，刷新是空操作；页面关闭不影响已打开的详情页。
- 窗口右边中部有一个宿主注入的小房子控件：平时只露出一个“‹”，点它展开成 🏠，再点弹出源面板（列已装的 WebHome 源，点一个切过去，下面一行“关闭”回主界面）；几秒不用自动收回右边。页面自己的返回键关不掉窗口。
- iOS 上第二个 WebView 窗口尚未真机验证。

## 最小演示

仓库里有一份能直接导入的演示源：`examples/webhome-demo.js`（演示页
`examples/webhome-demo.html`，通过 githack 以正确类型加载）。导入后在插件详情页点
“打开 WebHome 首页”，页面上有搜索、打开详情、直链播放、查状态、弹提示五个按钮。

[当前支持状态](status.md) · [协议 06 §18](../plugin-protocol/06-capabilities.md)
