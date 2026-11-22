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

- `homePage` 必须是 https 绝对地址；宿主在不调用工厂的情况下读取并校验它。
- 工厂里的 `webhome: {}` 是声明式命名空间，只要求是对象，不要求任何方法。

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

- 同一时间至多一个 WebHome 窗口；打开另一插件的首页会先关掉当前窗口。
- 图片不走代理（`net.resourceUrl` 返回原地址）。
- `pan.*`（投屏）、`ui.setChrome` / `ui.restoreChrome` 不支持，调用抛 `UNSUPPORTED`。
- 直链播放走真实媒体会话，刷新是空操作；页面关闭不影响已打开的详情页。
- 窗口右上角有一个宿主注入的关闭按钮，页面自己的返回键关不掉窗口。
- iOS 上第二个 WebView 窗口尚未真机验证。

## 最小演示

仓库里有一份能直接导入的演示源：`examples/webhome-demo.js`（演示页
`examples/webhome-demo.html`，通过 jsDelivr 加载）。导入后在插件详情页点
“打开 WebHome 首页”，页面上有搜索、打开详情、直链播放、查状态、弹提示五个按钮。

[当前支持状态](status.md) · [协议 06 §18](../plugin-protocol/06-capabilities.md)
