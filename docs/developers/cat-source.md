# Node 猫源（CatVod/Node JS 源）

> 状态：设计文档，未实现。先接 CatPawPlay；真源已到（`index.js` 与
> `index.config.js`，md5 已验证），本设计以真源为准，不再猜格式。
> 本文档不写版本号。

## 1. 目标与背景

### 1.1 猫源是什么

“猫源”是 CatVod（猫影视）生态的 JS 内容源：一个 JS 文件（通常叫 `index.js`），
跑在 Node.js 引擎里，提供站点的首页、分类、详情、搜索、播放。
用户通过**订阅 URL**（形如 `https://…/index.js.md5`）或 **ZIP 文件**把源导入 App，
App 下载、校验后加载运行。截图里的另一 App 有三类站源——Node猫源、Python站源、
Webhome站源——红苹果现在已有后两类，缺的是 Node猫源这一类。

先接的源是 **CatPawPlay**（`https://catpaw.douer.me/index.js.md5`），它只是第一个
用来验证的源，不是内置源（见 §4.1）。

### 1.2 为什么 Node 引擎是现成的底子

红苹果 v0.0.70 已接入 nodejs-mobile v18.20.4（见 `node-engine.md`）：进程内
Node 线程 + 回环 HTTP 边车（`node-sidecar/host.js`，默认端口 55101，`/health`、
`/request`、`/diagnose`），Rust 侧只是 HTTP 客户端（`hpp::node::NodeClient`）。
猫源就是跑在 Node 里的 JS 文件——引擎、回环通道、代理 agent 都已存在，
本设计是**在边车上加一层“加载用户 JS 模块并调它”的能力**，不是再嵌一个运行时。

### 1.3 与 Python 蜘蛛的对照

Python 源是已落地的同类功能（v0.0.53 起），猫源照抄它的适配范式：

| | Python 蜘蛛 | Node 猫源（本设计） |
| --- | --- | --- |
| 运行时 | BeeWare CPython 3.12.16 边车 | nodejs-mobile v18.20.4 边车（已有） |
| 源文件 | `.py`，`plugin.py` | `.js`，`plugin.js` |
| 边车端点 | `/load`、`/call`、`/unload`、`/list` | `/cat/load`、`/cat/unload`、`/cat/list`（新增，见 §5.1；内容调用直接走源的 HTTP 服务） |
| 方法映射 | `python/convert.rs`（`dispatch`、`hpp_methods`） | 新增 `cat/convert.rs`，同一模式（见 §6.1） |
| 插件 runtime | `"python"` | `"cat"`（新增，见 §6.4） |

## 2. CatPawPlay 真源结构（已查实）

> 本节以用户发来的 `catpaw.zip`（`index.js` 6,440,449 字节，
> md5 `2a6dc3d49f9d052f5eee5228403d790e`，与 `index.js.md5` 一致；
> `index.config.js` 10,038 字节，md5 与 `index.config.js.md5` 一致）为准。
> `index.js` 是 esbuild 打包的 minified 单文件（含 Fastify、axios 与一个 Web 界面）。
> `index.config.js` 里是用户的个人网盘登录凭证，本节只写结构（有哪些键），
> 不写任何值；§2.6 与 §2.8 写明凭证的处理规则。

### 2.1 入口与生命周期

- 模块出口是 `module.exports = {start, stop}`（`start` 是 `T3n`，`stop` 是 `A3n`；
  文件末尾另有一行 `0&&(module.exports={start,stop})` 是死代码，不生效）。
- **`start(config)` 只取一个参数，就是配置对象**，函数体内第一件事是把它赋给
  服务实例：`$t.config = e`。配置由**壳子（宿主）传进来**，不是由 `index.js`
  自己读文件——`index.js` 里对 `index.config` 的引用是 0 次。
  因此 HGhub 侧的加载流程是：壳子先读配置（见 §5.2），再调 `start(config)`。
- `stop()` 是 `async function A3n()`：关掉 Fastify 服务（`$t.close()`），
  把实例置空。卸载源时调它；`start` 只能对同一模块调一次（重复 `start`
  会重建服务实例，旧实例需先 `stop`）。

### 2.2 HTTP 服务（Fastify，端口 9988）

- 服务是 Fastify 实例（`$t`），创建时 `serverFactory: catServerFactory`、
  `forceCloseConnections: true`、`trustProxy: true`、`caseSensitive: false`。
- 监听：`process.env.DEV_HTTP_PORT || 9988`，`host: "0.0.0.0"`；
  端口被占（`EADDRINUSE`）时端口 +1 重试。
  注意 `host` 是 `0.0.0.0`（全部网卡），不是只回环——见 §10 的局域网暴露风险。
- `$t.address()` 返回 `{url: "http://<本机局域网IP>:<端口>", dynamic: "js2p://_WEB_"}`；
  弹幕地址在配置里是 `http://<本机IP>:9321`（与本服务不同端口）。
- `proxySecret`：每次 `start` 时用 `crypto.randomUUID()` 现生成一个，
  挂在服务实例上（`$t.decorate("proxySecret", …)`），用于 `/proxy` 与
  `/m3u8-proxy` 的 HMAC-SHA256 签名（见 §2.4）。签名只在当次启动内有效，
  重启后失效——播放地址不要持久化（见 §6.3）。
- `messageToDart`：`globalThis.messageToDart = $t.messageToDart`，向
  `http://127.0.0.1:<dart端口>/msg` POST 动作消息（见 §2.5 的 Action）；
  取不到端口（返回 0）时直接返回 null，源本身能容忍没有 Dart 壳子。
  `catDartServerPort` 在包内只出现一次（调用处），定义不可见
  （应是由壳子注入或 minified 后改名），HGhub 侧按 §5.5 处理。
- 持久化：`$t.db = new JsonDB(new Config((process.env.NODE_PATH || ".") + "/ABC.db.json", …))`。
  源的持久数据（`saveProfile` 的内容、配置）存在 `ABC.db.json`，
  位置由 `NODE_PATH` 环境变量决定（未设时为当前工作目录）。
  `$t.db.push` 被包了一层：写库后还会调 `messageToDart({action: "saveProfile", …})`
  通知壳子（另一 App 的跨设备同步用；HGhub 没有 Dart 壳子时这一步是空操作，
  库文件本身仍是事实来源）。

### 2.3 站点结构（meta / api / check）

每个站点是一个对象：

```js
{
  meta: { key: e, name: t, type: 3 },   // type 3 是普通站源（与 CatPawOpen /config 的 type 一致）
  api: async (u) => {                    // u 是 Fastify 实例（或路由实例），在上面注册该站点的路由
    u.post("/init", bn),
    u.post("/home", s),
    u.post("/category", o),
    u.post("/detail", c),
    u.post("/play", Xr),
    u.post("/search", x),
    u.get("/proxy/:site/:what/:flag/:shareId/:fileId/:end", Bn),
    u.get("/test", sue)
  },
  check: i                              // 校验函数（校验该站点的配置/凭证）
}
```

站点通过 `register(o.api, {prefix: x})` 挂到服务上，`prefix` 就是该站点的
接口前缀（形如 `/spider/<key>/<type>`，与 CatPawOpen `/config` 的 `api` 字段一致）。
另有按网盘挂的前缀路由：`/auth`、`/quark`、`/uc`、`/uc-tv`、`/website`。

### 2.4 API 端点（已查实）

内容接口（每个站点在自己的前缀下注册；另有顶层同名路由）：

| 方法 | 端点 | 用途 |
| --- | --- | --- |
| POST | `/init` | 站源初始化（首次加载、搜索时调用） |
| POST | `/home` | 分类列表、分类的二级筛选 |
| POST | `/category` | 分类数据（可同时返回当前分类的 filter） |
| POST | `/detail` | vod 详情，含线路、选集信息 |
| POST | `/search` | 搜索 |
| POST | `/play` | 播放链接 |
| POST | `/check` | 校验凭证（例如 115 的 Cookie 是否有效；请求体带 `cookie`） |
| GET | `/test` | 站点自检 |

管理与登录接口（顶层）：

| 方法 | 端点 | 用途 |
| --- | --- | --- |
| GET | `/config` | 返回库里存的配置（见 §2.6 的凭证警告） |
| PUT | `/config` | 保存配置（TMDB 的 `apiKey`、`apiProxy`、`imgProxy` 等） |
| GET | `/account`、`/qrcode`、`/cookie`、`/token`、`/setting` | 网盘账号与登录（扫码登录、取 Cookie/token、设置） |
| GET | `/urls`、`/list`、`/pic`、`/episode` | 链接、列表、图片、选集等辅助接口 |
| GET | `/proxy`、`/m3u8-proxy` | 代理取流（带 HMAC 签名，见下） |

代理路由（取流用，HMAC 签名校验）：

- `GET /proxy/:site/:what/:flag/:shareId/:fileId/:end`（站点内代理）
- `GET /proxy/:mode/:key/:end`（按模式代理）
- `GET /proxy?url=…&h=…&sig=…`：只允许 http/https、SSRF 拦截（主机名检查）、
  签名 `HMAC-SHA256(proxySecret, url + "|" + h)` 必须对上；转发 `Range`，
  用 Chrome 140 UA、15 秒超时、最多 5 跳重定向，流式回传，带 CORS 头；
  失败返回 502（“proxy 上游失败”）。
- `GET /m3u8-proxy?url=…&h=…&sig=…`：同款签名校验；取回 m3u8 后把其中的
  相对地址与 `URI="…"` 全部重写为经过 `/m3u8-proxy` 的签名地址再返回。

响应信封：JSON 接口返回 `{code: 0, data: …}`（`code` 非 0 为错误，
`message` 带中文错误文案）；播放类失败常用 502。

### 2.5 数据格式

与 CatPawOpen 公开协议一致（git8477/CatPawOpen《猫爪源接口协议文档》，非官方，
本节已按真源的路由表核对过端点名）：

- **`/config`**：`{video: {sites: [{key, name, type, indexs, api}], danmuSearchUrl}}`。
  `type` 3 是普通站源，4 是推送；`indexs` 为 1 代表点击 vod 后跳搜索；
  `api` 是站源的接口前缀。
- **`/home`**：`{class: [{type_id, type_name, type_flag}], filters: {<type_id>: [{key, name, init, value: [{n, v}]}]}}`。
  `type_flag` 为 `"1"` 代表目录模式（“我的网盘”、AList 等）。
- **`/category`**：入参 `id`（`/home` 返回的分类 ID）、`page`（从 1 开始）、
  `filter`（Boolean 是否开启筛选）、`filters`（对象，key/value 对应 `/home` 的 filters）；
  返回 `{page, pagecount, list: [{vod_id, vod_name, vod_pic, vod_remarks}], filter: […]}`。
- **`/detail`**：入参 `id`（vodId，可传单个 ID，也可传数组）；
  返回 `{list: [{vod_name, vod_year, vod_actor, vod_director, vod_content, vod_play_from, vod_play_url, vod_links: [{name, url}]}]}`。
  `vod_play_from` 以 `$$$` 分隔为线路；`vod_play_url` 以 `$$$` 分隔为各线路的
  选集信息，各线路内以 `#` 分隔每一集，每一集以 `$` 分隔选集名和选集 ID。
- **`/play`**：入参 `flag`（线路）、`id`（选集 ID）；
  返回 `{url: [画质名, 地址, 画质名, 地址, …], header: {Cookie, User-Agent}, extra: {danmaku, audio}}`。
  播放地址支持 `push://` 协议（`push://http://…` 表示用链接推送跳到播放页）。
- **`/search`**：入参 `wd`（关键字）、`page`；
  返回 `{page, pagecount, list: [{vod_id, vod_name, vod_pic, vod_remarks}]}`。

壳子提供给源的 Action（源通过 `messageToDart` 发给壳子；HGhub 没有 Dart 壳子，
按 §5.5 处理）：`toast`、`danmuPush`、`openInternalWebview`、`openExternalWebview`、
`saveProfile` / `queryProfile`、`urlPush`、`getPlayInfo`。

### 2.6 配置与凭证（只写结构，不写值）

- 配置对象（`start(config)` 的入参）里有按网盘分的登录凭证：
  阿里、夸克、UC、115、百度、哔哩的 token/cookie（各网盘一段）。
  本文档只写到“有哪些网盘”这一层，不写任何键名对应的值、token 或 cookie。
- 配置文件 `index.config.js` 由**壳子**加载（`require` 后把对象传给 `start`），
  `index.js` 自己不读它。网络导入时订阅 URL 只给代码（`index.js`），
  配置是用户自己的、另外提供（见 §4.3）；ZIP 导入时配置随 ZIP 一起带进来（见 §4.4）。
- `GET /config` 返回的是库（`ABC.db.json`）里存的配置，可能含凭证——
  适配器绝不能把它的原始响应透传给前端界面或写进日志（见 §2.8）。

### 2.7 猫源怎么发请求

- 源内部用 axios（`_e`）发上游请求（视频站、网盘接口），例如 `/proxy` 里
  `_e.get(url, {headers, responseType: "stream", timeout: 15e3, maxRedirects: 5})`。
  源自己的上游请求是否跟随用户的代理设置，**待真机验证**（见 §9）：
  若不跟随，适配器要在加载时给源的 axios 实例配代理（不改源文件），
  或接受“源的上游请求只走 TUN 层”（与 `node-engine.md` §6.1 同一结论）。
- 源的 `/proxy` 与 `/m3u8-proxy` 是给播放器用的取流代理（回环地址 + HMAC 签名），
  不是源的上游请求——播放器调它们时走回环，不经过用户代理。

### 2.8 凭证安全规则（硬性）

导入的猫源配置与库文件可能含用户凭证（网盘 token/cookie），以下规则是设计的一部分：

- **只存本机**：配置文件（`index.config.js`）与库文件（`ABC.db.json`）只存在
  该猫源的插件目录里，不上传、不备份到任何云端、不随诊断报告外发。
- **绝不记日志**：`ext.log`、运行日志、`debug.log` 里不得出现凭证值；
  适配器在记日志前必须把 `token`、`cookie`、`apiKey` 等字段的值替换为
  `[已隐藏]`；`GET /config` 的原始响应不得记日志。
- **绝不外传**：适配器只把**分享链**传给猫源服务（见 §8），不直接取 token；
  猫源服务内部用 token 解析，token 不出服务。
- **删除即清除**：删除一条猫源订阅时，配置文件与 `ABC.db.json` 随插件目录
  一起删掉（见 §4.7）。
- **设计文档与诊断输出里不出现凭证值**：本文档只写结构；用户发来的
  `~/workspace/tmp/catpaw/` 整个目录按含敏感凭证处理，不复制到别处
  （清理建议见交付）。

## 3. HGhub 现有基础（本设计复用的部分）

### 3.1 Node 引擎与边车

- 引擎：nodejs-mobile v18.20.4，单实例、不可重启，`process.exit()` 会杀掉整个 App
  （`node-engine.md` §3.6）。猫源的加载、调用都必须遵守：顶层捕获异常、永不调
  `process.exit()`；一个坏源只报自己的错，不能拖垮边车与其他源。
- 启动：Rust 裸 FFI 调 `node_start`（`src-tauri/src/node.rs`，
  `dlopen` 包内 framework 再 `dlsym` 出 `node_start`，懒加载）；`node_start` 的
  argv 是 `["node", "<bundle>/node-app/host.js"]`；info 文件
  `<app_data_dir>/node-sidecar.json` + `/health` 发现地址。
- 边车：`src-tauri/node-sidecar/host.js`（CommonJS，只用 Node 核心模块
  `http`/`https`/`net`/`tls`/`dns`/`fs`），默认端口 55101，被占落临时端口；
  端点 `/health`、`/request`、`/diagnose`；`module.exports` 导出函数供本机测试。
- Rust 侧客户端：`hpp::node::NodeClient`（`node/client.rs`），`post_json`、
  `health()`（校验版本以 `v18` 开头）、`request()`（`/request`，
  成功返回 `{status, headers, bodyBase64}`，失败按 `code` 映射
  `TIMEOUT`/`NETWORK`/`UPSTREAM_ERROR`/`INTERNAL`）。
- 代理：每跳的代理地址由 Rust 侧 `proxy::effective_proxy_url`
  （`crates/hpp/src/proxy.rs:221`，`pub(crate)`）按用户设置算出
  （直连 → `None`；自定义 → 自定义地址；跟随系统 → `HGHUB_PROXY` 环境变量，
  否则 iOS 系统代理），随 `/request` 的 `proxyUrl` 传给边车；
  边车按 scheme 选 agent（CONNECT / SOCKS5 / SOCKS4）。猫源的请求复用同一条路（§5.4）。

### 3.2 Python 蜘蛛适配器（照抄的范式）

- 边车客户端：`python::client::SidecarClient`（`python/client.rs`），
  `/load`（`{key, code, mediaBase}` → `{success, methods, style}`）、
  `/call`（`{key, method, args}` → 结果 JSON）、`/unload`（尽力而为）、
  `/list`（`{spiders: [key]}`）。
- 方法映射：`python/convert.rs` 的 `dispatch`（HPP 方法 → 蜘蛛方法与参数）
  与 `hpp_methods`（蜘蛛方法 → HPP 方法面），`check_required`（必需
  `detailContent` + `playerContent`，且至少一个内容入口）。
- HPP 方法面（内容源必须提供的调用面，见 `convert.rs` 与协议 06）：
  `catalog.getCategories`、`home.getSections`、`feed.list`、`catalog.list`、
  `search.search`、`media.getDetail`、`media.resolvePlayback`。
- 数据转换：`vod_to_item`、`page`、`categories`、`sections`、`detail`、`playback`
  把蜘蛛的 JSON 转成 HPP 内容模型（协议 05）。

### 3.3 插件模型（猫源以什么身份出现）

- 插件是一等公民：`PluginManager`（`manager.rs`）管理安装、列表、启停、删除、调用。
  `list()` 返回 `PluginInfo`；`import_code(code, source, allow_downgrade)` 导入；
  `set_enabled(id, enabled)` 启停；`remove(id)` 删除；`call_transfer` / `call` 调方法。
- `meta.runtime`（`model.rs:15`）：目前只允许 `"js"`（默认，QuickJS 插件）或
  `"python"`（TVBox Python 蜘蛛）；校验在 `model.rs:166`。猫源新增 `"cat"`，
  校验同步放开（见 §6.4）。
- 插件文件：JS 存 `plugin.js`、Python 存 `plugin.py`（`manager.rs:650`，
  按 `meta.runtime` 选文件名）。猫源的 JS 同样存 `plugin.js`（runtime 区分身份，
  不另起名）；猫源的配置文件存同一插件目录下的 `index.config.js`
  （与用户发来的 ZIP 里的文件名一致，不改名）。
- WebHome 源是另一维度的标记：`meta.webhome { homePage?, localPage? }`
  （`webhome.rs:236`、`protocol.ts:19`），与 runtime 正交。
  `localPage` 是直接导入的本地 HTML 文件（即“HTML 源”），`homePage` 是远端地址
  （即“WebHome 源”）。猫源不带 `webhome` 标记。
- 前端：插件中心 `src/pages/Plugins.tsx`（已装/导入两栏，见 §7）；
  WebHome 源列表 `src/pages/WebHomeSources.tsx`
  （底部栏 Webhome 页，`sources.filter(p => p.meta.webhome)`）；`useHost()` 提供
  `sources`/`plugins`。资源库/搜索走 HPP 方法面调插件——只要猫源插件暴露了
  §6.1 的方法面，资源库/搜索无需改代码即可用它（见 §7.3）。

## 4. 订阅管理（导入 / 删除 / 启停 / 存储）

> 用户 2026-11-25 10:08 的明确要求：猫源做成可以导入源和删除源那种，
> 不是直接写死进去的。本节是本设计的核心要求，不是可选部分。

### 4.1 无内置源

- **不内置任何具体源**：CatPawPlay、王二小都不写进代码或配置。CatPawPlay 只是
  第一个用来验证的源，验证用的订阅 URL 由用户自己贴进来。
- **首次使用时猫源列表为空**：用户打开“猫源”页看到空列表与“添加订阅 / 导入 ZIP”
  两个入口，没有任何预置条目；不出现“示例源”“推荐源”之类的假条目。

### 4.2 订阅记录

每条猫源订阅是一条记录，字段：

| 字段 | 说明 |
| --- | --- |
| `id` | 插件 id（即订阅的身份；见 §6.4 的 id 派生规则） |
| `name` | 显示名（源自 `/config` 的站点名，或用户导入时给的名字） |
| `sourceType` | `"url"`（网络导入）或 `"zip"`（ZIP 导入） |
| `url` | 订阅 URL（仅 `sourceType` 为 `"url"` 时有；ZIP 导入的没有，见 §4.5） |
| `md5` | 下载到的 JS 的 md5（网络导入时是校验值；ZIP 导入时是存档时算出的值） |
| `enabled` | 启用/停用 |
| `updatedAt` | 上次成功拉取/导入的时间 |

存储：订阅记录存在 App 数据目录的 `cat-sources.json`（与插件的 `entry` 分开，
因为“订阅 URL”不是插件协议的一部分；插件的 `entry` 仍按现有格式存）；
下载的 JS 存在该插件目录下的 `plugin.js`（与现有插件文件布局一致，
`manager.rs:650` 的规则复用）；配置文件存在同一插件目录下的
`index.config.js`；库文件 `ABC.db.json` 存在该插件的数据目录（见 §5.2）。
订阅记录与插件是一一对应的：删订阅即删插件（§4.7）。

### 4.3 网络导入（贴订阅 URL 添加）

流程（与截图里“添加订阅”对应）：

1. 用户在“猫源”页点“添加订阅”，贴订阅 URL（例如 `https://catpaw.douer.me/index.js.md5`），点确认。
2. App 先拉订阅 URL 本身：若 URL 以 `.md5` 结尾，返回体应是 JS 的 md5（32 位 hex，
   去空白后校验格式）；若 URL 是普通 `.js` 地址，跳过这一步（无已发布校验值，
   只记录下载后算出的 md5，见第 4 步的说明）。
3. 再拉 JS 文件：URL 去掉末尾 `.md5`（`.md5` 订阅的约定，jadehh/TVSpider 的
   `dist/index.js.md5` 同款）；用插件 `net.request` 同款的代理决议拉取
   （`effective_proxy_url`，跟随用户设置，不默认直连）；超时、404、非 200 都按
   失败处理，不存半截文件。
4. 算下载到的 JS 的 md5：有已发布校验值时比对，不一致报“校验失败，文件可能被篡改或
   未更新”并丢弃；无已发布校验值时把算出的 md5 存下来作为基线（下次更新时比对用）。
5. 校验通过后，把 JS 存为新插件目录下的 `plugin.js`，写订阅记录（`sourceType: "url"`，
   `url` 原样保存，`md5` 存校验值），注册为 `runtime: "cat"` 的插件并加载（§5.1 的
   `/cat/load`），默认启用。
6. 失败时在导入页就地提示（网络错误 / 校验失败 / JS 加载失败各有明确文案），
   不留下一条“坏的”订阅记录。
7. **配置另外提供**：订阅 URL 只给代码（`index.js`），不含用户的个人配置。
   网络导入的源初始配置取 §5.2 的最小默认形状（不是空对象——真源的 `start`
   里 `config.sites.list` 是不设防的读法，空对象会直接炸掉加载）；用户要加网盘凭证时，
   走两种途径：① 在“猫源”页对该源点“导入配置”，选一个 `index.config.js` 文件
   导入（存进插件目录，见 §4.2）；② 走站源配置的登录入口（问题 3，见 §8），
   登录成功后把 token 写进该源的配置。两种途径都不经过网络订阅。

### 4.4 ZIP 导入（从 ZIP 文件导入）

流程（与截图里“导入 ZIP”对应）：

1. 用户点“导入 ZIP”，选一个 `.zip` 文件（文件可能来自别的 App 导出，或从网上下载）。
2. App 解压到临时目录：找 `index.js`（先找根目录，再找仅一层子目录；找不到报
   “ZIP 里没有 index.js”）。ZIP 里除 `index.js` 外的其他文件（如 `index.config.js`、
   附带的配置、额外的 JS 模块）一并解到插件目录，`index.js` 里的相对 `require`
   按插件目录解析。
3. 校验：若 ZIP 内含 `.md5` 文件（`index.js.md5` / `index.config.js.md5`），
   用它校验对应的文件（真源的 ZIP 就是这么配的：两个 32 字节校验文件）；
   否则校验 ZIP 本身完整性（解压无错）并算出 `index.js` 的 md5 存档；
   再把 `index.js` 交给边车做一次加载探测（§5.1 的 `/cat/load`），加载失败
   （语法错、缺依赖、不是猫源）报明错，不注册。
4. 校验通过后，把解出的文件存为新插件目录（`index.js` 为 `plugin.js`，
   `index.config.js` 原样保留，其余文件原样保留），写订阅记录
   （`sourceType: "zip"`，`url` 为空，`md5` 存算出的值），注册为
   `runtime: "cat"` 的插件，默认启用。
5. 失败时在导入页就地提示（ZIP 损坏 / 缺 `index.js` / 加载失败），不留残留文件
   （临时目录清掉）。
6. **配置随 ZIP 一起带进来**：ZIP 里若有 `index.config.js`，它是用户的个人配置，
   直接存进插件目录（不改名、不改内容）；加载时壳子读它传给 `start(config)`
   （见 §5.2）。ZIP 里没有配置时与网络导入一样，初始配置取 §5.2 的最小默认形状。

### 4.5 导入后的源与“订阅 URL”之间的关系（更新规则）

| 导入方式 | 有无订阅 URL | 更新方式 |
| --- | --- | --- |
| 网络导入 | 有（`url` 字段保存） | 点“检查更新”重新拉同一 URL：先拉 `.md5`，md5 没变就不下载；变了再拉 JS、校验、覆盖 `plugin.js`、更新记录的 `md5` 与 `updatedAt`，重新加载。更新失败（网络错、校验错）保留旧版与旧记录，只提示失败，不删源。 |
| ZIP 导入 | 无（`url` 为空） | 没有可重新拉取的地址，**只能重新导 ZIP**：重新导入同一源的 ZIP 时按 id 覆盖同一条订阅与同一插件目录（与 Python 蜘蛛“重复导入同一文件覆盖同一个插件”同一规则），`md5` 与 `updatedAt` 更新。重新导 ZIP 时若新 ZIP 里没有 `index.config.js`，保留旧的配置文件（不删用户的凭证）；若新 ZIP 里有，则覆盖。 |

UI 上要让这个区别看得见：网络导入的条目显示订阅 URL 与“检查更新”；ZIP 导入的
条目不显示 URL、不显示“检查更新”，只显示“重新导入 ZIP”（见 §7.1）。

### 4.6 启用 / 停用

- 停用：`PluginManager::set_enabled(id, false)`；边车卸载该源（`/cat/unload`，
  见 §5.1，即调 `stop()` 关服务），它不再出现在资源库/搜索/分类里；订阅记录、
  JS 文件、配置文件保留，重新启用时重新加载（`/cat/load`），不用重新下载。
- 启用：`set_enabled(id, true)`；重新加载并探测方法面，加载失败时插件标
  `failed` 并提示，不静默吞错。
- 启停与导入/删除是同一页的操作（§7.1），每条订阅行上直接可点。

### 4.7 删除

- **每条猫源订阅都要有删除操作**（列表行上的删除按钮，点后二次确认）。
- 删除时删掉：订阅记录（`cat-sources.json` 里该条）、插件目录（含下载的 `plugin.js`、
  配置文件 `index.config.js`、ZIP 导入时附带的文件、库文件 `ABC.db.json`）、
  边车里已加载的该源（`/cat/unload`）、该源的运行时缓存
  （与 `clear_all_runtime_caches` 的“只清运行时缓存”同一边界）。
- **不碰用户其他数据**：设置（`hpp.*` localStorage、代理设置等）、其他站源
  （Python/WebHome/其他猫源）、下载的视频、其他插件的源文件，一律不动。
  （与用户 2026-11-24 的缓存清理红线同一口径：删源只删该源自己的东西。）
- 删除是即时生效的，不留“已删除但文件还在”的残留；失败（文件删不掉）时报明错
  并保留订阅记录，不造成“列表里没了但文件还在”的不一致。

### 4.8 存储位置（汇总）

| 内容 | 位置 |
| --- | --- |
| 订阅记录（`cat-sources.json`） | App 数据目录（与 `node-sidecar.json`、`python-sidecar.json` 同一目录层级） |
| 下载的 JS（`plugin.js`）与 ZIP 附带文件 | 该插件目录（`PluginManager` 的 `root/<id>/`，与现有插件一致） |
| 配置文件（`index.config.js`，含用户凭证） | 该插件目录（只存本机，见 §2.8） |
| 库文件（`ABC.db.json`，含用户凭证） | 该插件的数据目录（由 `NODE_PATH` 指定，见 §5.2；只存本机，见 §2.8） |
| 运行时缓存（源的内存数据） | 边车内存（随 `/cat/unload` 与 App 重启丢掉，不落盘） |
| 源的持久化配置（Cookie 等，`saveProfile`） | 库文件 `ABC.db.json`（源自己的命名空间，不与其他源共用） |

## 5. Node 边车扩展

### 5.1 新端点（照抄 Python 边车，内容调用走源的 HTTP 服务）

真源已确认是**形态 B：自带 HTTP 服务**（§2.2），不是函数模块。
因此边车的职责是**加载模块、调 `start(config)`、发现端口、管生命周期**；
内容调用（首页/分类/详情/搜索/播放）由 Rust 侧直接调源的 HTTP 服务
（回环地址，`no_proxy` 的 loopback 客户端，与 `NodeClient` 同款），
不再经过边车中转一遍。

`host.js` 增加一组 `/cat/*` 端点，Rust 侧新增 `hpp::node::CatClient`
（与 `NodeClient` 并列；与 `python::client::SidecarClient` 同一调用风格）：

| 端点 | 请求 | 返回 |
| --- | --- | --- |
| `POST /cat/load` | `{key, file, configFile}`（`file` 为插件目录下 `plugin.js` 的绝对路径；`configFile` 为 `index.config.js` 的绝对路径，可空） | `{success, port, baseUrl, sites}`（`port` 为源实际监听的端口；`baseUrl` 为 `http://127.0.0.1:<port>`；`sites` 为 `GET /config` 返回的站点列表，含各站点的 `api` 前缀） |
| `POST /cat/unload` | `{key}` | 尽力而为（调 `stop()` 关服务，从注册表移除；失败不报错；`/cat/load` 同 key 会覆盖） |
| `GET /cat/list` | — | `{sources: [key]}`（已加载的猫源 key 列表） |

不设 `/cat/call`：内容调用不经过边车中转，Rust 侧直接调源的 HTTP 服务
（见 §6.1）。这样源的 HTTP 语义（`{code, data}` 信封、HMAC 签名代理地址）
原样透传，不被边车改写。

### 5.2 加载（`require` + `start(config)` + 端口发现）

- **加载**：边车 `require(file)`（CommonJS；`node-app/package.json` 已定死
  CommonJS，见 `node-engine.md` §4.2）。模块按 `key` 存进边车内的 `Map`
  （与 Python 边车的蜘蛛注册表同一做法）。
- **配置**：边车读 `configFile`（`index.config.js`，`require` 它得到配置对象；
  文件不存在时取最小默认形状，见下），再调 `start(config)`。配置对象只在内存里
  传给 `start`，不写进边车的任何日志（见 §2.8）。
  注意：`start(config)` 的入参就是配置对象（§2.1），不要把它包一层
  （不要传 `{config}`，直接传配置对象本身）。
  - **ESM 默认导出解包**：真源的 `index.config.js` 是 esbuild 打包的产物，只导出
    `default`（`__esModule: true`，`default` 才是真正的配置对象）；`require`
    直接得到的是 `{default: <配置>}`，不解包就传给 `start`，源里读
    `config.sites.list` 时 `config.sites` 是 `undefined`，加载当场炸掉
    （真机已复现：`Cannot read properties of undefined (reading 'list')`）。
    边车在 `require` 之后、`start` 之前解包：`__esModule === true` 且
    `default` 是对象时取 `default`。
  - **最小默认形状**：解包后（或无配置文件时）补齐真源 `start` 里不设防读的键，
    使任何无配置的源都能起来：`sites` / `pans` 默认为 `{list: []}`（已有
    但 `.list` 不是数组时重置为空数组），`tgsou` / `danmu` 默认为 `{}`。
    其余键原样保留；网盘凭证对象（`ali` / `quark` / `uc` / `y115` 等）不补默认——
    那些路径本来就需要真凭证，缺凭证时报错是缺凭证的错，不是加载的错。
- **宿主全局 `catServerFactory`**：真源的 `start()` 里 Fastify 以
  `{serverFactory: catServerFactory}` 创建服务，期望宿主在加载前提供
  这个全局，否则 `start()` 一调就抛 `ReferenceError: catServerFactory
  is not defined`（真机已复现）。边车在 `require` 之前定义
  `globalThis.catServerFactory = (handler) => require('http').createServer(handler)`，
  等价于 Fastify 的默认工厂（Fastify 调它时可能多传一个 `opts` 参数，
  直接忽略）。这是 CatPaw 系源的壳子约定，不是 HGhub 自创的接口；
  其他猫源若也用 `serverFactory` 选项，同一全局同样适用。
- **端口发现**：`start` 里监听的端口是 `process.env.DEV_HTTP_PORT || 9988`，
  被占时 +1。边车在调 `start` 前给该源分配一个端口并设
  `process.env.DEV_HTTP_PORT`（加载是顺序进行的，一次只加载一个源，
  进程级环境变量不会竞态）；调完 `start` 后用 `GET /test`
  （或 `GET /config`）向 `http://127.0.0.1:<端口>` 探活，通了才算加载成功，
  并把实际端口与 `baseUrl` 返回给 Rust 侧。若探活失败（端口被别的服务占了、
  `start` 抛错），按 EADDRINUSE 重试逻辑向后探 `+1`、`+2` 端口，
  仍不通则 `/cat/load` 报 `INTERNAL`（“猫源服务未启动”），不注册。
- **库文件位置**：调 `start` 前把 `process.env.NODE_PATH` 设为该插件的数据目录，
  使 `ABC.db.json` 落在该目录（§2.2）。同样因为加载是顺序进行的，不会竞态。
  注意 `NODE_PATH` 也会影响模块解析——源的 `require` 在 `start` 之前
  （加载时）已完成，`start` 之后源若懒加载模块可能受 `NODE_PATH` 影响；
  实现时核对 CatPawPlay 是否在 `start` 之后还有 `require`（真源已确认：
  `index.js` 顶层 `require` 全部是 Node 内置模块与打包进来的依赖，
  不依赖 `NODE_PATH` 解析第三方包）。
- **探测**：加载成功后边车调 `GET /config` 拿站点列表（`video.sites[]`，
  含各站点的 `key`、`name`、`type`、`api` 前缀），返回给 Rust 侧；
  拿不到站点列表、或列表为空时，`/cat/load` 报 `INTERNAL`
  （“该源没有可用站点”），不注册。
  `GET /config` 的原始响应可能含凭证（§2.6），边车只把其中的站点列表
  （`key`、`name`、`type`、`api`）返回给 Rust 侧，**不把整个配置对象
  传回 Rust 侧、不记日志**。
- **依赖**：源只能 `require` Node 核心模块与打包进来的依赖；
  CatPawPlay 的 `index.js` 是 esbuild 单文件包，顶层 `require` 全部是
  Node 内置模块（crypto、fs、http/https、net、os、path、stream、zlib、
  worker_threads、buffer 等），不依赖 `node_modules`。其他猫源若 `require`
  第三方包（无 `node_modules`），加载即失败，导入时报明错
  （“该源依赖未提供的模块：xxx”）。

### 5.3 错误隔离与超时

- **加载超时**：`start(config)` 本身没超时，源内部一旦挂住，`/cat/load`
  的 handler 永不回。边车给 `start()` 加 30 秒超时，超时按启动失败报
  （“猫源启动超时（30 秒）”）；Rust 侧的加载预算（默认 15 秒）不变，
  仍先到先报。
- **超时**：每次内容调用（Rust 侧直接调源的 HTTP 服务）有独立超时
  （默认与插件 `net.request` 的单跳超时同一量级，15 秒）；超时后该次调用报错，
  不杀边车、不卸载源。
- **错误隔离**：源抛错、返回非 JSON、返回 `{code != 0}` 时，按源的错误信封
  原样转成 `PluginError`（`code` 与 `message` 透传，截断 1000 字）；
  **一个坏源只影响自己的调用**，不影响边车与其他源；边车顶层
  `uncaughtException` / `unhandledRejection` 只记日志，绝不调
  `process.exit()`（`node-engine.md` §4.2 的硬性要求）。
- **服务挂掉**：源的 HTTP 服务异常退出（`stop` 被调、进程内崩溃）时，
  下一次调用报 `INTERNAL`（“猫源服务已停止，请重新启用该源”），
  不自动重启（Node 同进程不支持二次启动，与 `node-engine.md` §3.6 同一约束）。

### 5.4 猫源的网络请求与代理

- **源的上游请求**：源内部用 axios 发上游请求（视频站、网盘接口）。
  是否跟随用户的代理设置，**待真机验证**（见 §9）：验证时在跟随系统与直连
  两种模式下各跑一遍分类，确认出口与行为。
  若不跟随，适配器要在加载时给源的 axios 实例配代理（不改源文件；
  例如在 `start` 前设 `HTTPS_PROXY`/`HTTP_PROXY` 环境变量，或由边车
  在加载期给源的 axios 默认配置注入代理——以真机验证结果为准定方案）。
- **绝不默认直连**：用户模式为跟随系统/自定义时，猫源的请求一定走代理；
  只有用户把模式设成“直连”时才直连（与 `node-engine.md` §6.4 同一红线）。
  回环地址（边车自身、猫源服务自身、媒体回环服务）永远 `no_proxy`。
- **播放取流**：播放器调源的 `/proxy` 与 `/m3u8-proxy`（回环地址 + HMAC 签名）
  取流；这两个端点是源的服务提供的，播放器直接调，不经过用户代理
  （回环地址本来就不走代理）。
- **限速与预算**：猫源的调用挂在对应插件的 `Network` 名下，复用插件
  `Network` 的调度语义（单源并发上限、单主机上限、速率限制），不要另起一套限速；
  单次调用的响应体上限 8 MiB（与 `/request` 一致），超限报错不读内存。

### 5.5 壳子动作（`messageToDart`）

- 源通过 `messageToDart` 向壳子发动作（`toast`、`danmuPush`、`saveProfile` 等，
  见 §2.5），目标是 `http://127.0.0.1:<dart端口>/msg`；取不到端口时返回 null，
  源本身能容忍。
- HGhub 没有 Dart 壳子。第一版接受 `messageToDart` 为空操作（源已能容忍），
  但有两点要处理：
  - `saveProfile` 的跨设备同步在 HGhub 不需要——库文件 `ABC.db.json`
    本身就是事实来源（见 §2.2），配置持久化不依赖 `messageToDart`。
  - 若后续要接 Action（例如 `toast` 提示），边车提供一个 `POST /msg` 端点
    （回环地址），并让源能发现它：`catDartServerPort` 在包内只出现一次
    （调用处），定义不可见——实现时先确认它从哪里取端口（环境变量、
    全局变量或壳子注入），再决定是设环境变量还是在加载期注入；
    确认前不写死端口号。
- `getPlayInfo`（取当前播放信息）由 HGhub 的播放器直接提供，不经过 `messageToDart`。

## 6. 适配器映射（猫源 HTTP 服务 → HPP 站源接口）

### 6.1 映射表（复用 `python/convert.rs` 的 `dispatch` 模式）

HPP 方法面（§3.2）不变；新增 `cat/convert.rs`，把 HPP 方法译成对源 HTTP 服务的调用。
站点前缀取自 `GET /config` 的 `video.sites[]` 的 `api` 字段
（形如 `/spider/<key>/<type>`），调用时拼在端点前：

| HPP 方法 | 猫源 HTTP 调用 | 参数 |
| --- | --- | --- |
| `catalog.getCategories` | `POST <api>/home` | 无（`{}`） |
| `home.getSections` | `POST <api>/home`；`/home` 只给分类不给列表时（设计 §5 的 `/home` 形状本来就没有 `list`），取首个分类调 `POST <api>/category` 拿列表当推荐内容；有 `list` 的站点不走回退 | 无 |
| `feed.list` | `POST <api>/home`（同上） | 无 |
| `catalog.list` | `POST <api>/category` | `{id: tid, page: pg, filter, filters}` |
| `search.search` | `POST <api>/search` | `{wd: query, page: pg}` |
| `media.getDetail` | `POST <api>/detail` | `{id}`（单个 ID；批量时传数组） |
| `media.resolvePlayback` | `POST <api>/play` | `{flag, id}` |

多站点调用是并发扇出（最多 6 路并行，结果按站点顺序返回），不再逐站串行；
单个站点失败只跳过该站，全部失败时返回第一个站点的错误。

分类按站点分组返回：`catalog.getCategories` 的顶层一个节点一个站
（`id` 是站 key、`label` 是站名），子节点是该站的 class（`id` 是
`站::type_id`、`label` 是原始 `type_name`，不带站名前缀）；空站不出现。
单站订阅仍返回扁平列表。分类页只在切到某个站后才展示该站的分类
（手机端进分类页先看到资源列表，点一个资源再看它的分类；桌面端用
左侧分类目录切站）。
为避免一打开分类页就把全部站点的 `/home` 扇出一遍，`catalog.getCategories`
支持两个可选参数：`{shallow: true}` 只返回已知的站点列表（顶层节点
`children` 为空数组，不调源服务）；`{site: <站 key>}` 只返回该站的
分类（扁平列表）。前端分类页先调浅层拿资源列表，点某个站后再按站取
分类；只有一个站有分类时仍直接平铺，不多一次点选。

源服务连不上时（`猫源服务连接失败`），worker 记 fault；下一次调用重新加载
服务（新端口、新 revision）而不是永远返回旧错——分类/推荐页的重试按钮
就是这条恢复路径。重载前等旧服务卸载完成，避免后到的卸载杀掉新加载的
同名服务。

调用前先调 `POST <api>/init`（站源初始化，首次加载、搜索时调用；
返回无内容，失败不阻塞后续调用——与 CatPawOpen 协议一致）。

### 6.2 站点发现（`/config`）

- 适配器在 `/cat/load` 成功后调 `GET /config` 拿站点列表
  （`video.sites[]`：`key`、`name`、`type`、`indexs`、`api`）。
  一个猫源订阅可能含多个站点（CatPawPlay 的 `/config` 里有多站）——
  每个站点在 HGhub 里是一个独立的内容源入口（共用同一个猫源服务与配置，
  但分类、搜索、详情按站点分开）。
- `type` 3 是普通站源（进资源库/搜索）；`type` 4 是推送（不进资源库，
  只用于链接推送）；`indexs` 为 1 的站点点击 vod 后跳搜索（豆瓣这类）。
- `danmuSearchUrl`（弹幕搜索页面地址）记下来供弹幕能力用。

### 6.3 数据转换（复用 Python 蜘蛛的转换函数）

- vod 列表项 → HPP item：复用 `vod_to_item` 的字段映射
  （`vod_id` → id、`vod_name` → 名称、`vod_pic` → 封面、`vod_remarks` → 备注）；
  分类/搜索的分页（`page`、`pagecount`、`total`）按 `page()` 转换。
- 详情：`vod_play_from`（`$$$` 分线路）与 `vod_play_url`
  （`$$$` 分线路、`#` 分集、`$` 分集名与集 ID）按 `detail()` 的解析规则
  拆成线路与选集；`vod_links`（外链）按现有详情模型挂在详情下，不单独成线路。
- 播放：`url` 的画质名/地址对按 `playback()` 转成播放候选；
  `header`（`Cookie`、`User-Agent`）随播放请求带上（播放器已支持自定义请求头时）；
  `extra.danmaku`（弹幕）与 `extra.audio`（外挂音轨）按播放器能力接，
  第一版接不上时忽略、不报错。
- `push://` 开头的播放地址：按 `urlPush` 语义跳到播放页并推送目标地址
  （与 CatPawOpen 协议同款；第一版若播放器未接 `push://`，先报
  “该源的播放地址需要推送，暂不支持”，不静默失败）。
- **代理播放地址不持久化**：`/play` 返回的地址可能是经 `/proxy` 或
  `/m3u8-proxy` 的签名地址（HMAC 用当次启动的随机 `proxySecret` 签发，
  见 §2.2），重启后失效。收藏、历史记录里存原始选集 ID（`flag` + `id`），
  不存签名后的代理地址；播放时重新调 `/play` 拿 fresh 的地址。

### 6.4 订阅 = 插件（`runtime: "cat"`）

- `model.rs` 的 `runtime` 校验放开到 `"js" | "python" | "cat"`
  （`model.rs:166`），并在 `model.rs:15` 的注释里写明 `"cat"` 是 Node 猫源。
- 插件文件：猫源的 JS 存 `plugin.js`（`manager.rs:650` 按 `meta.runtime`
  选文件名，`"cat"` 走 JS 分支）；配置文件存同一插件目录下的
  `index.config.js`（不改名）；库文件 `ABC.db.json` 存该插件的数据目录（§5.2）。
- 插件 id 派生：网络导入时优先用订阅 URL 的 host + 路径生成稳定 id
  （例如 `cat.catpaw-douer-me`），或用 `/config` 里第一个站点的 `key`
  （`cat.<key>`）；ZIP 导入时用 ZIP 文件名词干或第一个站点的 `key`。
  id 前缀 `cat.`（与 Python 的 `py.` 前缀同款），版本恒为 `1.0.0`
  （与 Python 蜘蛛同款；猫源没有版本概念时不编造版本号）。
  id 必须稳定：同一订阅 URL / 同一 ZIP 重新导入时覆盖同一条订阅与同一插件，
  不产生重复条目。
- `allowInsecureHttp`：猫源插件声明为 `true`（`cat_install` 现拼的 Meta
  里带，见 `manager.rs`）。边车服务本身就是回环 http，`/play` 与
  `/proxy` 返回的播放地址都是 `http://127.0.0.1:<端口>/…`；按协议
  01 §2，不声明时播放校验会把全部 http 流丢掉、报“没有合法可播放流”
  （2026-11-26 齐天圣瞳真机故障：集数已出来，点播却报无合法流）。
  这是猫源自己的声明，不改校验器、不放宽其他 runtime 的 http。
  老源迁移：v0.0.100 之前导入的猫源，index.json 里没有这一项，启动
  恢复时（`with_mode`）给 `runtime == "cat"` 的条目在内存里补上，
  无需重新导入；不另写 index.json，下次启动恢复时还会再补。
- 能力面：猫源插件的 HPP 方法面由 §6.1 的映射表决定（内容源必须提供
  `catalog.getCategories`、`catalog.list`、`search.search`、`media.getDetail`、
  `media.resolvePlayback`；`home.getSections` / `feed.list` 有则提供）；
  不声明 `localProxy`、`init`、`action` 等非 v1 方法（与 Python 蜘蛛同款）。
- 启停、删除、日志、设置：全部走 `PluginManager` 现有方法（§3.3），
  不另起一套；猫源的日志来源记 `"cat"`（与 `"python"` 同款做法，
  见 `manager.rs:1206` 的 `/pylog` 桥）。

### 6.5 Action 映射（第一版的取舍）

| Action | 第一版 |
| --- | --- |
| `toast` | 接：用 App 的 toast 通道显示（需 §5.5 的 `/msg` 端点；未提供前忽略） |
| `danmuPush` | 接：有弹幕能力时推送，否则忽略 |
| `openInternalWebview` | 接：走应用内浏览器窗口（v0.0.92 已有 `browser.rs`） |
| `openExternalWebview` | 接：走系统 opener |
| `saveProfile` / `queryProfile` | 不走 `messageToDart`：配置持久化以库文件 `ABC.db.json` 为准（见 §2.2）；跨设备同步不在第一版做 |
| `urlPush` | 接：跳到播放页并带上播放信息 |
| `getPlayInfo` | 接：由播放器直接提供，不经过 `messageToDart` |

## 7. UI

### 7.1 落点：Node源面板为主，插件中心“猫源”分栏保留

猫源订阅管理的主入口是底部栏 **Node源面板**（`src/pages/WebHomeSources.tsx`，
见 [webhome.md](webhome.md)）：面板分 webhome、猫源两组，猫源组就是下面的
`CatSources` 组件（Node 运行状态、添加猫源、已添加的猫源列表）。
插件中心的“猫源”分栏保留，挂的是同一个组件，两处是同一批数据、同一批 IPC，
状态一致。

红苹果**没有“站源订阅”这个页**（全仓 grep 无此文案），来源的管理入口是
插件中心 `src/pages/Plugins.tsx`（标题“插件中心”），已有两个分栏：
“已安装”与“导入与订阅”（`PluginDistribution`，里面有“从网址导入插件或订阅列表”、
选择插件文件、粘贴代码导入、插件订阅、插件更新队列）。

**猫源导入菜单放在插件中心，新增一个“猫源”分栏，与“已安装”“导入与订阅”并列。**
不要新开一个“站源订阅”页。猫源跟 Python 源、WebHome 源都在插件中心统一管理。

“猫源”分栏里的布局照另一 App 的 Node猫源页（用户截图）：

- **上面是运行状态**：“Node 运行中”/“Node 已停止”一行（状态来自 `/health` 的
  `process.version`，与现有 Node 诊断同一来源；另有“停止”按钮可停掉 Node 引擎
  ——注意 Node 单实例不可重启，停掉后需重启 App 才能再用，按钮文案要写明这一点）。
- **“添加猫源”区**：**网络导入**（贴订阅 URL，见 §4.3）与 **ZIP 导入**
  （选 ZIP 文件，见 §4.4）两个入口（与截图里的“添加订阅”“导入 ZIP”对应）。
- **“已添加的猫源”列表**：每条显示名称、订阅地址（网络导入的才显示；
  ZIP 导入的显示“ZIP 导入”）、运行状态（“运行中”/“已停用”），
  每条有**启用/停用开关**、**运行日志**按钮与**删除按钮**（§4.6、§4.7）；
  运行日志读该猫源的插件日志（`logs`，与已安装插件的“读取运行日志”
  同一接口），诊断行来源记 `"cat"`（见 §6.5）。
- 每条另有**配置中心**按钮：找到该猫源的配置站（站点 key 含 `baseset` 或
  名字含“配置”），取它的 `/home` 里那条 URL 分类，在应用内浏览器打开配置页
  （见 §7.4 末段）；没有配置站时报“该猫源没有配置中心”。
- 网络导入的条目另有“检查更新”（§4.5）；ZIP 导入的条目是“重新导入 ZIP”。
- 空列表时只显示两个入口，不放任何预置源（§4.1）。

### 7.2 按类型分组管理（Python / HTML / 猫源 / WebHome 分开）

> 用户 2026-11-25 10:15 的明确要求：Python、HTML、猫源要分类分开管理，
> 做区分，便于这些源管理。

**现状**：插件中心的“已安装”分栏按 `meta.kind` 分组（`Plugins.tsx:99,104`），
只有“数据源”与“解析器”两组；Python 蜘蛛、HTML 源、WebHome 源、普通 JS 插件
都落在“数据源”这一组里混在一起（行副标题只用 `· Python` 标一下 Python，
`Plugins.tsx:108`），找起来费劲。

**改成按源类型分组**：

| 分组 | 判定规则（按 `meta` 字段，互斥，按此顺序判定） |
| --- | --- |
| 猫源 | `meta.runtime === "cat"`（在“猫源”分栏里管理，见 §7.1；“已安装”里不再重复列出，避免两处重复） |
| Python | `meta.runtime === "python"` |
| HTML | `meta.webhome?.localPage` 已设置（直接导入的本地 HTML 文件） |
| WebHome | `meta.webhome?.homePage` 已设置（远端地址；且无 `localPage`） |
| 其他 JS 插件 | `meta.runtime === "js"` 且无 `webhome`（兜底组，保证每个插件都有归属，不丢项） |

规则：

- 每组是独立的列表区域（自己的标题、条数、列表），不混在一起；
  每组的每条仍有启用/停用、删除、设置、日志操作（现有逻辑不动，只是按组呈现）。
- 每组有自己的导入入口：猫源的在“猫源”分栏（§7.1）；
  Python / HTML 的仍在“导入与订阅”分栏（选择插件文件、粘贴代码导入、
  从网址导入插件或订阅列表——现有入口不动）；WebHome 的仍走现有的
  WebHome 导入（插件文件）。
- 列表与筛选要能按类型区分：类型分组本身就是筛选（点某组标题只看该组）；
  现有的 `kindFilter`（全部/内容源/解析器，`Plugins.tsx:33,99`）保留，
  但只作用于“解析器”与“数据源”的大类区分，不再是唯一的分组方式；
  在“数据源”大类内，按上表的类型分组呈现。
- **Python / HTML 现有的管理方式不动**：只在分组呈现上做区分，
  不改它们的导入逻辑（文件选择、粘贴代码、订阅列表）与启停逻辑
  （`PluginManager::set_enabled`、`remove` 的调用方式不变）；
  改动只限于 `Plugins.tsx` 的分组渲染与 `kindFilter` 的语义，
  不动 `manager.rs` 的导入/启停实现。

### 7.3 插件中心的其他呈现

- 猫源插件在插件中心显示为内容源（`meta.kind` 为 `"source"`，与 Python 蜘蛛同款），
  名称为源声明的名字，版本 `1.0.0`；启停、删除、日志、设置走现有插件中心操作。
- WebHome 源列表页（`src/pages/WebHomeSources.tsx`，底部栏 Node源面板）
  分 webhome、猫源两组（见 [webhome.md](webhome.md)）；其上的“管理插件”按钮进插件中心（现有行为）。

### 7.4 资源库 / 搜索 / 分类

不用改：资源库、搜索走 HPP 方法面调插件（§3.3），猫源插件一旦暴露了
§6.1 的方法面，就自动出现在资源库与搜索里（与 Python 蜘蛛接入时同一结论）。
猫源的搜索（`/search`）进搜索页；猫源的详情与播放走现有详情页与播放器。
一个猫源订阅含多个站点时（§6.2），每个站点是独立的内容源入口（共用同一个
猫源服务与配置，但分类、搜索、详情按站点分开）。
**猫源不再进分类页**（已移到 Node源面板，见 §7.1）：
分类页的片源栏按类型分组（插件、Python、HTML），不再有猫源分组；
启动失败、已停用或不兼容的源另以状态行列出，只显示状态与原因，不提供内容（见
`docs/plugin-protocol/08-frontend.md` §4）。

猫源配置站是个例外：它的 `/home` 只回一个 class，`type_id` 是 `setting`，
`type_name` 就是本机 `/website` 地址（端口随每次加载变）。它在 Node源面板
猫源组的每条后面有“配置中心”按钮：点按直接在应用内浏览器打开配置页
（扫码登录、网盘 token 等都在那页里管），不调 `catalog.list`——调了也只
会拿到扫码配置/点击配置/弹幕服务三张动作卡片，点开详情是空的。

## 8. WebHome 网盘链走猫源 token 解析（已实现，未真机验证）

> 用户 2026-11-25 10:18 问的后续集成点。**已实现**（2026-11-26，`hpp/src/webhome.rs`
> 的 `pan.play` 分支，树里，未出版本）：链路经静态分析与 VM 实探定位，
> §8.4 的三个问题已确认；端到端只能在真机验证（VM 里猫源配置的 token
> 已过期，见 §8.4 末尾），验证步骤见 §8.5。

### 8.1 现状

WebHome 页里的网盘链，点播放调 `fm.pan.play`。v0.0.92 的做法是走应用内浏览器
打开（用户 2026-11-25 02:08 的决定：不走系统浏览器，直接在红苹果 App 内打开；
实现是 `browser.rs` 的应用内浏览器窗口）。2026-11-26 起，`pan.play` 先走猫源
token 解析（§8.2）：解析成功进原生播放器，解析不到回退应用内浏览器打开，
v0.0.92 的行为是回退路径，不变。

### 8.2 机制（猫源 token 解析）

猫源的配置里已有各网盘的 token（§2.6：阿里、夸克、UC、115、百度、哔哩），
猫源服务本身能用这些 token 解析网盘分享链并播放。实现的机制是：

- WebHome 的 `pan.play` 在**有已启用猫源时**，按猫源 id 顺序逐个尝试：
  先调该猫源 **push 站**（`nodejs_push`，`/spider/push/4`）的
  `media.getDetail`，入参就是分享链本身（`id: "nodejs_push::<分享链>"`），
  拿到第一条线路的第一集；再调同一站的 `media.resolvePlayback`
 （`episodeId` 为该集 id、`playlistId` 为线路名），拿到
  `{parse:0, url, header, format}`；最后 `prepare_direct`（猫源插件 id）
  建直链会话，进原生播放器。
- **没有猫源、或所有猫源都解析不到时**，回退到应用内浏览器打开
  （v0.0.92 的行为不变）。
- **token 留在猫源服务内部**：HGhub 只传分享链，不直接取 token；
  由猫源服务内部用 token 解析（与 §2.8 的凭证规则一致）。
  按网盘类型判断走哪个 token：分享链的域名决定网盘
  （例如 `pan.quark.cn` → 夸克、`aliyundrive.com` → 阿里、`drive.uc.cn` → UC、
  `115.com` → 115、`pan.baidu.com` → 百度），由猫源服务按域名路由到对应的
  网盘处理（王二小里是 `NVt`/`cGt` 分发函数 + 各网盘客户端类）。
- **push 站是分享链的入口**：`mypan` 站的 detail 只解析光鸭分享链
  （`SY→Xv→U$r` 只认 `guangyapan.com`），夸克/阿里/UC 分享链不能走
  `mypan`。王二小里 push 站（`dc0`，key `push`、type 4）的 detail 调
  `panService.resolve`（按域名分发到各网盘客户端），play 调
  `panService.play`（分集 id 是 base64 JSON：`{providerId, shareId,
  fileId, name, playToken, mode}`）。CatPawPlay 里对应的入口是 `tgsou` 站
  的 detail（调 `pan.js` 的 `rr`），不是 push 站——两套包的架构不同，
  不能照搬（见 §8.4 末尾）。
- **带提取码的分享链暂不支持**：`pan.play` 只传分享链，不传 `password`；
  带码的链解析失败时回退浏览器，用户在网盘页面里自己输码。

### 8.3 与“问题 3 站源配置”的关系

两者互补，不互相替代：

| | 猫源 token | 问题 3 站源配置 |
| --- | --- | --- |
| token 来源 | 用户的猫源配置里已有的 token（随猫源导入带来） | 用户在红苹果里自己扫码登录各网盘，token 入库到红苹果 |
| 适用用户 | 已导入猫源、且猫源配置里有对应网盘 token 的用户 | 所有用户（尤其没有猫源的用户） |
| 关系 | 是其中一种 token 来源 | 是红苹果自己的登录入口 |

没有猫源的用户仍需要问题 3 的入口；有猫源的用户可以先用猫源 token，
问题 3 的入口仍保留（作为兜底与没有猫源时的登录方式）。

### 8.4 可行性与接口（已确认，2026-11-26）

- `/play` 不直接吃分享链：入参是 `{flag, id}`，`id` 是解析后得到的复合
  id（王二小里是 base64 JSON：`{providerId, shareId, fileId, name,
  playToken, mode}`；CatPawPlay 里是 `*` 拼接串，如夸克
  `shareId*code*stoken*fileId`）。分享链 → 复合 id 这一步走 push 站的
  detail（王二小）或 `tgsou` 站的 detail（CatPawPlay），`mypan` 的
  detail 只认光鸭分享链。
- 返回的地址可直接进原生播放器：`url` 通常是经猫源服务 `/proxy` 的
  回环签名地址（鉴权头存在服务端的会话里，`header` 为空），个别网盘
  （如百度 m3u8）返回上游直链 + `header`；`prepare_direct`（猫源插件
  id，`allowInsecureHttp` 已声明）两种都接。签名用当次启动的随机
  `proxySecret`，重启后失效——播放时重新解析，不持久化。
- 无法解析时（token 失效、分享链过期、网盘限流）：源返回空列表或报错，
  HGhub 按源换下一个、全部失败回退到应用内浏览器，不把源的错误原样
  抛给用户。
- **实探结论（2026-11-26，VM）**：链路已定位并调通到解析入口——
  CatPawPlay 的 `tgsou` detail 对夸克分享链回了 1 条但 from/url 为空，
  `mypan` category 查夸克/115/UC/百度全部 0 条；原因是 VM 里那份
  2026-08-31 的 CatPawPlay 配置 token 已过期，解析在取 token 这步就
  失败，与接口本身无关（VM 到 `pan.quark.cn` 是通的，200）。**王二小
  与 CatPawPlay 不是同一套代码**（王二小 6,643,607 B，md5 与记录一致；
  没有 `tgsou` 站、没有共用的 `Xr`，各站自带 detail/play 方法），所以
  实现按王二小的 push 站写，CatPawPlay 的 `tgsou` 链路只作参考。
  端到端验证只能在真机做（王二小配置 + token 都是活的）。

### 8.5 验证步骤（实现后，真机，未跑）

1. 用真机上的王二小（夸克 token 是活的），在 WebHome 里点一条夸克网盘链
   （用户 2026-11-26 发来的「密探2025」：`https://pan.quark.cn/s/60d2401dde3a`），
   确认走了猫源解析、进了原生播放器（而不是应用内浏览器）。
2. 停用猫源（或用一个没有对应网盘 token 的猫源），再点同一条网盘链，
   确认回退到应用内浏览器打开。
3. 换一个网盘（例如阿里）的分享链，确认按域名路由到了对应的 token。
4. 确认 token 没有出现在任何日志、报错文案或播放地址里（见 §2.8）。
5. 确认没有猫源的用户点网盘链时行为不变（应用内浏览器，与 v0.0.92 一致）。

## 9. 拿到真机后的验证步骤（猫源基础支持）

> 以下每步都要在真机上做（本机沙箱拦非回环 TCP，且 VM 拉 CatPawPlay 返回 401，
> 本机无法代替真机验证）。先用 CatPawPlay 的订阅 URL 走一遍 §4.3 的网络导入。

1. **订阅加载**：贴 `https://catpaw.douer.me/index.js.md5` 添加订阅；
   确认 md5 校验通过、插件注册成功、`/cat/load` 返回的 `port` 与 `sites`
   与 §2.3、§5.2 的预期一致（记下实际的端口与站点列表）。
2. **配置**：确认 `start(config)` 收到配置对象（`$t.config` 有值）；
   用 ZIP 导入（含 `index.config.js`）时确认配置被正确传进；
   用网络导入（无配置）时确认 `start({})` 不报错、源能正常提供内容
   （无 token 的站点内容不受影响）。
3. **分类**：打开猫源的分类页，确认 `/home` 的分类出现、点分类出列表、
   翻页正常；筛选（`filter`/`filters`）有则测，无则跳过。
   列表“暂无内容”但无报错时，先分清是两条链路中的哪一条：分类页上
   每个分区预览各调一次 `catalog.list`（一分类一调）；点“推荐分区”
   / “信息流”入口走的是 `home.getSections`（一次扇出）。两条链路的
   诊断行都在插件日志里（来源 `"cat"`，只记数量与形态，不记地址原文）。
   `[cat] list` 行首标出分类 id（例：`[cat] list [nodejs_douban::movie]
   转换后：…`）；源返回空时才记一笔 info——`[cat] list […] 转换后：共
   0 条，缺 id 或标题 0 条；封面 https 0 条、回环 http 0 条、其它 http
   0 条、无封面 0 条、非 http(s) 0 条`，返回条目正常、校验没丢的调用不
   记（没行就是正常；每笔都记会把日志刷爆——2026-11-26 一次打开分类
   页就触发限速丢了 302 行）；校验丢了条目再记一笔 warn——`[cat] list
   […] 校验丢条目：20 条→0 条（转换后封面：…）`。读法：先找对应分类
   id 的那行，有“共 0 条”就是源 `/category` 本来就返回空；有“校验丢
   条目 N 条→0 条”就是校验丢掉——再看封面形态计数，回环 http 占多数
   且插件没声明 `allowInsecureHttp`（见 §6.4）时，就是封面校验
   （`model.rs` 的 `media_item` 走 `resource()`，`http:` 要声明才放行）
   把整条丢掉；“N 条→100 条（触顶 100 条上限）”不是丢坏条目，是列表
   校验上限 100 条的截断（`model.rs` 的 `catalog.list` 分支），该分类
   本来就有内容，分区预览只取 8 条，不受影响；两行都没有，说明调用正常返回了条目、或调用根本没到
   Rust——前端对 `catalog.list` 有 5 分钟元数据缓存，可能直接用了之前
   的空页，点分区上的“重试”强制重调一次再看日志。
   `[cat] sections` 是 `home.getSections` 的对照行——`[cat] sections
   转换后：共 S 个分区，合计 N 条`（有空分区时后跟 `；空分区 E 个
   （站点 key…）`），校验丢了分区或条目再记一笔 warn——`[cat] sections
   校验丢内容：S 个分区→S' 个，N 条→M 条`。校验丢条目时每条还有一笔
   通用警告（`items[N].id: 重复条目已丢弃` / `items[N]: …`），同类丢
   弃只列前 3 条明细、余下的记一笔总数（`items: 重复条目已丢弃 N 条
   （仅列出前 3 条）`），免得一次扇出丢几百条时刷满 240 行/60 秒的
   日志缓冲、埋掉别的信号行；丢了多少总数看 `[cat] sections` 的对照
   行。2026-11-26 实测：推荐分区 79 个分区、1824 条→901 条，923 条
   是同一分区内 id 重复被丢掉，分区本身一个没少，推荐分区仍能显示。
   2026-11-26 豆瓣|首页六个分区在 v0.0.100 上重试后全变“暂无内容”，
   就是这个待查的病例：当时日志里只有几行“共 42 条”（其它分类的正
   常调用）、没有分类 id、302 行被限速丢掉，对不上分区，才补了上面这
   两处。
4. **详情**：点一部片进详情，确认线路（`vod_play_from` 的 `$$$` 拆分）与
   选集（`vod_play_url` 的 `#`/`$` 拆分）都对得上，封面与简介正常。
   详情 0 集时查插件日志里的两行（来源 `"cat"`，只记数量与长度，
   不记地址原文）：`[cat] detail 转换后：夸克原画 3集 最长id 120字节…`
   是转换后的样子；`[cat] detail 校验丢分集：夸克原画 3集→0集
   （最长id 300字节，上限2048字节）`说明分集 id 超长被校验丢掉——
   分集 id 含站名前缀后不得超过 2048 字节（`model.rs` 的 `episode`
   校验；分集 id 是分享链接，实测最长 565 字节；列表本身会留下来
   变空，所以界面上是“有线路、0 集、没报错”）。
5. **搜索**：搜一个关键字，确认 `/search` 返回列表、点结果能进详情。
6. **播放**：点一集播放，确认 `/play` 返回的地址能播（画质对、请求头对）；
   若返回的是经 `/proxy` 或 `/m3u8-proxy` 的签名地址，确认播放器能直接播；
   若返回的是 `push://`，按 §6.3 的约定验证。点播报“没有合法可播放流”
   但详情里集数正常时，先看 `/play` 返回的地址是不是
   `http://127.0.0.1:<端口>/…` 的回环 http 地址：是的话就是猫源插件
   没声明 `allowInsecureHttp`（见 §6.4），校验把流全丢了——这是
   2026-11-26 齐天圣瞳在 v0.0.100 上的真机故障，根因链是
   `playback()` 校验 → `model.rs` 的 `url()` 只认 `https:` →
   猫源 Meta 缺 `allowInsecureHttp`（`Meta.allow_insecure_http` 缺省 false）。
   点播报“episodeId 标识无效”时，是另一处上限：播放前读进度
   （`library.progress.get`）用的 `library.rs` 的 `ident()` 只认 256
   字节以内的 id，网盘分集 id（分享链接，实测 565 字节）直接被挡掉——
   分集 id 在片库进度里走 `EPISODE_ID_MAX`（2048 字节），与 `model.rs`
   的分集校验同限。这是 2026-11-26 齐天圣瞳在 v0.0.101 上的真机故障：
   此时“没有合法可播放流”已修好（`media.prepare` 成功取到了流），是读
   进度这一步先报了错，播放页才显示加载失败。
   点播报“播放失败，已尝试可用播放流”时，是取流被拦：`media.prepare`
   成功后播放器调的是宿主媒体服务（`{base}/hpp/{id}/0`），宿主再去取
   源的 `/proxy` 回环地址；严格安全模式（默认开）下 `Network::target`
   禁止回环地址，`proxy_upstream` 只有 Python 边车代理入口走裸 wreq
   直取，猫源服务地址不在豁免里——每条流都取不到，刷新与换流也一样，
   最后报全流失败。这是 2026-11-26 齐天圣瞳在 v0.0.102 上的真机故障。
   修法：宿主登记已加载的猫源服务地址（`manager.rs` 的
   `is_trusted_loopback_url`，与 Python 代理入口同理），取流与 HLS
   清单改写时一并豁免；海报不受影响（前端 `<img>` 直接调 `/pic`，
   不走宿主取流）。
7. **启停与删除**：停用后确认猫源从资源库/搜索消失；重新启用后恢复；
   删除后确认订阅记录、JS 文件、配置文件、`ABC.db.json` 都清掉，
   设置与其他源不受影响（§4.7）。
8. **代理**：跟随系统与直连两种模式下各跑一遍分类与播放，确认猫源的请求
   跟随代理设置（§5.4）；出口 IP 用 `fm.req("https://api.ipify.org", …)`
   核对（与 `node-engine.md` §6.5 同一口径）。
9. **凭证安全**：清完日志后检查 `ext.log` 与运行日志，确认没有出现
   token、cookie、`apiKey` 的值；`GET /config` 的原始响应没有出现在任何日志里。
10. **崩溃**：复现一次崩溃（闪退回桌面）后重开 App，设置 → 运行日志，
    点刷新看最后几行。猫源每次加载/卸载/调用边界都会往运行日志
    （落盘的 `debug.log`）写一行 `[crumb]` 面包屑，只记方法与站点
    key，不记地址原文与凭证（§2.8）：`[crumb] cat <订阅 id> 加载开始/
    完成`、`卸载开始/完成`、`调用 <方法> <站点> 开始/完成（Nms）`，
    失败记 `失败：<错误码>`。读法：`[panic]` 行前面一行就是崩前最后
    一步——有“开始”没“完成”的那次调用就是崩时正在飞的操作；`[panic]`
    行本身给崩点（文件:行号）。注意插件日志（来源 `"cat"` 的
    `[cat] …` 行）只在内存里，崩了就没了，崩后只能看运行日志里的
    面包屑；也别在复现前清缓存（清缓存会删 `debug.log`）。

每步的结果（通过/失败、实际的端口、站点列表、返回结构）回填到本设计文档，
适配器按实测定稿；若 CatPawPlay 的实际行为与本设计不符，先改设计文档，
再改实现。

## 10. 风险与未定事项

- **真源已到，本设计的“真源未到”堵点已解除**：`catpaw.zip` 的 md5 已验证，
  `index.js` 的结构（入口、服务、路由、签名、库文件）已按真源查实。
  剩余未定的是运行时行为（§9 的真机验证），不是格式。
- **服务绑 `0.0.0.0`（局域网暴露）**：源监听全部网卡，同一局域网内的其他设备
  能直接访问它的 HTTP 服务（包括可能含凭证的 `GET /config` 与取流代理）。
  第一版接受源的默认绑定（改不了，`listen` 的 host 写死在源里），
  但要在文档与 UI 里写明：猫源服务只应在可信网络下启用；后续可评估在
  边车侧用防火墙/回环代理收窄（不在第一版做）。
- **`proxySecret` 每次启动随机**：签名地址只在当次启动内有效，重启后失效。
  收藏、历史记录不得存签名后的代理地址（见 §6.3）。
- **`messageToDart` 的 Dart 端口发现方式未确认**：`catDartServerPort` 在包内
  只出现一次（调用处），定义不可见。第一版接受 `messageToDart` 为空操作
  （源已能容忍，见 §5.5）；要接 Action 时先确认它从哪里取端口。
- **源的上游请求是否跟随代理**：待真机验证（§5.4、§9 第 8 步）。
- **Node 18 已 EOL**：与 `node-engine.md` §3.4 同一结论——nodejs-mobile 钉死
  v18.20.4 是假设检验，不是长期运行时承诺；猫源跑在同一个引擎上，继承同一
  维护状态与安全补丁缺口。
- **闭源 App 代码绝不抄**：本设计只用公开协议（CatPawOpen、FongMi 蜘蛛文档、
  TVSpider）与用户发来的真源 `index.js`（用户自己的源文件，按结构使用）、
  HGhub 自有代码；另一 App（截图来源）的实现未读、不用。
- **订阅 URL 的可达性**：订阅 URL 是第三方服务，可能失效、换域名、加验证
  （VM 的 401 就是一例）；导入失败时要报明错、不留坏记录（§4.3 第 6 步），
  更新失败时保留旧版（§4.5）。
- **源的任意代码执行**：猫源 JS 是用户主动导入的第三方代码，在 Node 引擎里
  以 App 的身份运行。第一版的边界：只接受用户主动导入（订阅 URL 或 ZIP），
  不接受页面静默拉取；加载时只给核心模块与注入的辅助，不给文件系统写权限
  （除插件目录外）与 `child_process`（iOS 本来就没有）；源的网络请求走
  代理与限速（§5.4），不绕过；服务绑 `0.0.0.0` 的风险见本节第二条。
  更细的沙箱（VM 上下文隔离）是后续工作，不在第一版做。
- **配置文件的凭证**：见 §2.8。设计文档、日志、诊断输出里绝不出现凭证值。
