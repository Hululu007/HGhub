# Node 引擎（WebHome 请求走真 Node）

> 状态：v0.0.70 设计文档。本文档是实现依据，与实现同一提交交付。
> 本文档只写 v0.0.70 的设计与约束；许可证、体积、维护状态的结论见 §3、§10。

## 1. 目标与背景

### 1.1 为什么走到这一步

WebHome 玩偶站分类请求连续三版被 Cloudflare 拦截（同一部手机、同一个网络、同一个站、同一个页面）：

| 版本 | 做法 | 真机结果 |
| --- | --- | --- |
| v0.0.67 | reqwest + native-tls | 被拦 |
| v0.0.68 | wreq + Chrome 149 指纹模拟 | 被拦 |
| v0.0.69 | wreq + `http1_only()`、Node 式极简画像（无 sec-ch-ua、裸 UA 原样） | 被拦（2026-11-23 16:20 用户截图） |

对照组 Arcane（同一部手机、同一个网络、挂代理和直连两种模式都能过）的做法是：页面 → WKWebView 消息 → Swift → **内嵌 Node.js 引擎（NodeMobile v18.20.4）** → Node 的 `https.request` 发请求。Arcane 的分析只读了行为，没有抄任何代码。

结论：wreq 模仿出来的画像不够，剩下的差异就是**引擎本身**。v0.0.70 把真正的 Node 引擎嵌进来，让插件 net 请求走 Node 的 `https.request`，从根本上复刻 Arcane 的请求行为。

### 1.2 这是一次假设检验，不是保证

如果真 Node 也被拦，说明“画像假说”本身有问题（例如拦截与引擎无关），到时如实回报，转向 WKWebView `fetch` 或真浏览器拿 clearance cookie 的路线，不要在 Node 上硬凑。

### 1.3 范围

- 只改**插件 net 请求**这一路（`Network::fetch` / `fetch_once` / `buffered`，即 JS 插件的 `hg.fetch` 与 WebHome 页面的 `fm.req`，以及分发清单拉取）。它们在 iOS 上走 Node 引擎。
- **不动**：媒体取流（`Network::request` + `Queue::Media`）、下载、代理测试（`proxy::probe`）、Python 边车。`Network::request` 本身不动（媒体要流式读体，端点探测只读状态）。
- 接口不变：`fm.req` / `hg.fetch` 的签名、返回值、错误码、协议文档里的调用约定都不变，只换请求实际发出的那一层。协议不改版。

## 2. 架构总览

```
WebHome 页面 / JS 插件
  │  fm.req / hg.fetch（接口不变）
  ▼
hpp webhome::dispatch / runtime
  │  Transfer JSON
  ▼
Network::fetch → fetch_once → buffered → shared_hop → read_hop
  │  Rust 侧不变：目标校验、DNS 解析+固定、Cookie 罐、限速/去重/
  │  缓存/重试、重定向环、超时、maxBytes、字符集
  ▼
read_hop 里的传输分发（新增，见 §5）：
  ├─ Queue::Api + iOS + Node 可用 → Node 边车（本版新增）
  └─ 其他（Queue::Media、非 iOS、Node 不可用）→ wreq（原样）
        │
        ▼
Node 边车（进程内独立线程 + 回环 HTTP，见 §4）
  host.js：HTTP 服务 127.0.0.1:55101（/health、/request）
  /request → Node https.request → 目标站
        │
        ▼（返回 {status, headers, bodyBase64}，Rust 侧拼回 BufferedResponse）
```

Node 边车与 Python 边车是**同一架构范式**（进程内解释器线程 + 回环 HTTP + info 文件发现地址），只是换了一个运行时：

| | Python 边车（v0.0.53 起） | Node 边车（本版新增） |
| --- | --- | --- |
| 运行时 | BeeWare CPython 3.12.16（Python.xcframework） | nodejs-mobile v18.20.4（NodeMobile.xcframework） |
| 入口 | `app/main.py`（Flask） | `node-app/host.js`（Node `http` 服务） |
| 启动 | Rust 裸 FFI 调 `Py_Initialize` + `PyRun_SimpleString` | Rust 裸 FFI 调 `node_start`（见 §4.1） |
| 发现地址 | `<app_data_dir>/python-sidecar.json` + `/health` | `<app_data_dir>/node-sidecar.json` + `/health` |
| 默认端口 | 55001（被占落临时端口） | 55101（被占落临时端口） |
| 宿主客户端 | hpp `python::client::SidecarClient`（wreq + `no_proxy`） | hpp `node::client::NodeClient`（同款） |

## 3. 引擎：nodejs-mobile v18.20.4

### 3.1 选型与版本（已实测核对）

- 项目：社区组织 `nodejs-mobile/nodejs-mobile`（JaneaSystems 原仓已声明不再积极维护，后续开发交给该社区组织）。
- 版本：**v18.20.4**（发布于 2024-10-07），即 Node.js **18.20.4**，V8 10.2.154.26-node.37、libuv 1.44.2、OpenSSL 3.0.13+quic。
- 下载：`https://github.com/nodejs-mobile/nodejs-mobile/releases/download/v18.20.4/nodejs-mobile-v18.20.4-ios.zip`
  - 大小 **51,492,431 字节**（与 release 资产明细一致，2026-11-23 实测下载核对）。
  - sha256 `8c5ca3a0d1e38de7f182a5642593e82593b820efd375a14b3ecafc4bcfee620e`（实测下载所得；release 页未公布官方校验值，见 §3.4）。
- 包内：`NodeMobile.xcframework`（`ios-arm64` + `ios-arm64_x86_64-simulator` 两个 slice）+ Node 头文件。
  - device slice 的 framework 二进制 **54,959,120 字节**（未压缩）；`CFBundleIdentifier` 为 `com.janeasystems.NodeMobile`；`MinimumOSVersion` 为 **13.0**（不高于应用现有底线）；Xcode 15C500b / SDK iphoneos17.2 构建；动态 framework。
  - 二进制内含 `v18.20.4` 字串，已确认是 Node 18.20.4。

### 3.2 启动入口（关键纠正）

**v18.20.4 的 framework 暴露的是纯 C 入口，不是 Obj-C API。**`NodeMobile.framework/Headers/NodeMobile.h` 全文只有一个声明：

```c
int node_start(int argc, char *argv[]);
```

二进制里有 `_node_start` 符号，没有 `NodeRunner` / `startNodeWithArguments` 的踪影（那是旧版文档里的 Obj-C  wrapper，本版已无）。因此：

- **不需要 Swift 桥。**Rust 直接用裸 FFI 调 `node_start`，与 Python 边车调 `Py_Initialize` / `PyRun_SimpleString` 是同一套做法（见 §4.1）。
- `node_start(argc, argv)` 是 Node 的标准嵌入入口：在调用线程上跑起 Node 事件循环并阻塞到引擎退出。`argv` 形如 `["node", "<bundle>/node-app/host.js"]`。
- 调用线程需要 **2MB 栈**（官方文档要求运行时 1MB、推荐 2MB），Rust 侧用 `std::thread::Builder::stack_size(2 * 1024 * 1024)` 保证。

### 3.3 许可证（已核对公开资料，不是法律意见）

- nodejs-mobile 核心库与 Node.js 同许可：**MIT**（Node.js 许可 bundle，版权 "Copyright Node.js contributors"，Janea Systems 的修改部分同样 MIT）。
- 随 bundle 分发的第三方许可：V8（BSD）、libuv（MIT）、OpenSSL 3.0（Apache-2.0）、zlib（zlib）、c-ares/nghttp2/brotli（MIT/BSD/Apache）、npm（Artistic 2.0）、ICU（Unicode V3）。
- 与 HGhub 的 **PolyForm Noncommercial 1.0.0 兼容**：MIT / Apache-2.0 / BSD / ISC 都是 permissive 许可，不强加 copyleft、不强制项目改许可，可以作为依赖分发。**必须随包带上 Node LICENSE 全文 + 第三方 NOTICE**（不要只贴一个 "MIT"；GitHub 把该仓库 LICENSE 识别为 Other/NOASSERTION，正因为它是复合 bundle）。
- 若未来引入 npm 包（本版不引入，见 §6），各包许可多为 MIT，同样须列入 NOTICE；**绝不引入 GPL/AGPL 的代理或 TLS 相关包**。
- 正式发布前建议法务/作者复核一次。

### 3.4 维护状态与 EOL（必须如实记下）

- Node 18 本身已 EOL：上游最后更新 18.20.8（2025-03-27），安全支持止于 **2025-04-30**；nodejs-mobile 钉的 18.20.4（2024-07）比上游最后版还旧，约一年半的安全补丁缺口。
- JaneaSystems 原仓最后 push 2021-10-27；社区仓 2023 年秋 "reboot" 推到 18.20.4 后又沉寂；Node 22 的 PR 未合并发布。两个仓库均未归档，但实际处于停滞状态。
- 单人 fork `jo32/nodejs-mobile` 做到 Node v26.13.0（最低 iOS 17.0），无官方背书，**本版不用**，只作为“有人能把新版 Node 编出来”的存在性证明。
- **定位**：v0.0.70 是 pin 死 v18.20.4 的假设检验，不是长期运行时承诺。PoC 通过后是否长期用，要另做维护决策（自担 framework 二进制与安全补丁，或评估接手新版 fork）；PoC 不通过就停掉 Node 路线。
- release 页未公布 zip 的官方 sha256；§3.1 的值是实测下载所得，`sync-ios-node.mjs` 里以“大小 + sha256”双重校验 pin 住（与 Python 的 3.12-b13 同款做法）。

### 3.5 体积

- device slice framework 二进制未压缩约 55MB；进 IPA 后按 Mach-O 二进制的常规压缩率估算，**IPA 增大 ~25–35MB**（现 IPA 约 21.5MB，v0.0.70 预计 ~45–55MB），以实际打出的 IPA 为准。
- 无官方 slim 版；自编裁剪（`--without-inspector`、small-icu 等）等于接手 Node 交叉编译工具链，**本版不做**。

### 3.6 运行时约束（nodejs-mobile 的硬性限制）

- **单实例、不可重启**：一个应用内只能启动一个 Node 实例；引擎退出后不能再启动。`process.exit()` 会直接杀掉整个 App（且违反 App Store 指南）；`child_process` / `cluster` 不可用（iOS 不允许 fork/spawn 新进程）。
- **无 inspector、无 intl**：V8 inspector 与 `intl` 模块在当前构建中不可用（host.js 不用它们）。
- **`process.cwd()` 返回 `/`**：host.js 里所有路径必须用绝对路径（info 文件路径由环境变量传，见 §4.2）。
- **崩溃即全 App 崩溃**：Node 引擎 fatal（V8 致命错误）会杀掉整个进程，CPython 也一样；两者任一崩溃 = 整个 App 崩溃，无隔离。host.js 必须顶层捕获异常、永不调 `process.exit()`。
- JIT-less V8（iOS 禁止 JIT）：对发 HTTP 请求这类 I/O 型任务不是瓶颈。

## 4. 启动与生命周期

### 4.1 Rust 侧：裸 FFI + 引导 trait（照抄 Python 边车）

hpp 新增 `node` 模块（`node/mod.rs`、`node/client.rs`），与 `python` 模块同款结构：

```rust
// hpp/src/node/…（示意）
pub trait NodeBootstrap: Send + Sync {
    fn ensure_started(&self) -> BoxFuture<'static, Result<NodeInfo>>;
}
pub struct UnavailableNode; // 非 iOS 目标：任何启动都报 UNSUPPORTED
pub struct NodeInfo { pub api_base: String }
```

应用层 `src-tauri/src/node.rs` 实现 `IosNodeBootstrap`（照抄 `src/python.rs` 的 `IosPythonBootstrap`）：

- 持有 `info_path`（`<app_data_dir>/node-sidecar.json`）、`started` 标志、`starting` 互斥锁、`startup_error`。
- `ensure_started()`：加锁 → 未启动则 `start_engine_thread()` → `probe()`（轮询 info 文件 + `/health`，30 秒 deadline；超时或启动错误报明错）。
- `start_engine_thread()`：先校验包内布局（`node-app/host.js` 存在；不存在先报明错，不要等引擎启动失败），设环境变量（`HG_NODE_INFO_PATH`），用 `stack_size(2MB)` 起一个名为 `hg-node` 的线程，在线程里调 `node_start`。传给 `node_start` 的 argv 必须是一段连续内存（v0.0.72，libuv 要求）：单个 `Vec<u8>` 缓冲区先写 `node\0` 再写 host.js 路径 + `\0`，两个指针都指向该缓冲区——之前两个独立 `CString` 是两次堆分配，字符串不相邻，是潜伏的崩溃点。**v0.0.71：`node_start` 不再强链接**——启动引擎前先 `dlopen` 包内 framework（`.app/Frameworks/NodeMobile.framework/NodeMobile` 的绝对路径，绕开 @rpath），再 `dlsym` 出 `node_start`，函数指针传进引擎线程再调用（见 `src-tauri/src/node.rs` 的 `ffi::load_node_start`）。v0.0.70 强链接时 App 在闪屏后闪退，改成懒加载后启动时 dyld 不碰 framework，加载失败只会让当次插件请求报错（"Node 引擎加载失败：…"），不会拖垮启动；句柄故意不保存、永不 `dlclose`（Node 只允许启动一次，库必须常驻）：

```rust
#[cfg(target_os = "ios")]
mod ffi {
    // dlopen / dlsym / dlerror 是 libSystem 的导出符号，直接声明，不用加依赖。
    extern "C" {
        fn dlopen(path: *const c_char, mode: c_int) -> *mut std::ffi::c_void;
        fn dlsym(handle: *mut std::ffi::c_void, symbol: *const c_char) -> *mut std::ffi::c_void;
        fn dlerror() -> *mut c_char;
    }
    const RTLD_NOW: c_int = 2; // Darwin 值：加载时立即解析全部符号
    pub type NodeStartFn = unsafe extern "C" fn(c_int, *mut *mut c_char) -> c_int;
    pub fn load_node_start(framework: &Path) -> Result<NodeStartFn, String> { /* … */ }
}
```

- 引擎线程正常情况下永不返回；`node_start` 返回即引擎已死（Node 不能二次启动），记 `startup_error`，之后所有请求报明错（"Node 引擎已停止，请重启应用"），**不尝试重启**。
- `PluginManager` 增加 `node_bootstrap: Arc<dyn NodeBootstrap>` 字段（默认 `UnavailableNode`），builder 方法 `with_node_bootstrap(...)`；`src-tauri/src/plugins.rs` 在建 manager 时像 `python_bootstrap` 一样传入（info_path 为 `<app_data_dir>/node-sidecar.json`）。`Network::new` 增加一个参数接这个句柄（两个构造点：`manager.rs::installed` 与 `distribution.rs::distribution_network` 都要传）。

### 4.2 Node 侧：host.js（照抄 Python 边车 main.py）

`src-tauri/node-sidecar/host.js`（只用 Node 核心模块，不引 npm 包）：

- 启动时读环境变量 `HG_NODE_INFO_PATH`；在 `127.0.0.1` 上起 `http` 服务，默认端口 **55101**，被占则落临时端口（先拿原生 socket 试绑，绑上再交给 server，参考 main.py 的 `bind_server` 做法）；实际端口写进 info 文件（`{"apiBase": "http://127.0.0.1:<port>"}`，先写临时文件再改名）。
- 端口/地址变化时重写 info 文件并更新 `/health` 返回值（与 Python 边车自愈重绑同一语义；Rust 侧按 §8 的重探逻辑跟进）。
- 端点：
  - `GET /health` → `{"ok": true, "node": process.version}`（`process.version` 应为 `v18.20.4`，验包与诊断时核对）。
  - `POST /request` → 见 §5.2。
- 自愈：`server` 的 `error` 事件重建监听（不退出进程）；顶层 `uncaughtException` / `unhandledRejection` 只记日志（写 stderr，宿主日志可见），**绝不调 `process.exit()`**；证书校验保持开启（Node 自带 Mozilla 根证书，与 Python 边车因无 CA 根而关闭校验不同，**不许关**）。
- `process.cwd()` 是 `/`，host.js 里不依赖相对路径；`require` 只用核心模块，不读包内其他 JS 文件（除非实现时明确需要）。
- `node-app/` 里还有一份 `package.json`（`{"type": "commonjs"}`）：host.js 用 `require` 写 CommonJS，但仓库根 `package.json` 是 `type: module`，没有这份文件时本机测试会把 host.js 当 ESM 跑直接报错；iOS 包里 node-app/ 下本来就没有 package.json，同样需要它定死 CommonJS。

## 5. 请求通路

### 5.1 分发点：read_hop

`Network::send()`（`network/api.rs`）**不动**——它仍是 wreq 路径，供 `Network::request()`（媒体取流 `Queue::Media`、端点探测 `Queue::Api`）使用，返回 `wreq::Response`（媒体要流式读体，不能改）。

只改 `fetch` 路径的 `read_hop()`（`network/api.rs`，`fetch_once` → `buffered` → `shared_hop` → `read_hop` 的末端）：

```rust
// read_hop 内，现有 self.send(outgoing, Queue::Api, cancel) 之处改为：
let hop = self.send_api_hop(outgoing, cancel).await?; // 返回 HopResponse { status, headers, bytes, redacted }
```

`send_api_hop` 的规则：

- iOS 且 Node 引导可用 → 走 Node 边车（§5.2）。
- 否则（非 iOS、或 Node 引导是 `UnavailableNode`）→ 走现有 `send()` + 缓冲读体（即今天 `read_hop` 做的事，原样保留）。
- 调度与 Cookie 语义与今天完全一致：`check_cooldown`、`permits`（插件 6 / 单主机 4）、`rate_limit`（5 次/秒、突发 15）、`receive_cookies`、`429/503` 的 `Retry-After` 冷却——能复用现有方法就复用（它们已是 `Network` 上的独立方法），不要复制第二份限速逻辑；`429/503` 冷却写入（约 12 行）若无法复用可小段重复，并在注释里写明与 `send()` 的对应关系。
- 并发许可保持到响应体读完或取消（协议 03 §3）：Node 一次返回完整 body，许可在 `send_api_hop` 返回时释放，与 wreq 分支“先取许可、读完再放”的效果一致。

`Network::request()`、`buffered()` 的重定向环、`outgoing()`（Cookie 附加 + UA 默认值）、`redirect()`（至多 5 跳、禁止 HTTPS 降级、每跳重校验）都不动——重定向仍由 Rust 逐跳驱动，每跳都是一次独立的 Node 单跳请求，Node 侧**不**自己跟重定向（`redirect: "manual"` 语义由 Rust 控制）。

### 5.2 Node 边车的 /request 协议

Rust → Node（`POST /request`，JSON）：

```json
{
  "url": "https://www.wogg.net/vodshow/1--------1---.html",
  "method": "GET",
  "headers": {"User-Agent": "Mozilla/5.0", "Referer": "https://..."},
  "bodyBase64": null,
  "timeoutMs": 15000,
  "proxyUrl": null,
  "pinnedAddrs": ["104.18.32.0:443", "2606:4700::6812:2000:443"]
}
```

- `headers` 是 Rust 的 `headers()` 已过滤后的头（已去掉 `Host`、`Content-Length`、`Connection`、`Transfer-Encoding`、`Accept-Encoding`；页面给的 UA 原样在内，缺失时 Rust 已补 `HGhub/HPP-dev`——与今天 `outgoing()` 的行为一致）。
- `bodyBase64`：有 body 时才带（GET/HEAD 不带；Rust 侧已保证）；上限 1 MiB（Rust 侧已校验）。
- `timeoutMs`：Rust 侧已 clamp 到 1–30 秒。
- `proxyUrl`：本跳生效的代理地址（见 §6），`null` 表示直连（无 HTTP 代理；TUN 仍在 socket 层生效，见 §6.1）。
- `pinnedAddrs`：v0.0.76 起恒为空列表——Rust 侧 `network_policy::resolve_target_keep_order` 只做预检（校验 + 诊断，见 §7），Node 用默认 DNS（与 Arcane 完全一致）。`makeLookup` 在 pinnedAddrs 非空时仍会用自定义 lookup（测试与将来恢复固定时用），生产路径上不再出现。

Node → Rust（HTTP 200 + JSON；边车自身错误时 HTTP 500 + `{"error": "..."}`）：

```json
{
  "status": 200,
  "headers": {"content-type": "text/html; charset=utf-8", "set-cookie": ["a=1; Path=/"]},
  "bodyBase64": "..."
}
```

- `status` 是上游实际状态码（3xx/4xx/5xx 都是正常返回的数据，不是错误——重试与缓存逻辑仍由 Rust 的 `fetch()` 按状态码判断，与今天一致）。
- `headers`：Node 侧 `res.headers` 原样（`set-cookie` 保持数组；其余值 Node 已合并成字符串）。
- `bodyBase64`：完整响应体（base64；空体为空串）。Rust 侧解码后按 `maxBytes` 校验（超过报 `UPSTREAM_ERROR`，与今天一致）；Node 侧同样设 8 MiB 上限，超限直接报错，不要无界读内存。
- 超时、连接失败、TLS 失败、DNS 失败：Node 返回 HTTP 500 + `{"error": "...", "code": "..."}`，Rust 按 `code` 映射（`TIMEOUT` → `TIMEOUT`、`NETWORK` → `NETWORK`、`UPSTREAM_ERROR` → `UPSTREAM_ERROR`，无 `code` 时为 `INTERNAL`）；不要把传输失败伪装成 HTTP 状态码。`code` 字段是实现时加的（设计初稿只写了 `{"error"}`）：没有它，Rust 无法区分上游超时与边车内部错误，`fetch` 的 GET 重试只认 `NETWORK` / `TIMEOUT`。

Node 侧发请求的要求（复刻 Arcane 的行为，也是本版要验证的核心）：

- 用 Node 核心 `https.request`（`http.request` 用于 http 目标），**不**用 `fetch`（Node 18 的 `fetch` 是 undici，画像与 `https.request` 不同；Arcane 用的是 `https.request`）。
- **不**加任何默认头：Node 默认只发 `Host`（+ 有 body 时的 `Content-Length`），`User-Agent`、`Referer` 等只发 Rust 传来的；**不**发 `sec-ch-ua`、`Accept`、`Accept-Language` 等浏览器头。
- HTTP/1.1：Node 的 `https.request` 默认就是 HTTP/1.1（不用 `http2` 模块）。
- TLS：Node 自带的 OpenSSL（3.0.13+quic）默认握手，证书校验开启；**不**改 `ciphers`、`ALPN`、`servername` 以外的 TLS 参数。`servername`（SNI）取目标 hostname（Node 默认行为；用自定义 `lookup` 时仍要显式确认，见 §7）。
- 连接不复用：自定义 agent 设 `keepAlive: false`（每次请求新连接；插件 API 速率低，不复用无性能问题，且与 Arcane 的一次性请求行为一致）。
- 重定向：Node 侧**不**跟（Rust 逐跳驱动）；收到 3xx 时只返回状态码与 `location` 头。

### 5.3 Rust 侧的 NodeClient

hpp `node/client.rs`，照抄 `python/client.rs` 的 `SidecarClient`：

- `NodeClient::new(api_base)`：wreq 客户端 + `.no_proxy()`（回环地址绝不能走代理；Python 边车客户端同款）。
- `health(timeout)` → `NodeInfo`（同时校验 `process.version` 以 `v18` 开头，版本不对报明错）。
- `request(req: NodeRequest, timeout)` → `NodeResponse`（POST `/request`，JSON；超时 → `TIMEOUT`；连不上 → `INTERNAL` "Node 边车连接失败"；HTTP 500 + `{"error", "code"}` 按 `code` 映射：`TIMEOUT` → `TIMEOUT`、`NETWORK` → `NETWORK`、`UPSTREAM_ERROR` → `UPSTREAM_ERROR`，无 `code` 时为 `INTERNAL`，错误文本截断 1000 字）。
- `base()` 供调试与日志。

## 6. 代理（本版最关键的设计点）

### 6.1 先说清楚 TUN 与 HTTP 代理是两层

- **Karing 的 TUN（Network Extension）在内核路由表层面生效**：所有 socket（包括 Node 的裸 socket）的 IP 包都会按路由表进入 TUN 接口；Node 无法“绕过”一个已激活的全流量 TUN。TUN 内的分流（某条流直连还是走美国节点）由 Karing 自己的规则定。
  - 因此 "Node 直连 = 真实 IP" 只在 Karing 把该流判为直连时成立，不能从 "Node 不读系统代理" 直接推出。Arcane 挂代理也能过，说明它的 Node 流量在 Karing 下的出口并没有决定成败。
  - 我们的 Node 流量同样会进 TUN、同样按 Karing 规则走——**TUN 层的行为与 Arcane 一致，不需要、也不可能在应用层绕过**。
- **HTTP 代理是另一层**：Node 18 的 `https.request` 默认不读系统代理设置，也不读 `HTTPS_PROXY` 环境变量（`NODE_USE_ENV_PROXY` 是 Node 22.21+ 才有的，Node 18 没有）。所以应用层的代理设置（跟随系统/直连/自定义）必须由 Rust 侧解析、显式传给 Node。

### 6.2 代理地址的解析（Rust 侧，复用现有逻辑）

每跳请求生效的代理地址按现有 `proxy::configure` 的同一规则解析（**不要写第二份解析逻辑**；`proxy.rs` 里把"算出 effective proxy URL"抽成一个 `pub(crate) fn effective_proxy_url(config: &Config) -> Option<String>`，`configure` 与 Node 路径共用）：

| 用户设置 | 生效代理 |
| --- | --- |
| 直连（Off） | `None`（无 HTTP 代理；TUN 仍在 socket 层生效，见 §6.1） |
| 自定义（Custom） | 自定义地址（`normalize_proxy` 已校验：`http` / `https` / `socks5` / `socks5h` / `socks4` / `socks4a`，可带认证） |
| 跟随系统（System） | `HGHUB_PROXY` 环境变量（非空时优先），否则 iOS 系统代理（`SCDynamicStoreCopyProxies` 的 HTTP/SOCKS 固定代理；只有 PAC 时为 `None`，与今天一致） |

代理设置变化时 `proxy::generation()` +1；Node 路径的请求每次都重新解析（不缓存代理地址），设置改动立即生效（wreq 路径靠 60 秒池重建，Node 路径按请求解析，更及时）。

### 6.3 Node 侧的代理 agent（host.js，手写核心模块，不引 npm 包）

`proxyUrl` 按 scheme 选 agent（每次请求现选，Node 侧不缓存）：

- `null` → 默认 agent（直连 socket）。
- `http://` / `https://` → **CONNECT 隧道**：`net.connect` 到代理 → 发 `CONNECT <host>:<port> HTTP/1.1`（带 `Host` 与认证时的 `Proxy-Authorization: Basic ...`）→ 收到 `200` 后把 socket 交给 `tls.connect({socket, servername: hostname})`；非 200 按代理错误返回。http 目标走同一代理时发绝对 URI 形式的请求行（`GET http://host/path HTTP/1.1`）。
- `socks5://` / `socks5h://` → **SOCKS5 握手**：问候（无认证 / 用户名密码认证）→ CONNECT（`socks5h` 送域名由代理解析；`socks5` 送 Rust 已解析的 IP）→ 成功后 `tls.connect`。认证信息从 proxyUrl 的 userinfo 取。
- `socks4://` / `socks4a://` → **SOCKS4/4a 握手**（`4a` 送域名，`4` 送 IP；用户名取 userinfo 的 user 部分）。
- 以上全部用 Node 核心 `net` / `tls` 手写，**不引 `https-proxy-agent` / `socks-proxy-agent` 等 npm 包**（避免打包 node_modules 与验证第三方包在 Node 18.20.4 + JIT-less V8 下的行为；CONNECT 与 SOCKS 握手都是定型协议，手写约 200 行可控）。
- 代理失败（连不上代理、CONNECT 非 200、SOCKS 拒绝、认证失败）按传输失败返回，Rust 侧文案与今天 `proxy::connection_error` 对应（"无法连接代理目标服务器"等），不要把代理错误伪装成目标站错误。

### 6.4 必须保留的行为

- 用户明确要求：有些 HTML 资源需要代理才能连上，**绝不能默认全直连**。本设计里"直连"只在用户把模式设成"直连"时发生；跟随系统/自定义时代理一定生效（§6.2 的表）。
- 回环地址（Node 边车自身、Python 边车、媒体回环服务）永远 `no_proxy`（Rust 侧的边车客户端同款；Node 侧不对 `127.0.0.1` 用代理——它只接 Rust 传来的外网目标，自身不发回环请求）。
- 代理测试（`proxy::probe`）不动，仍用 wreq Chrome 149 客户端（它是连通性探测，不是插件请求）。

### 6.5 验证（真机）

- WebHome 玩偶站分类在**跟随系统**与**直连**两种模式下都测（与 v0.0.68/69 的验收同一口径）。
- 出口核对：页面里 `fm.req("https://api.ipify.org", …)` 看 Node 实际出口 IP，与 Arcane 同一网络下的结果对照（验证 TUN 层行为一致；这一步由用户在真机上做，**不是**应用里新增的功能）。

## 7. DNS 固定

协议 03 §2 要求"通过检查的解析结果直接用于连接，不在检查后重新解析"。Node 路径在 v0.0.76 之前保留这条规则；v0.0.76 起连接时固定暂停（见最末一条），只保留校验预检。

- Rust 侧在每跳发 Node 请求前调 `network_policy::resolve_target`（与今天 `Network::client()` 里同一调用）：校验（内网/保留地址拒绝、FakeIP 198.18.0.0/15 放行、混合答案只留允许的地址）+ IPv6 优先排序，得到 pinned 地址列表。
- 把列表（`"ip:port"` 字符串数组）随请求发给 Node；Node 的 `https.request` 用自定义 `lookup(hostname, opts, cb)` 返回该列表。**lookup 必须按 `opts.all` 回值**：`all: true` 时回 `[{address, family}]` 数组，`all: false` 时回首个地址字符串 + family。**v0.0.73 的教训**：旧代码无条件回数组；本机 Node 24 上 `autoSelectFamily` 默认开（lookup 以 `all:true` 被调用），测试全过；真机 Node 18.20.4 上 `autoSelectFamily` 默认关（lookup 以 `all:false` 被调用），Node 把数组当成单个 IP，直接抛 `Invalid IP address: [ [Object], ... ]`（个数即 pinned 个数）。所以 `doFetch` 里显式传 `autoSelectFamily: true`（Node 18 也走 Happy Eyeballs，逐个试 pinned 列表，不只试首个），同时 `makeLookup` 按 `all` 回值——两处缺一不可，缺前者 Node 18 只试首个地址，缺后者违反 lookup 契约。
- **v0.0.74：Node 路径改用系统解析器原顺序，不做 IPv6 优先重排。**v0.0.73 修好 lookup 后真机仍被拦截（“响应被拦截”回来了，`Invalid IP address` 没了）——Node 画像假说被证伪。剩下与 Arcane 的最大差别就是 DNS：我们是 Rust 解析 + IPv6 优先重排后钉死，Arcane 是 Node 默认 lookup（系统顺序）。系统顺序下首个地址是 Karing 的 FakeIP（198.18.x.x，经 TUN 走美国节点）；重排后首个变成直连 IPv6 边缘，Node 先连它而被 Cloudflare 拦截。证据链：py 边车（Python `requests`，系统顺序）能播、WKWebView（系统顺序）能打开页面、wreq 路径（同一 `resolve_target` 重排）同样被拦。改法：`send_api_hop` 改调 `resolve_target_keep_order`（校验+固定不变，只是不重排；协议 03 §2 的 DNS 固定仍成立，wreq/代理探测路径仍用 IPv6 优先）。**v0.0.74 真机仍被拦（2026-11-23 20:46–20:47），DNS 顺序假说被证伪**——两种失败并存：有的请求 TLS 握手没建成就被断开（`Client network socket disconnected before secure TLS connection was established`），有的拿到 Cloudflare 拦截页。下一跳见 §10.3（v0.0.75 已做其中的 ② 和 TLS 重试）。
- **v0.0.75：两件事。① 持久化 Cookie 罐里的 `__cf_bm` 不存、不发。**证据链：罐子落盘在 `<app_data_dir>/hpp/<插件 id>/cookies.json`（`Network::new` 读、`receive_cookies` / `cookies(set/clear)` 回写；`app_data_dir` 是应用数据容器，跨版本升级保留）；`fm.req` 没设 `credentials` 时宿主按默认 `include` 把罐子里的 Cookie 附上（协议 04 §5）；`receive_cookies` 对 `Set-Cookie` 不做状态检查、不分名字——挑战页发的 `__cf_bm` 会被存下来、下次再发，失效的 bot 管理 Cookie 让 Cloudflare 一直重新挑战（Arcane 的 net.request 不带 Cookie）。改法：`receive_cookies` 跳过 `__cf_bm`（普通罐与网页登录会话罐都跳过）；`Network::new` 加载时把罐子里残留的 `__cf_bm` 清掉并回写。**注意这个嫌疑只能解释“响应被拦截”，解释不了 TLS 握手被断开**（Cookie 是 TLS 建好之后才在 HTTP 请求里发的）。**② Node 直连路径 TLS 失败时显式换地址重试。**机制已证实：本机 Node v24 插桩复现——坏端点 TCP 接通、读到 ClientHello 后 150ms 断开，请求报错与真机一字不差（`ECONNRESET Client network socket disconnected before secure TLS connection was established`），好端点一次连接都没收到；也就是说 Happy Eyeballs 只在 TCP 接通失败时回落，**TCP 接通后 TLS 被断不会回落**（立即断开时 Node 会当成 TCP 失败而回落，真机上 RST 来自远端边缘、TCP 早已建立，所以不回落）。改法：`doFetch` 循环试 pinned 列表，每次排除掉 TLS 失败的那个地址（从请求 socket 的 `remoteAddress` 认出来）；只在“TCP 已接通、TLS 没完成、还没收到响应”时重试——这时请求字节还没发出去，换地址重发是安全的；超时、已收到响应、握手已完成都不重试。重试是 `doFetch` 自己的循环，不依赖 `autoSelectFamily`，Node 18 与 24 行为一致。**范围：重试只做直连路径。**代理路径不动：HTTP CONNECT 由代理自己解析域名（不用 pinned），SOCKS 只用首个 pinned 地址且失败的那个地址从 socket 上认不出来（`remoteAddress` 是代理）。**诊断：直连失败时报错末尾带上已试的 pinned 地址**（格式 `（pinned: <ip1>, <ip2>）`；没接通过任何地址时列出全部 pinned 地址），下次真机测试能区分是 IP 问题还是指纹问题。
- **v0.0.76：去掉 pinning 的决定性实验——与 Arcane 完全一致。**v0.0.75 真机仍被拦（2026-11-23 21:42–21:43 录屏）：跟随系统模式下臻选→TLS 被断、木偶/臻选→响应被拦截、电影→TLS 被断、动漫→响应被拦截、电视剧→骨架屏约 16 秒后响应被拦截；改直连后臻选/电影/电视剧全是响应被拦截，一次 TLS 被断都没出现；所有 TLS 报错里都没有 `pinned:` 后缀。由此排除：① Cookie 罐不是（主要）原因（清了 `__cf_bm` 拦截页照样来）；② TLS 重试没派上用场（跟随系统走代理路径不适用、直连下的失败全在 HTTP 层）。查到的三条证据：① Rust 的解析器就是系统解析器——tokio 1.54.1 的 `lookup_host` 调 `std::net::ToSocketAddrs`（`spawn_blocking` 里走 libc `getaddrinfo`），并没有绕过 Karing 自建 DNS，所以“Rust 直连 DNS、拿到真实边缘 IP”这个假说在代码层面不成立；② 真机 pinned 列表曾是 24 个 IPv4（v0.0.72 的 `Invalid IP address: [Object]` 个数，v0.0.73 测试注释有记），不是单个 FakeIP——说明当时系统解析器回的就是真实边缘地址；③ Node 18.20.4 的 `autoSelectFamily` 会在首个地址 250ms 未接通时并行试下一个地址（`lib/net.js` 的 `lookupAndConnectMultiple` + `internalConnectMultiple`，`autoSelectFamilyAttemptTimeoutDefault = 250`；首个地址的地址族优先、其余交替——本机 Node v24 实测 `[IPv4, IPv6]` 时先连 IPv4，与 v18 源码一致），而 Arcane 不设它（Node 18 默认关，只试首个地址）——所以我们的请求可能落到直连边缘而不是首个地址。改法：`send_api_hop` 仍调 `resolve_target_keep_order` 做预检（DNS 失败/不合规地址快速失败），但 `pinned_addrs` 传空列表，Node 用默认 lookup；`doFetch` 里不再显式传 `autoSelectFamily`，v0.0.75 的重试循环随 pinning 一起撤掉（没有 pinned 列表就无地址可换）。**代价是 TOCTOU 缺口**：Node 在连接时重新解析，校验与连接之间 DNS 答案再变就拦不住——Arcane 同样有这个缺口（它根本不校验）；若实验证明 pinning 不是原因、要恢复固定，再把列表传回去（协议 03 §2 同步记了这一条）。**诊断补齐**：`v0.0.75` 的 `pinned:` 只在直连 NETWORK 错误上出现——跟随系统走代理路径时不适用、拦截页是 HTTP 200（没有宿主错误）本来就带不上；v0.0.76 起 Rust 层在 Node 请求失败时把预检解析到的地址附在报错末尾（`（解析: …）`，直连/代理、NETWORK/TIMEOUT 都带，取消不带）。**骨架屏转十几秒**：单跳超时默认 15 秒（`network.rs` 的 `timeoutMs` 缺省 15000），v0.0.75 的重试循环受同一 deadline 约束、不会把单次请求拖得比超时更长——长转是目标黑洞（接通后不响应）烧满了超时，不是重试循环造成的；本版不动超时。
- **v0.0.77：WKWebView clearance 路线（§10.3 ④）。**v0.0.76 真机仍被拦（2026-11-23 23:52 录屏）：玩偶 臻选 → 约 5 秒后 `分类失败：无响应被拦截`；木偶 动漫 → TLS 被断，报错带 `（解析: 198.20.19.443）`——新诊断生效，解析到的就是单个真实边缘 IP，不是 Karing FakeIP；随后骨架屏约 25 秒 → `响应被拦截`；电影 → 骨架屏转到录屏结束。结论：**pinning 与 lookup 路径（pinning/autoSelectFamily）全部排除**——去 pinning 没修好，Node 默认 lookup 也没修好；GitHub 上也没有开源的 iOS WebHome 宿主实现可参考（webhtv 只有 Android 宿主、映匣没有 WebHome、CatPawOpen 没有 iOS 壳），这条路只能按第一性原理自己搭。改法：Node 请求拿到拦截页（HTTP 200，体里含 `just a moment` / `cf-browser` / `access denied` 等标记，与页面 `_getInner` 同一组）或 TLS 握手被断时，宿主用 WebHome 窗口自己的 WKWebView 取同一 URL——先挂隐藏 iframe（页面本身不导航，待回的 fm.req 还活着），挑战跑完后用 Tauri 2.12.5 自带的 `WebviewWindow::cookies_for_url` 读出 `cf_clearance`；15 秒内没拿到（例如挑战页禁 iframe）再把窗口本身导航到目标 URL 跑一遍，拿到后导航回首页；整个过程最多 30 秒，拿不到就原样返回原始报错。通行证只认 `cf_clearance`、只存内存（不落盘、不进 Cookie 罐），Node 请求按目标主机匹配后并进 Cookie 头（`credentials: omit` 时不带；已有同名 Cookie 不重复带；Secure / HttpOnly 取 WebView 里那只 Cookie 自身的属性，不硬编码——Secure 的通行证不发给 http 目标）；`__cf_bm` 绝不进这份存储。**注意：`fetch` 不能替这件事**——它不执行挑战页的 JS，拿不到通行证；只有真实导航（iframe 或顶层）能让挑战跑完。**读 WebView Cookie 不用自己接 wry**：§10.3 旧文说“没有现成宿主接口”是过时的——Tauri 2.12.5 已把 `cookies_for_url` / `cookies` / `set_cookie` 包成 `WebviewWindow` 的公开方法（底层就是 vendored wry 的 `WKWebView::cookies()`，经 `WKHTTPCookieStore.getAllCookies`），直接调即可；它是阻塞调用（等主线程回消息），必须放 `spawn_blocking` 里。**诚实边界**：本地验证不了真机挑战过程——WebView 的 `fetch`/导航对分类 URL 是否通、挑战是否自动通过（需人点验证框的挑战等再久也不会自己通过），只能真机验收；若 WebView 也被拦，clearance 路线不成立。另注意：TLS 握手被断时通行证本身帮不上忙（Cookie 在 TLS 建好之后才发）——那一路的价值是 WebView 能证明该 URL 可达、且重发时 Node 会重新解析（可能落到不重置的边缘）；若 Node 的 TLS 在每条边缘都被确定性重置，clearance 修不了它，真机结果会直接说明这一点。
- **v0.0.78：窄回落 + 诊断（§10.3 ⑤）。**v0.0.77 真机仍没修好（2026-11-24 00:51 录屏）：玩偶 臻选/电视剧 → 约 5 秒内 `分类失败：HTTP 502`；木偶 → `加载中...` 10 秒以上没结果；重开玩偶聚合 WebHome 后海报骨架屏转 30 秒以上没出海报。**502 是上游在 TLS 隧道后真实返回的状态码**——我们栈造不出来（Node 边车原样透传 `res.statusCode`，host.js:510；`media.rs:994` 的 `_ => 502` 是媒体回环的，与 net.request 无关）；代理 agent 逐项审计无 bug（代理回非 200 时 host.js 直接报 NETWORK 错误，绝不透传状态码）；本机测同一批域名回的是 403 挑战页不是 502，说明不是全站挂掉，是手机那条路（边缘/出口/代理）的问题。**v0.0.77 的 clearance 不可能动 502**——触发条件只有拦截页体或 TLS 被断，502 两样都不是。对照：Arcane 同机同网玩偶分类正常出海报（2026-11-24 01:01 截图），站点没挂；代理模式确认是跟随系统（01:05–01:07 录屏）；唯一结构差是代理——Arcane 的 Node 请求裸 socket 直连走 TUN，HGhub 跟随系统走系统代理（`proxy.rs:221`，真机上读 SCDynamicStoreCopyProxies）。改法（两件，都在 `send_api_hop` 里，host.js 不动）：① **窄回落**——只在跟随系统模式、只在 Api 的 Node 路径；首跳经代理快速失败（< 5 秒）且是上游 502/503/504 或代理握手失败（host.js 文案含“代理”，或 SOCKS 连不上代理时的裸 `connect ECONNREFUSED <代理 host>:<port>`）时，同一 URL/头/方法直连重试一次；成功正常返回；失败返回首跳（代理）的原始错误。自定义代理不动（用户显式路由，不擅自绕过）；直连模式无代理可回落；媒体/下载/代理测试/py 边车不动；不持久化、不改模式、不改保存的设置。**回落本身就是定性仪器**：重试成功坐实代理路径，重试也 502 则与代理无关——本版的前提警告（诊断报告 §六）：整个代理假说建立在“00:51 那次是跟随系统”上（现已确认）；若那次其实是直连，剩下嫌疑是罐子 Cookie 与域名缓存/DNS 时序。② **诊断**——5xx（回落未做或两次都失败）转 `UPSTREAM_ERROR`，消息含原始状态 + 代理描述（模式 + host:port，认证只写有/无，不写值）+ 解析地址 + 两次尝试结果 + Cookie 附带情况（只报名字，不报值），`details.status` 保留供 `fetch()` 的重试判定（`details.status >= 500` 才算失败）；每次 net.request 的代理决议（模式 + host:port + 有/无）写进该插件的 ext.log（requestId `"net"`，不记密码、不记 Cookie 值）。**5xx 契约只改 Node 路径**：wreq 路径仍把 5xx 当数据返回（`node/client.rs` 顶部注释已加作用域说明）。`429/503` 的 Retry-After 冷却保留：503 转错误时仍写，只在重试成功时不写（直连能通就不应再冷却该主机）。**本版不修**：https 代理明文 CONNECT 的潜 bug（host.js:206 不区分 scheme）——诊断报告说单独修，见 §10.3 ⑤末尾。
- **v0.0.79：直连 TLS 被断换地址重试回来了——但不用 pinning（§10.3 ⑥）。**v0.0.78 真机仍没修好（2026-11-24 06:25 截图）：玩偶 臻选在**直连模式**下 `分类失败：NETWORK: Node 边车请求失败：Client network socket disconnected before secure TLS connection was established（解析: 198.20.0.19:443）`——不是 502，v0.0.78 的回落按设计不触发（只管 502/503/504 与代理握手失败）。同一时刻对照：Arcane 能出玩偶分类（用户 06:32 确认），站点没问题；Karing 开着、改跟随系统也是一样的 TLS 被断、同样的解析地址 198.20.0.19:443（用户 06:35 截图）——**两种代理模式失败完全一致、解析地址也相同，代理彻底洗清**。剩下最可能的机制是边缘抽签：Node 18 的 `autoSelectFamily` 默认关，Node 只试解析到的第一个地址，第一个边缘 TLS 被断就直接失败；解析到的地址是轮换的（v0.0.72 时一次解析曾回 24 个 IPv4；v0.0.76/78 两次真机报错里的解析地址各不相同：198.20.19.4:443、198.20.0.19:443），赶上坏边缘就断。改法：`doFetch` 自己拿系统顺序的地址列表逐个试——`dns.lookup(hostname, {all:true, verbatim:true})`（不做 IPv6 优先重排，与 v0.0.74 的结论一致），每次只钉一个地址（`requestOptions` 的第三参），某地址 TCP 已接通、TLS 没完成就被断开（且还没收到响应、请求字节还没发出去）时排除该地址试下一个；成功正常返回；全部试完都失败时返回首个错误并附上已试地址诊断后缀。重试受同一总超时约束；已收到响应、超时、握手已完成都不重试（请求字节已发，重发不安全）；http 没有 TLS，接通后被断不轮换，只在 TCP 没接通时轮换。**不用 pinning**：Rust 侧的预检（`resolve_target_keep_order`）只做校验与诊断，不恢复固定；调用方给了 `pinnedAddrs`（测试用）时轮换用它，生产上 Rust 传空、走 `dns.lookup`。**不用 `autoSelectFamily`**：手动轮换才有“哪个地址失败”的诊断，自动 Happy Eyeballs 会把失败藏起来。**诊断补齐**：先查清了 `（解析: …）` 后缀到底是第一个地址还是全部地址——`resolve_target_keep_order` 收集 `lookup_host` 的**全部**结果（`.collect()` 后去重、校验，不截断到第一个），所以真机报错里只有一个地址就是解析当时只回了一个；`with_resolved_addrs` 从本版起在后缀里带总数（`（解析: …；共 n 个地址）`），直连与跟随系统两种模式都能从报错里直接读出“解析回了几个地址、轮换试了几个”；Node 侧直连 NETWORK 错误也带自己的后缀 `（已试: …；共 n 个地址）`，只回一个地址时直接写 `（已试: …；仅 1 个地址，无处可换）`，Rust 原样透传后再 append 自己的 `（解析: …）`——两份答案（Node 自己解析的与 Rust 预检的）互相印证，DNS 轮换时两者可能不同。
- **v0.0.80：DoH 取全量边缘地址 + 轮换，同时带网络诊断探针（§10.3 ⑦）。**v0.0.79 真机仍没修好（2026-11-24 07:27 截图）：玩偶 臻选仍是 TLS 被断，但 v0.0.79 的诊断生效了——报错 `（解析: 198.20.0.19:443；共 1 个地址）`，系统解析器当时只回 **1 个**地址，轮换无处可换；且解析地址第四次完全相同（06:25/06:33/07:27/07:36 全是 198.20.0.19）。**07:27 报错缺 `（已试: …）` 后缀的原因已查清：那次是跟随系统（用户 07:36 确认），跟随系统走代理路径、轮换不适用，没有该后缀是符合设计；07:36 那次是直连，报错完整写着 `（已试: 198.20.0.19；仅 1 个地址，无处可换）（解析: 198.20.0.19:443；共 1 个地址）`——直连路径的轮换已正确触发、试过唯一的那个地址、诊断如实报告无处可换。结论：v0.0.79 的后缀逻辑没有 bug；核对项只在代码层面确认直连路径一定附后缀，不改行为。** 但公开 DNS 给同一域名回 **5 个** A 记录（Google DoH，本机查过：104.17.255.47、172.66.199.198、104.20.56.211、172.66.194.190、172.66.133.214，另有 5 个 AAAA；www.wogg.net 同一组 5 个），手机解析器只给 1 个、且那个不在公开答案集里（198.20.0.19 虽然不在这 5 个里，但本机 curl 强制 `--resolve` 到它能正常拿到 Cloudflare 拦截页，说明它本身是健康的 Cloudflare 边缘，不是死 IP；手机上 Node 的 TLS 到它被断，是手机那条路/那一刻的问题）。改法（DoH 补充，两件）：① **触发规则**——只在直连路径、只在系统列表全部试完都 TLS 被断（无字节发出的 NETWORK 失败）时，才向 DoH 查同一主机名的 A 记录（首选 `https://dns.google/resolve?name=<host>&type=A`，JSON 解析 `Answer` 里 type 1 的 data；AAAA 有则顺带、无则只做 A）；系统列表永远先试，DoH 只是补充；DoH 查询本身走当次请求的同一代理设置；DoH 查不到/失败/超时时静默退回只用系统列表，不报错、不阻塞；DoH 答案按主机名在 host.js 内存里短缓存 60 秒（只缓存非空答案），同一 burst 里多次请求不重复查。② **同一套放行校验**——DoH 答案过与系统列表同一套校验（host.js 的 `isAllowedIp`，JS 版的 Rust `allowed_ip`：内网/保留地址拒绝、FakeIP 198.18.0.0/15 放行；IPv6 只留全球单播 2000::/3，排除 6to4/Teredo/文档地址；IPv4-mapped 按 IPv4 规则），去重后继续轮换；DoH 也全部失败时返回首个错误。**不改**：系统解析优先顺序、代理路径（代理路径由代理自己解析域名，DoH 轮换只做直连路径，与 v0.0.79 同一边界）、v0.0.78 窄回落与 clearance。**诊断**：报错后缀区分系统与 DoH 两组——`（已试: <系统地址>；DoH: <DoH 地址>；共 n 个地址）`；只回 1 个系统地址且 DoH 也没拿到时仍写 `仅 1 个地址，无处可换`；DoH 查过但没拿到可用地址时写明 `DoH 无结果`。`doh` 字段随成功（`result.doh`）与错误（`firstError._doh`）带回 Rust：`NodeResponse.doh` + `NodeClient::request` 解析（成功）与 `PluginError.details` 的 `{"doh": …}`（错误）；`Network::log_doh_usage` 把每次 net.request 的 DoH 使用情况（主机名、拿到几个地址、成功/失败，不记地址值以外的敏感信息）写进 ext.log（requestId `"net"`）。③ **网络诊断探针**（同版同 tag，用户 07:43 确认要带）：设置页代理测试附近的「网络诊断」按钮（复用现有设置页 UI 与命令通道：`pluginCommand("net.diagnose")` → `invoke("hpp_manage")` → `PluginManager::network_diagnose` → 第一个可用 WebHome 插件的 Network——每个插件有自己的 Network + Logs，代理决议与 Node 边车是全局共享的，只有日志落点不同；走已有的 `hpp_manage` 命令，不新增 Tauri capability）。探针只读、不改请求路径：对玩偶 4 个域名（woggpan.888484.xyz、www.wogg.net、woggpan.xxooo.cf、wogg.xxooo.cf）分别用系统解析（`dns.lookup all:true`）与 DoH（复用本版 DoH 代码）各查一遍，列全部 A/AAAA 地址与数量；对每个去重 IP 用 `tls.connect`（SNI=域名、host=IP，超时 5 秒）单独握手，记 TCP 通不通、TLS 通不通、失败在哪一步/报什么错、耗时、成功时协商的 cipher 与 ALPN；握手通的边缘再发一次 HTTPS GET，记状态码与 body 是否挑战页；出口 IP 经同一条 Node 路取一次公网 IP（`https://www.cloudflare.com/cdn-cgi/trace`，顺带记 colo；不可用再换 `api.ipify.org`）。报告是一份紧凑文本（标题含时间戳与代理模式 + 每个域名一行 DNS 对照 + 每个地址一行握手结果 + 出口 IP），应用内可滚动查看（`<pre className="hg-diagnose-report">`），同时写进 ext.log（requestId `"net"`）；结果不存进任何持久化存储，不动 Cookie 罐；探针自己的 DoH 查询不受“系统列表不够用才查”的限制（探针就是要对照两边）；错误信息压成单行（OpenSSL 的报错尾部常带换行，会把“每地址一行”的报告撑破）。**本机测试的诚实边界**：沙箱拦截一切发往非 loopback 地址的 TCP（同进程、即使本机有监听也一样；直连公网 IP 则黑洞），DoH 给的非保留地址永远到不了本地“好服务器”，所以“系统 1 个坏地址 + DoH 回 5 个、其中 1 个好 → 成功”这个用例在本机显式跳过，成功路径由真机验收验证；可测的部分（DoH 查询、过滤、轮换尝试——全失败用例的后缀证明 6 个地址都被试了、`_doh` 字段、60 秒缓存、DoH 不可达时静默退回、内网地址被滤掉）都已覆盖（见 §10.1）。
- **v0.0.81：失效镜像重定向（§10.3 ⑧）。**v0.0.80 真机探针是决定性的（2026-11-24 09:28:06，直连）：woggpan.888484.xyz 系统 5 个 + DoH 2 个，7 条边缘全部 TCP 通、TLS 通（TLSv1.3 / TLS_AES_256_GCM_SHA384，ALPN 无 ALPN，368–1469ms）、HTTP 200、非挑战页；www.wogg.net 系统 2 个 + DoH 2 个，4 条边缘全部通、HTTP 200；woggpan.xxooo.cf 系统 1 个 [198.20.0.94]、DoH 0 个（查无记录），198.20.0.94 TCP 通、TLS 失败（141ms）；wogg.xxooo.cf 系统 1 个 [198.20.0.21]、DoH 0 个，198.20.0.21 TCP 通、TLS 失败（147ms）；出口 IP 23.237.196.226（colo=LAX）。09:30 分类仍失败：`（已试: 198.20.0.19；DoH 无结果；仅 1 个地址，无处可换）（解析: 198.20.0.19:443；共 1 个地址）`——198.20.0.19 是 .cf 段的地址（.xyz/.net 只解析到 104.x/162.x/172.x/108.x，从不解析到 198.20.0.x），**分类请求打到了已死的 .cf 镜像**；DoH 对 .cf 无能为力（公开 DNS 查无记录，所以报 `DoH 无结果`）；Node 的 TLS、出口、直连路径本身都没问题（.xyz/.net 全通就是证明）。页面自身的域名逻辑（`~/workspace/tmp/wo/app.js` `_getInner`：先试缓存域名、失败清缓存再并发竞速 4 个域名）理论上能绕开死域名，但真机仍失败在 .cf 上（09:30 那次是直连模式，用户 10:28 确认）。改法：**失效镜像重定向**——Node Api 请求路径（`send_api_hop` 入口，choke 点）上，请求目标 host 是 `woggpan.xxooo.cf` 或 `wogg.xxooo.cf` 时，upfront 把 host 改写为 `woggpan.888484.xyz`（只换 host，scheme/端口/路径/查询/片段/请求头都不变；SNI/Host 随 URL 一起变——Node 只从 URL 取 hostname）。重写发生在冷却/许可/预检解析之前，所以 `（解析: …）` 后缀与 DoH/轮换看到的都是改写后的 host；每次改写写一行 ext.log（requestId `"net"`，`net.request 失效镜像重定向：<from> → <to>`）。**范围**：只改 Node Api 路径（net.request、pyfetch、分发更新检查共用同一条路，一并受益）；探针不重写（探针不走 `send_api_hop`，测的就是原始域名）；DoH、轮换、v0.0.78 窄回落、clearance、Cookie 罐不动。**为什么 upfront 而不是“先试原域名、失败再重定向”**：.cf 之死是确定性的（TLS 被断 + 公开 DNS 查无记录），没必要每次浪费一次失败请求；若 .cf 日后复活，重定向也无害（.xyz 同样能出内容）。**注意**：`outgoing()` 已按原 host 附好 Cookie/通行证，.cf 与 .xyz 是同一站，附过去无害；.xyz 不挑战，不需要通行证；`receive_cookies` 按改写后的 host 存——公开分类页不依赖登录态，无影响。
- **v0.0.82：探针诊断准确性升级（§10.3 ⑨）。**动机有两件，都是 2026-11-24 上午真机上暴露的。① 10:54 那次分类失败（截图 IMG_6134.png）与 v0.0.80 的 09:30 失败一字不差，第一反应是“跑的还是旧版”，但探针报告本身没有版本号、改写状态也无从查证——“哪个版本跑出来的”这个问题报告自己回答不了，只能靠用户另发关于页截图（11:01 才确认 0.0.81 已装上）；而 11:08 用户在已确认的 0.0.81 上重测，分类仍报同一错误（`已试: 198.20.0.19；DoH 无结果；仅 1 个地址`），“旧版”推断被推翻。只读诊断已完成（11:20）：**Rust 侧改写不可能被绕过**——全链已追完（fm.req → dispatch_with_clearance → webhome::dispatch → network.fetch → fetch_once → buffered → shared_hop → read_hop → send_api_hop 入口改写 → node_hop_with_fallback → host.js doFetch；已排除旧版二进制、wreq 路径、clearance 绕过、direct_retry/重定向绕过、fetch 重试/缓存）；两份后缀定案——Rust 的 `（解析: 198.20.0.19:443；共 1 个地址）` 是对改写后目标做的解析，若原 host 是两个 .cf 之一必是 .xyz 地址，Node 的 `（已试: 198.20.0.19；DoH 无结果）` 说明 Node 收到的 hostname 的 DoH 为 0、不是 .xyz；**所以 11:08 那次请求的 host 根本不在映射表里**。最可能机制：页面实际用的域名被改掉了——监控同步（`syncDomainsFromMonitor`，从 site.920410.xyz 等拉 monitor_data.json，重算域名列表并置缓存，每次启动跑一次、缓存 1 小时、过期仍用旧快照兜底）或用户手动覆盖（`SITE_DOMAIN_OVERRIDES`，localStorage wo_site_domains，可整体替换域名列表；单域名时走 tryOne 直通、无竞速）；子形态是 IP 字面量 URL（如 `https://198.20.0.19…`，解析恒为自身单地址、DoH 查 IP 无记录；198.20.0.19 从 06:25 到 11:08 七次完全相同更符合写死 IP 而非轮换 DNS）或第三个未列入映射表的 .cf 镜像（现取的监控数据里玩偶 best_url 是 .xyz、urls 只有 .xyz/.net，手机 11:08 用的快照必然不同；且手机页面有“臻选”分类而本地 app.js 副本里没有，页面构建已有出入、域名列表不可默认一致）。**本版不抢修它**，只给探针加一把能当场验证改写的尺子。**诚实边界**：探针的真实分类检查测的是已知的两个 .cf/.xyz URL；若页面实际打的是 IP 字面量或第三个镜像，探针检查会通过、页面仍失败——两者要对照看，探针不能代替页面实际请求的 URL。② 10:56 探针视频：用户在对话框里点了自定义（地址已填）但没点保存代理就点网络诊断，探针仍按已保存的直连跑、报告头仍写直连——报告与界面选中的模式不一致。改法（四件，都只动探针，不改任何真实请求的行为）：① **探针用对话框当前选中的模式跑**——设置页把当前选中的模式（跟随系统 / 自定义+地址 / 直连）随 `net.diagnose` 传下去，`Network::diagnose` 用该模式做全部检查（DNS 对照、TLS 握手、GET、出口 IP），报告头写明实际使用的模式；只改变探针本次运行用的模式，不改已保存的设置、不改任何真实请求的代理行为。实现是每调用一次的 `proxy_override`（`buffered → shared_hop → read_hop → send_api_hop → node_hop_with_fallback` 逐层传，`node_hop_with_fallback` 用它或回落到 `proxy_decision()`；去重键含 override，不同模式不共享在途结果；全局状态不动——全局换设置会影响并发的真实请求）。**诚实边界**：override 只进 Node 路径；Node 不可用时回落的 wreq 路径按快照代理跑，用的是已保存的设置（iOS 上 Node 边车在时走 Node 路，wreq 只在 Node 挂了时才出现）。② **真实分类请求检查**——探针在域名对照后，用正常 net.request 路径（`buffered()`，即 `fetch()` 的核心：改写、冷却、许可、限速、Cookie、跟随重定向、去重都在）发两次真实分类请求：① `https://woggpan.xxooo.cf/vodshow/1--------1---.html`（检验改写是否生效），② `https://woggpan.888484.xyz/vodshow/1--------1---.html` 作对照。每条报告：请求 URL、改写后实际 host（是否改写、改成什么）、解析地址、HTTP 状态、是否含列表内容标记（`module-item` 等，与页面 `_getInner` 判“列表页无有效内容”同一组标记）、失败时的完整错误后缀。两次请求走正常限流/冷却、不绕过；超时 16 秒（页面 `req()` 的常规预算）；顺序发。③ **报告头加版本号与改写状态**——报告顶部写 `版本：<应用版本>（失效镜像改写：<映射表或“无”>）`；版本号是前端 `APP_VERSION`（vite define，取自 CI 从 tag 盖章的 `package.json`，与关于页同一值；不用 Rust 的 `CARGO_PKG_VERSION`——app/hpp crate 的版本是占位符 `0.0.4`/`0.0.1`，不是应用版本）。④ **报告顶部加一行结论**——Node `/diagnose` 除报告文本外再回域名小结（每域名实测计数：系统几条、DoH 几条、DoH 状态、去重边缘数、TLS 通几条、TLS 断几条、HTTP 200 几条），Rust 用纯函数从小结 + 两次真实分类请求的结果拼一句话结论（例：`.xyz/.net 全通；.cf TLS 全被断（DoH 无记录）；真实分类请求经改写后成功`）；结论只陈述本次测到的事实，失败原因只按错误消息自身的措辞分类（`before secure TLS` → TLS 被断、超时 → 超时、冷却/RATE_LIMITED → 被限流/冷却、DNS → DNS 失败、502/503/504 → 上游错误），完整错误文本仍留在检查行里。**除两次真实分类请求外不新增网络请求**；探针的域名对照部分仍不重写（测的就是原始域名），真实分类请求检查是唯一走改写的部分——它就是要测改写。最坏总耗时：Node 60 秒 + 两次各 16 秒，设置页文案写“约半分钟”。
- **v0.0.83：设置页一键清掉 WebHome 全部数据（§10.3 ⑩）。**动机：v0.0.82 真机探针（2026-11-24 11:40 直连、11:41 跟随系统）证明改写在真实路径上生效——.cf 经改写→.xyz 后 HTTP 200 含 module-item、.xyz 对照同样 200；但 11:46–11:47 分类重测仍失败、同一 198.20.0.19 错误，机制①彻底坐实（页面分类请求的 host 不在映射表里）。11:56–11:57 视频又暴露：用户点的监控同步约 2 秒就回来、列表一字未变——按页面代码（app.js:1166–1185）成功一定会把合并后的最新监控列表写回输入框，所以这次同步失败了（fetchMonitorData 走 fm.req 即 Node 边车，三地址各 12 秒超时，2 秒回来说明是快速连接失败而非超时；且对话框清掉了 1 小时缓存、无过期快照可兜底）；各站域名列表都冻在旧快照上、无法刷新，这与玩偶一直打死掉的 .cf 地址吻合（现取的监控数据里玩偶 best_url=.xyz、urls 只有 .xyz/.net；木偶当前监控是 123.666291.xyz 等——与手机上的旧列表 muou.site/muou.asia 对不上，证实手机快照是旧的）。改法（设置页代理测试旁的「清理 WebHome 全部数据」按钮，点后先弹确认框）：四步按顺序——① 在 webhome 窗口里跑 JS 删三个 localStorage 键（wo_domain_cache、wo_monitor_snapshot、wo_site_domains），回调取回每键清理前是否存在；② `WebviewWindow::clear_all_browsing_data` 全清 WebView 浏览数据（页面缓存、全部 localStorage/sessionStorage、Cookie、IndexedDB、Service Worker 缓存等）；③ 清绑定插件的 Rust 请求 Cookie 罐（内存 + 落盘 cookies.json，`Network::clear_cookie_jar` + `PluginManager::clear_cookie_jar`，只动罐子本身）；④ `location.reload()` 重载 webhome 窗口（页面把域名列表存在内存变量里，不重载没用；hide-and-reuse 下重开同源只 show() 不重载）。窗口不存在时命令报 `WEBHOME_NOT_OPEN`，UI 提示“请先打开一次 WebHome（任意站点），再回来清理”，不新建隐藏窗口。**注意**：② 是 fire-and-forget——wry 的 `clear_all_browsing_data` 调 `WKWebsiteDataStore.removeData` 时 completion handler 是空的，调完立即返回、不等删完；重载可能跑在删除完成之前，但① 的三个键是 JS 同步删掉并经回调确认的，所以重载后页面读到的域名列表不受影响，其余浏览数据的清除在后台完成。② 清的是 WebView 的共享数据存储（Tauri 文档原话 "clears the WKWebsiteDataStore of this webview"；未配自定义 `data_store_identifier` 的窗口共享默认存储），主窗口的本地偏好也可能一并恢复默认。③ 跟分类失败没关系，是为了“全部”才算进去。**不碰**：App 设置（代理模式、自定义代理地址等）、导入的插件及插件文件、下载好的视频；清完页面里的登录态会掉（Cookie 清了），结果页提醒用户重登。**诚实边界**：清完只是把页面域名数据回到起点——下次打开 WebHome 时页面会重新从监控同步域名；若监控同步在真机上仍失败（11:56 视频里就是失败的），页面可能又拉不到新列表、仍用默认域名列表（玩偶默认 4 个域名里仍有已死的 .cf，`_getInner` 会先试缓存域名再竞速全部域名，死域名只会拖慢、不会单独决定成败——除非列表里只剩死域名）；同步本身仍失败是已知问题，不在本版修。真正的 WKWebsiteDataStore 删除在 Linux 上测不了（wry 的 iOS 分支只在 Apple 平台编译），本机只测到“调了清浏览数据的那一步”——真机验收见 §10.2。
- **v0.0.84：设置页一键清掉全 App 运行时缓存（§10.3 ⑪）。**动机：v0.0.83 只清 WebHome 四步（页面三键 + WebView 浏览数据 + 绑定插件的罐子 + 重载），用户 2026-11-24 12:18 选定“全部清掉”——各站域名列表冻在旧快照上，单清页面数据不够，要把所有可能藏旧数据的运行时缓存一次清干净；12:34 定红线“插件、py源、HTML源都不能删除”，12:38 补“日志也可以删除”，随后收紧为只限红苹果影视自己的日志（不碰系统日志、其他 App 的日志、App 数据目录之外的日志文件）。改法（设置页代理测试旁的按钮改名「清理全部缓存」，点后先弹确认框）：顺序是 ① JS 删三个 localStorage 键（v0.0.83 不动）→ ② `clear_all_browsing_data` 全清 WebView 浏览数据（v0.0.83 不动，仍是 fire-and-forget）→ ③ 中间步骤：全部插件的 Rust 请求 Cookie 罐（内存 + 各自的 cookies.json；v0.0.83 只清绑定插件，`PluginManager::clear_cookie_jar` 是单插件的，本版新增 `clear_all_runtime_caches` 遍历全部已装插件——已停用的也在内，network/store 在 installed() 里对全部索引条目都建好了）、Network 响应缓存（每插件内存里的 `CachedResponse`，新增 `Network::clear_response_cache`）、插件 KV 存储里的 `webhome:` 前缀键（`webhome.rs` 的 `cache()` 逻辑抽成 `clear_webhome_prefix_keys` 共用；只清该前缀，插件自身的键不动，不删存储文件）、全部会话的媒体预加载缓存（`media/preload.rs` 的 `Cache` 新增 `clear()`，先取消在途预热再丢条目）、插件目录下的 `__pycache__` 与 `*.pyc`（沿插件根目录递归删，只删字节码，绝不删 `*.py`；符号链接不跟进）、Python 边车已加载蜘蛛的 `_RUNTIME_CACHE`（`base/spider.py` 的 `BaseSpider.clearCache` 只清本蜘蛛命名空间，`spider_runner.py` 的绑定元组里加上它；Rust 侧 `SidecarClient` 新增 `list` + `clear_all_spider_caches`，逐个调 `/call clearCache`；函数式蜘蛛没有运行时缓存，调不到方法时跳过、不计入也不警告；边车没在运行时不启动，只读地址文件 + 健康检查（App 侧 `python::running_client`），`py_running` 为 false——`_RUNTIME_CACHE` 是边车进程里的内存缓存，边车没跑就没有已加载的蜘蛛，边车下次启动时本就是空的）、插件导入暂存 `plugin-imports` 里的文件（已核实是纯暂存：选择器返回的文件先拷进去，预览与安装读的都是这份拷贝，另有 7 天自动清理；只删文件，目录本身保留）→ ④ `location.reload()` 重载 webhome 窗口 → ⑤ 最后清日志：全部插件的 ext.log 内存缓冲（`native/logs.rs` 的 `LogBuffer` 新增 `clear()`，上限 2000 条的 `VecDeque`，不落盘；新增 `PluginManager::clear_all_logs`）+ App 自己的 `debug.log`（`<app_data_dir>/debug.log`，`logging.rs` 的 `append_debug` 写的调试日志；命令返回后删文件，`append_debug` 下次写时自动重建）。日志放最后：本次清理动作自身的日志行也会随之消失，是预期的；清完后日志从空开始，下次出问题时新日志更干净、更好定位。**日志盘点（只限 App 自身）**：插件 ext.log 是内存缓冲、py 边车与 Node 边车只有 stdout（main.py/spider_server.py/host.js 均无文件日志配置）、App 自身只有 debug.log 这一个落盘日志文件；系统日志、其他 App 的日志、App 数据目录之外的日志文件一律不碰。**行为变化**：中间步骤改成按类别尽力清理、某一类失败记入 `warnings` 并继续清下一类、不中断重载（v0.0.83 是罐子失败即中断、不重载）；窗口步骤（删键、清浏览数据、重载）失败仍 Fail-fast、不继续往下走、不重载、日志不清（留着查错）。**不碰**：App 设置、导入的插件及插件文件、插件存储里非 `webhome:` 的键、下载好的视频、插件源文件（*.py、*.js、manifest）、HTML 源文件（localPage 的 *.html）；清完页面里的登录态会掉（Cookie 清了），结果页提醒用户重登。Node DoH 缓存（host.js 的 `dohCache`）60 秒自过期，不专门清；host.js 不动。**诚实边界**：清完只是把全部运行时缓存回到起点——下次打开 WebHome 时页面会重新从监控同步域名；若监控同步在真机上仍失败（11:56 视频里就是失败的），页面可能又拉不到新列表、仍用默认域名列表（玩偶默认 4 个域名里仍有已死的 .cf，`_getInner` 会先试缓存域名再竞速全部域名，死域名只会拖慢、不会单独决定成败——除非列表里只剩死域名）；同步本身仍失败是已知问题，不在本版修。真正的 WKWebsiteDataStore 删除在 Linux 上测不了（同 v0.0.83），本机只测到“调了清浏览数据的那一步”——真机验收见 §10.2。
- **v0.0.85：清理按钮的 ACL 权限漏了（§10.3 ⑫）。**v0.0.84 真机（2026-11-24 13:48 视频 + 13:47 截图 IMG_6143.png）：设置页点「清理全部缓存」，对话框蓝字报 `Command clear_webhome_data not allowed by ACL`——清理一次都没跑成。根因：`clear_webhome_data` 命令在 `src-tauri/src/webhome.rs` 里定义、`lib.rs` 的 `invoke_handler` 里注册、前端 `src/plugins/webhome.ts` 经 `invoke` 调用，但 `src-tauri/capabilities/default.json` 里没有 `allow-clear-webhome-data`，`src-tauri/permissions/app.toml` 里也没有它的权限定义——命令加了、白名单没加，是 v0.0.52 同类问题的重演。`cargo check` 与 `tsc` 都验不出来：权限白名单只在运行时由 Tauri 核对，编译期与类型检查都看不到。改法：按 `allow-open-webhome` 的既有格式在 app.toml 里加权限定义，并在 default.json 的 permissions 里加 `"allow-clear-webhome-data"`；只加主窗口——按钮在主窗口的设置对话框里，webhome 窗口不调这个命令，webhome.json 不动。审计：主窗口前端 31 个 invoke 调用点逐个核对（pluginCommand 走的 `hpp_manage` 是一次 invoke，不展开插件命令；`webhome_call`/`webhome_close` 由 webhome 窗口的远端页面经 fm 桥调、不在 src/ 里，由 webhome.json 覆盖），除 `clear_webhome_data` 外无其他遗漏。回归检查：新增 `scripts/check-acl.mjs`（`pnpm check:acl`），扫 src/ 全部 invoke 字面量、映射成权限名（下划线变连字符、加 allow- 前缀；`plugin:app|exit` 映射成 `core:app:allow-exit`），断言每个权限既在 default.json 里、又在 app.toml 里有定义（core:* 是 Tauri 核心插件自带权限，只核对 default.json）；负向验证：临时文件里去掉 `allow-clear-webhome-data` 后检查失败并点名缺失的权限（exit 1），加回去后通过（exit 0）。**诚实边界**：清理从没成功跑过，所以 v0.0.84 的“清缓存有没有用”这次测试没有结论；分类的 HTTP 500（源站回 500、直连重试也 500）是另一个故障，不在本版修，等 v0.0.85 修好、用户清理后再测。另按用户 2026-11-24 13:55 要求，清理入口从代理对话框移出——设置页独立「缓存清理」区块（新组件 `src/plugins/CacheClearSettings.tsx`，流程与文案不变：入口 → 确认框 → 调 `clear_webhome_data` → 结果页），WebHome 没打开过仍报 `WEBHOME_NOT_OPEN`；`ProxySettings.tsx` 里清理相关代码已全部删掉。
- **v0.0.86：WebView→引擎 Cookie 桥（§10.3 ⑬）。**动机：v0.0.85 真机（2026-11-24 14:36–14:37 视频）：清理跑成了（无 ACL 报错），但分类仍失败——页面报「分类失败：响应被拦截」，WebView 里直接出现了源站的「最新@网址」验证页（「当前访问人数过多，请验证密码」）。已核实这页不在页面代码里，是源站自己下发的；连 WebView（真浏览器）也被下发同一页。随后对照定性（用户 14:48–14:54）：同一台手机、同一时间段，Arcane 浏览正常、玩偶「电影」分类完整出内容（14:49 截图 IMG_6150.jpg，海报+标题全有）；HGhub 切直连（运营商 IP）也还是失败——出口 IP 彻底洗清（跟随系统美国节点与直连都被拦、Arcane 同机同时正常），差别只剩请求本身。头号假说仍是 Cookie，三点全合：① v0.0.82 探针（11:40）走正常 net.request 路径、Rust 罐里有 Cookie 时，真实分类请求 HTTP 200 带内容；② v0.0.85 全清后（罐空了）分类被下发验证页；③ Arcane 有 WebView→引擎 Cookie 桥，HGhub 没有——HGhub 只有 clearance 流程读 cf_clearance。页面在 fm.req 里给两家设的头一样（UA Mozilla/5.0 + Referer），两家都用 nodejs-mobile v18.20.4（同一套 TLS），页面代码也同一份，所以请求层面剩下的差别就是 Cookie 头。反例已记：13:47 那次 500 时 Cookie server_session_4409513a 是附带了的——Cookie 没拦住 500；但当前失败是验证页不是 500，桥针对的是验证页，不解 500。改法（最小接入点：app 侧 `dispatch_with_clearance`，不重构链路）：发起 WebHome 引擎请求（`req`/`net.request`）前，读 WebHome 窗口里目标 URL 的 Cookie（复用 clearance.rs 的 `cookies_for_url`，阻塞调用、走 spawn_blocking），合并进该插件的 Rust 普通 Cookie 罐（hpp `Network::merge_webview_cookies`，与引擎收到的 Set-Cookie 同等待遇：落盘 cookies.json、按域名/路径/Secure/过期匹配；`__cf_bm` 不收、空名/空值/空域名/已过期不收；同名同域同路径覆盖旧值；落盘失败不报错——内存已更新、当次请求照常用）；方向只要 WebView→引擎，不回写 WebView。clearance 重发前再桥一次（clearance 导航可能在 WebView 里落了新的站点 Cookie）。窗口不存在、URL 非法、读不到 Cookie 时静默回落到现在的纯罐行为，不失败请求。每次桥（窗口存在 + URL 合法）都在 ext.log 里记一行 `net.request Cookie 桥接：<n> 个（名字：…）`（requestId `"net"`，只报名字、不报值；0 个也记——桥跑了、窗口里没有该 URL 的 Cookie，与桥没跑是两回事，真机验收靠这行区分）。**不碰**：路由/代理模式、页面代码、clearance 流程、罐子以外的存储；不新增 Tauri 命令、不新增 invoke 调用点（capabilities 不动，check-acl 仍是 31 个调用点）。**诚实边界**：桥的前提是 WebView 里真有目标 URL 的 Cookie——首页在 WebView 里加载时站点落的会话 Cookie 才能被桥过去；若 WebView 里也没有（例如站点只在引擎请求里发会话 Cookie、首页不落），桥会记 `0 个`、分类仍被拦，那时看 ext.log 的桥接行就知道是“没东西可桥”，下一跳就不是 Cookie 而是请求本身的其他差别。本机测不了真机 WebView 的 Cookie 存储（Linux 上 wry 的 iOS 分支不编译），本机只测合并与日志逻辑——真机验收见 §10.2。
- **v0.0.87：加宽 Cookie 桥 + 引擎响应日志（§10.3 ⑭）。**v0.0.86 真机（2026-11-24 15:26 视频，插件 ext.log）：桥跑了，但每次都是 `Cookie 桥接：0 个`；全部请求 `Cookie：未附带`（罐空）；代理直连；目标包括 ipapi.co/ip.sb（出口检查）、pan-site-monitor.vercel.app + site.920410.xyz（监控同步在尝试）、玩偶 4 个域名全 race。分类还是打不开。**v0.0.86 的杠杆错了**：WebView 只加载 homePage（https://wo.vivas.cc.cd），从不访问 wogg* 域名，所以它没有 wogg* 的 Cookie 可桥——按目标 URL 读 Cookie 永远是 0 个。另两点已核实：罐子会存响应里的 Set-Cookie（`receive_cookies`），但整个会话里没有任何请求附带 Cookie，说明源站当前响应根本不下发可存的会话 Cookie；且 ext.log 只有请求行、没有响应日志——源站到底回了什么（验证页？500？空？）目前看不见。剩下唯一的 Cookie 来源是 WebView 里首页的 Cookie——如果验证页加载时落了会话 Cookie，而源站只认 Cookie 名不认它来自哪个域（服务器只看到 Cookie 头里的名和值），那它就是能用的钥匙。改法（两件，同一版）：① **加宽桥**——每次 WebHome 引擎请求前，除了读目标 URL 的 Cookie，再读 WebHome 窗口首页 URL（从插件 meta 取 `webhome_page_url`，和窗口地址同一来源）的 Cookie，把首页的 Cookie 也附到当次引擎请求上（即使域名不同）；只附到当次请求，**不**把它以错误域名存进罐子、不落盘。实现是 hpp 的 `Network::set_home_cookies`（按请求目标 origin 记“当次附加”，每次桥覆盖同一 origin 的旧条目，上限 32 个 origin；`__cf_bm`/空名/空值/空域名/已过期不收，与 `merge_` 同一条规矩），`outgoing()` 按目标 origin 取、按名去重后并进 Cookie 头（罐子与通行证的同名 Cookie 优先；Secure 的只给 https 目标；`fetch` 的重试循环里同一请求再调 `outgoing()` 时重试也带上同一份）；app 侧沿用 `dispatch_with_clearance` 的两个桥点（首次与 clearance 重发前），首页与目标同 origin 时跳过（目标桥已处理）。每条加宽桥记一行 ext.log（`net.request Cookie 桥接（首页 <来源域>）：<n> 个（名字：…）`，只报名字与域名、不报值；0 个也记——WebView 里首页也没有 Cookie 时这本身就是答案）。窗口不存在、首页地址读不出来时静默回落，不失败请求。② **响应日志**——每次 Node 引擎请求的响应（首跳与直连重试那跳、5xx 那跳都记）在 ext.log 记一行：HTTP 状态 + 收到的 Set-Cookie 名字（有就报名字、无就报“无”）+ 响应体标记（是否含 module-item、是否含「验证密码/访问人数过多」验证页文本、是否含 just-a-moment）。只记布尔标记，绝不记完整响应体；只报 Cookie 名字，不报值。实现是 `node_hop_with_fallback` 里每次拿到 NodeResponse 就记（`Network::log_response`，标记是 network.rs 的纯函数 `response_markers`）。**不碰**：路由/代理模式、页面代码、clearance 流程、罐子以外的存储；不新增 Tauri 命令、不新增 invoke 调用点（capabilities 不动，check-acl 仍是 31 个调用点）。**诚实边界**：加宽桥的前提是 WebView 里首页真有 Cookie——若首页也没落会话 Cookie，桥接行会记 `0 个`，那时看响应日志就知道源站到底回了什么（验证页/500/空体），下一跳按响应定；本机测不了真机 WebView 的 Cookie 存储与真机响应，本机只测并入与日志逻辑——真机验收见 §10.2。
- `servername`（SNI）与 `Host` 仍取目标 hostname（Node 用自定义 `lookup` 时的默认行为，实现时必须显式确认并加测试断言；不要让 Node 把 IP 当成 SNI 发出去）。
- 与 wreq 路径的差异：wreq 路径靠 60 秒池重建重新解析 DNS；Node 路径每跳都重新解析（走系统解析器缓存，开销可接受，且更及时）。行为差异如实记录，不掩饰。

## 8. 错误与降级

- **iOS 上不静默回落到 wreq。**Node 引擎起不来、边车连不上、/request 返回 500 时，请求直接报明错（`INTERNAL` "Node 引擎不可用/Node 边车连接失败"，附原因），不要悄悄改走 wreq——v0.0.70 是假设检验，静默回落会让真机结果无法判断（到底是 Node 过了还是 wreq 过了）。非 iOS 目标本来就没有 Node，走 wreq（今天的行为）。
- **重探重试（照抄 v0.0.69 的 `call_with_refresh`）：**Node 边车连不上（TCP 没连上、请求没进 Node）时，重探 `bootstrap.ensure_started()`（重读 info 文件 + `/health`，边车重绑换了端口时拿到新地址）换新客户端重试一次；重试成功不记故障。方法没进 Node 就不会重放已执行的请求。超时不触发重探。
- **Node 引擎 fatal：**整个 App 随之崩溃（与 Python 边车同款，无隔离）。host.js 顶层捕获异常、永不 `process.exit()`，把 fatal 概率降到最低；一旦发生，用户只能重启应用（bootstrap 的 `started` 标志保证不尝试二次启动——Node 不支持二次启动，尝试只会更糟）。
- **降级路径：**Node 不可用时（非 iOS、或 iOS 上引擎启动失败且用户急需用），唯一的降级是回退到 v0.0.69 的 wreq 行为——那需要改回传输选择，不是运行时开关。本版不做运行时开关；真机验收失败时按 §10 的下一跳处理。

## 9. 构建集成

### 9.1 新增 `scripts/sync-ios-node.mjs`（照抄 `sync-ios-python.mjs` 的两段式）

- **准备（默认模式，xcodebuild 之前）：**
  - 下载 pinned 的 `nodejs-mobile-v18.20.4-ios.zip`，校验大小（51,492,431）与 sha256（§3.1），解到 `src-tauri/gen/apple/NodeMobile.xcframework`（已是同一版本时跳过；xcframework 不进 git，gen/apple 本来就是 CI 现生成的）。
  - 把 Node 边车源码暂存到 `gen/apple/node-app/`（`host.js`；以后有需要再加文件）。
  - `node-app/` 里还有 `package.json`（`{"type": "commonjs"}`，定死 host.js 的 CommonJS 身份，见 §4.2）与 `NOTICE.md`（许可证，见 §9.4）。
  - 找不到 gen/apple 时跳过、不报错（与其他 `sync-ios-*.mjs` 同一约定）。
- **装包（`--install-app <APP 路径>`，xcodebuild 之后、打 IPA 之前）：**
  - 把 `NodeMobile.framework`（device slice）拷进 `.app/Frameworks/`。
  - 把暂存的 `node-app/` 拷进 `.app/node-app/`。
  - 不做 codesign（本次构建未签名，签名由分发方在安装时做；若将来走签名构建，这里要给 framework 补 codesign）。

### 9.2 `scripts/build-ios-unsigned.sh`

- 准备段加 `node scripts/sync-ios-node.mjs`（Python 那行之后）。
- **v0.0.71：xcodebuild 不再链接 NodeMobile**——`FRAMEWORK_SEARCH_PATHS` 里的 `$(PROJECT_DIR)/NodeMobile.xcframework/ios-arm64` 与 `OTHER_LDFLAGS` 里的 `-framework NodeMobile` 都已删掉（启动时 dyld 不加载 framework，见 §4.1）；`-framework Python -framework SystemConfiguration` 保留。
- 装包段加 `node scripts/sync-ios-node.mjs --install-app "$APP"`（Python 那行之后、打 IPA 之前）。

### 9.3 `src-tauri/build.rs`

- **v0.0.71 已删 `link_node_framework()`**（调用与函数都删了）：Rust 不再链接期找 NodeMobile——`src/node.rs` 不再用 `#[link(name = "NodeMobile", kind = "framework")]`，改 `dlopen` 运行时加载，不需要 `cargo:rustc-link-search=framework=…`。`link_python_framework()` 与 `link_system_configuration()` 不动。
- **不需要 Swift 桥**（§3.2 已纠正），`link_ios_native_plugins()` 不动。

### 9.4 打包清单

- `.app/Frameworks/NodeMobile.framework`（device slice）。
- `.app/node-app/host.js`、`package.json`、`NOTICE.md`。
- Node LICENSE 全文 + 第三方 NOTICE（§3.3）：随源码带一份（`src-tauri/node-sidecar/NOTICE.md` 或等效位置），并在实现时确认是否需要进 `.app`（以合规要求为准；至少源码侧必须有）。
- xcframework 本身不进 git（与 Python.xcframework 同款）。

## 10. 验证

### 10.1 本机（Linux）能验证的

- `cargo check --target aarch64-linux-android`（AGENTS.md §7.6 的环境配方；`cargo check --target aarch64-apple-ios` 本地无 xcrun，仍以 CI 为准）。
- hpp lib 测试（`src-tauri/crates/hpp` 内跑；`app` crate 的测试宿主编不过，沿用老办法）：现有测试全过（除 1 个既存失败 `api_limits_cover_bodies…` fixture 超时，非回归）；新增：
  - NodeClient 的请求/响应映射测试（用 axum 搭 mock 边车，像 Python 集成测试那样，断言 status/headers/bodyBase64 的往返与错误映射）。
  - `effective_proxy_url` 的三档模式测试（Off/Custom/System，含 HGHUB_PROXY 优先）。
  - Node 传输选择的测试（Queue::Api 走 Node、Queue::Media 走 wreq；非 iOS 走 wreq）。
  - 重探重试测试（边车换端口后调用重探到新地址重试成功，照抄 v0.0.69 的 `python_sidecar_restart_on_new_port_retries_call`）。
  - Cookie 罐的 `__cf_bm` 测试（v0.0.75：挑战页发的不存、加载时残留的清掉并回写）。
  - WebView clearance 测试（v0.0.77）：`looks_like_challenge` 的标记匹配；`cf_clearance` 只存内存、按主机匹配后并进 Node 请求的 Cookie 头；过期 / `__cf_bm` / `credentials: omit` 时不带；已有 Cookie 头时追加、不重复带。host.js 本版没动，13/13 仍过、无新增用例。
  - 窄回落与诊断测试（v0.0.78）：mock Node 按有没有 proxyUrl 分流——代理路 502 → 直连重试一次成功（只调两次，首跳经代理、第二跳直连）；代理 502 + 直连 502 → 返回首跳原始错误，转 `UPSTREAM_ERROR`，消息含状态 + 代理描述 + 解析地址 + 两次结果 + Cookie 名字（`details.status` 保留）；503 同款；自定义代理永不回落、直连模式无回落、跟随系统无代理时无回落（各只调一次）；代理握手失败（`代理拒绝 CONNECT（HTTP 502）`、SOCKS 裸 `ECONNREFUSED` + 代理地址）→ 直连重试；直连重试也失败 → 返回首跳原始错误；Cookie 只报名字不报值；每次 net.request 的代理决议写进 ext.log（requestId `"net"`）；`429/503` 的 Retry-After 冷却在 503 转错误时仍写。纯函数单测：代理描述（认证只写有/无、IPv6 加括号、无效地址）、回落资格、握手错误识别（host.js 全部代理文案 + 隧道后失败不识别）、Cookie 名字、5xx 诊断错误、未回落原因、错误消息截断。host.js 本版没动，13/13 仍过、无新增用例。
  - 直连轮换的诊断透传测试（v0.0.79）：mock Node 的 direct_error 带 `（已试: 127.0.0.2；仅 1 个地址，无处可换）` 后缀，最终报错里 Node 后缀原样在前、Rust 的 `（解析: 127.0.0.1:…；共 1 个地址）` 后缀 append 在后。
  - DoH 补充与探针测试（v0.0.80）：mock Node 的 direct_error 带 `（已试: 198.20.0.19；DoH: 104.17.255.47, 172.66.199.198；共 3 个地址）` 后缀，最终报错里 Node 后缀原样在前、Rust 的解析后缀 append 在后；`network_diagnose` 单测（mock `/diagnose` + `set_logs`，断言报告返回、请求体里是 4 个玩偶域名与代理描述 `直连`、ext.log 里有一条 requestId 为 `"net"` 的日志含完整报告；用 `Mode::Off` 的 `fallback_network`，代理描述确定是 `直连`）；NodeClient 单测（`/request` 200 的 `doh` 字段进 `NodeResponse.doh`、500 的进错误 `details`；`/diagnose` 200 回报告文本、500 转 `INTERNAL` 错误）。hpp 97 过 + 1 既存失败（`api_limits_cover_bodies…` fixture 超时，自 v0.0.41 起每次都有，已在干净 v0.0.78 树上复现确认、不是回归，不修）。
- **host.js 的测试（本机可跑）：本机有 Node v24.22.1**，host.js 只用核心模块，可直接用系统 Node 跑集成测试：对本地 HTTP/HTTPS 服务器发请求（断言只发了页面给的头、无 sec-ch-ua、HTTP/1.1）；对本地 mock 代理发 CONNECT 与 SOCKS5/SOCKS4 握手（断言握手字节与隧道建立）；自定义 `lookup` 返回固定 IP 时断言 SNI/Host 仍是 hostname；**lookup 契约测试**（`all:true` 回数组、`all:false` 回单个地址字符串 + family，用真机上出现过的 pinned 列表形态：24 个 IPv4 与单个 IPv4/IPv6）；**v0.0.76：`requestOptions` 契约测试**（无 pinned 时不用自定义 lookup、不设 `autoSelectFamily`；有 pinned 时 lookup 是函数、仍不设 `autoSelectFamily`）、**无 pinned 地址时走 Node 默认 lookup**（`localhost` 能通）、**TLS 握手被断时只试一次、不重试**（坏端点 `clientHellos === 1`）。v0.0.75 的两个重试测试（TLS 回落、全部失败带 `pinned:`）已随 pinning 撤掉而删除，诊断改由 Rust 层做（`（解析: …）`，见上）。**v0.0.79：轮换回来了（不用 pinning）**——TLS 握手被断时换下一个地址重试（`localhost` 坏端点 + 好端点 → 200）、换下一个 pinned 地址重试、全部地址都断时报错带已试地址与总数（`共 2 个地址`）、超时不轮换、已收到响应后不轮换、单个地址时只试一次且报错带“仅 1 个地址，无处可换”；`resolveAddrs` 单元测试（系统顺序、不重排、解析失败回空列表）；`requestOptions` 第三参 `addrs` 的契约（单地址 lookup、仍不设 `autoSelectFamily`）。host.js 19/19。**v0.0.80：DoH 补充 + 探针**——`isAllowedIp` 单元测试（与 Rust `allowed_ip` 同款放行校验：内网/保留拒绝、FakeIP 198.18/15 放行、IPv6 只留全球单播、IPv4-mapped 按 IPv4 规则）；DoH 全部失败时报错列出系统与 DoH 两组地址及总数（进程内调 `doFetch`，后缀 `（已试: 127.0.0.2；DoH: …；共 6 个地址）`，`_doh` 字段）；同一主机 60 秒内第二次请求不重复查 DoH（两次失败请求，`hits.a`/`hits.aaaa` 都是 1）；DoH 端点不可达时静默退回系统错误（后缀 `DoH 无结果`，`doh` 字段 `查询失败`）；DoH 回内网地址时被滤掉不试；`probeTls` 单元测试（TCP 不通时报 refused）；探针集成测试（mock DoH + 本地 TLS 服务器一个接受、一个 RST + mock 出口 IP 端点：报告里出现两组 DNS 地址、握手结果区分、出口 IP；trace 不可用时回落到备用地址；握手通的边缘返回挑战页时标出）。host.js 29 个用例，28 过、1 跳过。**跳过的是“系统 1 个坏地址 + DoH 回 5 个、其中 1 个好 → 成功”**：本机沙箱拦截一切发往非 loopback 地址的 TCP（同进程、即使本机有监听也一样；直连公网 IP 则黑洞），DoH 给的非保留地址永远到不了本地“好服务器”，而 loopback 地址又过不了放行校验——成功路径在本机不可测，显式跳过、由真机验收验证（见 §7 末尾的诚实边界）。轮换用例在测试进程内直接调 `doFetch`（`doFetch` 因此导出；`require(host.js)` 会在测试进程内起监听服务器，`after` 里调导出的 `closeServer()` 关掉，否则 `node --test` 等不到进程退出）；DoH 缓存按主机名记在模块级内存里，轮换用例各用一个主机名（`doh-allfail`/`doh-cache.test`）避免缓存串用例；`HG_DOH_BASE_URL` 是每次调用时读 env（不是模块加载时），用例可按测试切换 mock 端点。注意：本机 Node 是 v24，目标是 Node 18.20.4——所用核心 API（`http`/`https`/`net`/`tls`、自定义 agent、`lookup`）在 18→24 间稳定，仍要在真机上以 Node 18 复核。**但 `autoSelectFamily` 的默认值 18 与 24 不同（18 默认关、24 默认开）**：`all:false` 的 lookup 路径端到端测不到，只能把 `makeLookup` 导出后用子进程单独调（见测试文件）；不要只凭本机端到端通过就认定 Node 18 没问题——v0.0.72 就是这么漏过去的。
  - 失效镜像重定向测试（v0.0.81）：纯函数单测（两个 .cf 域名各改写到 .xyz，只换 host，路径/查询/端口不动；非列表内 host 不动）；mock Node 集成测试（请求 `https://woggpan.xxooo.cf/vodshow/1--------1---.html?x=1` 时 Node 收到改写后的 `https://woggpan.888484.xyz/...`——Node 只从 URL 取 hostname 做 SNI/Host，收到改写后的 URL 就是 SNI/Host 已改写的证明；第二个 .cf 域名同款；`www.wogg.net` 不动）；报错后缀测试（mock Node 失败时 `（解析: …）` 里是改写后 host 的解析地址——本机解析器给 .xyz 与 .cf 各回不同的 FakeIP，用同一解析器现查两边作对照，不硬编码地址）；每次改写写进 ext.log（requestId `"net"`，`失效镜像重定向：woggpan.xxooo.cf → woggpan.888484.xyz`；非列表内 host 不记）；探针不重写（`diagnose` 请求体里的域名仍是原始 .cf，且 ext.log 里无重定向行）。hpp 102 过 + 1 既存失败（`api_limits_cover_bodies…` fixture 超时，自 v0.0.41 起每次都有，已在干净 v0.0.78 树上复现确认、不是回归，不修）。host.js 本版没动，29 个用例，28 过、1 跳过（与 v0.0.80 相同）。
  - 探针准确性升级测试（v0.0.82）：Rust——对话框选中模式测试（已保存直连、`diagnose("custom", "http://127.0.0.1:7890", "0.0.82")` 时 `/diagnose` 请求体里 `proxyUrl` 是自定义地址、`proxyDesc` 是 `自定义 http://127.0.0.1:7890`，两次真实分类请求的 proxyUrl 也都是它）；真实分类请求测试（mock `/request` 对 `/vodshow/` URL 回带 `module-item` 的 HTML：① .cf 到达 Node 时已是改写后的 .xyz URL、报告写明改写 + 解析地址 + HTTP 200 + 含列表内容，② 对照不改写，结论含“经改写后成功”；失败用例：mock Node 500 时两次请求都失败、不重试，报告原样写错误、结论点名失败）；无效自定义地址/模式直接报错、不静默换已保存的设置；结论纯函数单测（全通 + 改写后成功、改写后仍 TLS 被断点名原因、域名分句的无可用地址/部分通/DoH 查询失败三分支）；`diagnose` 的域名对照部分仍不重写（请求体里仍是原始 .cf），但真实分类请求检查里 .cf 改写一次、ext.log 里正好一行重定向记录；mock 的 `/diagnose` 回 summary（与报告文本自洽），`diagnose_last` 单独记 `/diagnose` 请求体（两次分类请求后 `last` 已是分类请求）。hpp 109 过 + 1 既存失败（同上）。host.js：2 个新用例——`/diagnose` 回域名小结（与“两组 DNS 地址”用例同一套布景：sys 2、doh 1、edges 3、tlsOk 1、httpOk 1、tlsFail ≥ 1——DoH 地址在本机沙箱里可能报 TCP 不通或 TCP 通/TLS 失败，不断死数）；无可用地址时小结全零、报告写“未做握手”（沙箱解析器给任何合法主机名都造 FakeIP，用非法主机名 `a..b` 触发该分支；真机上由真正无记录的域名触发）。host.js 31 个用例，30 过、1 跳过（仍是 v0.0.80 那个“补充地址→成功”用例）。
  - WebHome 全部数据清理测试（v0.0.83）：hpp `webhome_data`——删键脚本字符串断言（只含三个键、不用 `localStorage.clear`/sessionStorage/cookie、不含 reload）；rquickjs 真跑删键脚本（三个键都在、另有一个无关键：跑完三个键全删、无关键还在、existed 如实回报）；无窗口时报 `WEBHOME_NOT_OPEN` 且 Cookie 罐清理闭包一次也不跑；全清按顺序跑（JS 删键 → 清浏览数据 → 清罐 → 重载）且报告内容正确（cleared/absent/jar_cleared）；回调失败/非 JSON/`ok:false`/回调不来（短超时）/清浏览数据失败/清罐失败/重载失败各路径都不继续往下走、不重载；`Network::clear_cookie_jar` 单测（种子 cookies.json 含 1 条普通 Cookie + 1 条 web 登录 Cookie：清完返回 2、内存罐空、落盘文件变成 `[]`、重建 Network 读回也是空罐）。hpp 121 过 + 1 既存失败（同上）。host.js 本版没动，无需重跑（31 个用例与 v0.0.82 相同）。
  - 全 App 缓存清理测试（v0.0.84）：hpp `webhome_data`——编排器改成中间步骤 + 日志两步闭包：无窗口时报 `WEBHOME_NOT_OPEN` 且中间与日志闭包一次也不跑；全清按顺序跑（JS 删键 → 清浏览数据 → 中间 → 重载 → 日志）且报告内容正确（新增字段全在）；中间步骤的 warnings 如实进报告、重载与日志照常跑；重载失败时日志闭包不跑（日志留着查错）；其余失败路径（回调失败/非 JSON/`ok:false`/回调不来/清浏览数据失败）仍不继续往下走、不重载。`manager`——两插件 fixture（一个已启用、一个已停用）：全清返回各类别计数正确（罐子 3 条、响应缓存 2 条、`webhome:` 键 2 个、`__pycache__` 目录 2 个、零散 .pyc 1 个），罐子落盘变 `[]`、响应缓存已空、`webhome:` 键已删、自己的键还在、`.py` 全在、只删了字节码；`clear_all_logs` 丢掉两插件的全部日志行、第二次返回 0。`network`——`clear_response_cache` 丢掉全部条目并返回条数，Cookie 罐不动。`webhome`——`clear_webhome_prefix_keys` 只清 `webhome:` 前缀，插件自身的键不动、存储文件不删。`preload`——`Cache::clear` 丢掉全部条目并返回条数。`logs`——`LogBuffer::clear` 丢掉全部行并把已丢弃计数归零。Python 集成：demo 蜘蛛 `setCache`/`getCache` 命中 → `clearCache` 返回 `succeed` → `getCache` 未命中（runner 把 None 归一成 `{}`）、空缓存再清也成功，47 项全过（42 + 5 个新检查）。hpp 127 过 + 1 既存失败（同上）。host.js 本版没动，无需重跑（31 个用例与 v0.0.82 相同）。
  - Cookie 桥测试（v0.0.86）：hpp `bridge_tests` 6 个新用例——合并进普通罐并落盘（重建 Network 读回同一条）、同名同域同路径覆盖旧值、`__cf_bm`（大小写）不收、已过期/空名/空值/空域名不收、host-only Cookie 不发给子域、域名 Cookie 发给子域；`log_cookie_bridge` 写 ext.log 行（requestId `"net"`，含桥接计数与名字；0 个也记一行）。hpp 133 过 + 1 既存失败（同上）。host.js 本版没动，无需重跑（31 个用例与 v0.0.82 相同）。
  - 加宽桥与响应日志测试（v0.0.87）：hpp `bridge_tests` 6 个新用例——首页 Cookie 只附给桥时那个 origin 的请求（其他 origin 不附带）、罐子同名 Cookie 优先（不重复带）、Secure 的只给 https 目标（http 目标不附带）、`__cf_bm`/已过期/空名/空值/空域名不收、同一 origin 重复桥覆盖旧值、32 个 origin 上限（超了丢最旧的）；`log_home_cookie_bridge` 写 ext.log 行（requestId `"net"`，含桥接计数、名字与来源域；0 个也记一行）。hpp `response_log_tests` 2 个新用例——`response_markers` 纯函数（module-item、验证密码/访问人数过多、大小写不敏感的 just-a-moment、空体）；`log_response` 写 ext.log 行（requestId `"net"`，含 HTTP 状态、Set-Cookie 名字、无 Set-Cookie 时写“无”、三组体标记；直连重试那跳带 `（直连重试）` 标记）。hpp 141 过 + 1 既存失败（同上）。host.js 本版没动，无需重跑（31 个用例与 v0.0.82 相同）。
- 前端 `tsc` + `vite`（两份 tsc + vite build）、`docs:build` + `docs:test`（`PYTHON=python3`）。
- 验包脚本：沿用 `verify-0.0.69.py` 的模板改 `verify-0.0.70.py`（推送后由交付流程跑；本阶段不跑 CI 验包，但实现时要留出可验证的标记：`NodeMobile` framework、`.app/node-app/host.js`、`process.version`、Rust 侧新标记 `is_node_transport` / `send_api_hop` / `node_start`（v0.0.71 起是 `dlsym` 的符号名字符串，不再是链接符号）/ `dlopen` 等）。

### 10.2 只有 CI / 真机能验证的

- iOS 构建：framework 的 Embed（仍由 `sync-ios-node.mjs --install-app` 拷进 `.app/Frameworks/`）与签名、Swift 不参与——只在 macOS CI（release.yml，tag 触发）上验证。**v0.0.71 起不再有链接期依赖**：主二进制里不应再有 `LC_LOAD_DYLIB … NodeMobile`（`dlopen` 是运行时加载，验包时核对）；`dlopen` 的路径与 `node_start` 解析只能在真机（或 CI 产出的包）上验证。本机 Linux 编不出 iOS 包，**不要把本机检查说成 iOS 验证通过**。
- 真机验收（用户做）：打开 WebHome 玩偶站，分类与海报能加载、不再报“分类失败：响应被拦截”；代理测试仍连接正常；py 源能播；跟随系统与直连两种模式各测一次；`fm.req("https://api.ipify.org")` 看 Node 实际出口 IP（§6.5）。**v0.0.77 的 clearance 验收判据（按顺序）：① 分类能不能出来（拦截页不再出现）；② 若仍失败，看报错——`（解析: …）` 是否还在、是不是 TLS 被断（通行证帮不上 TLS 层，见 §7）；③ 若 WebView 里挑战也没通过（例如一直要人点验证框），clearance 路线不成立，如实回报。**v0.0.78 的窄回落验收判据（按顺序）：① 玩偶分类报错是不是变成带代理描述的 `UPSTREAM_ERROR`（消息里应有“代理: 跟随系统 …；解析: …；直连重试: …；Cookie: …”）；② 直连重试有没有成功——分类直接出来说明代理路径是分歧点，仍报 `同样 502` 说明与代理无关；③ 跟随系统与直连两种模式各测一次并告知模式（直连模式下不应有回落，报错应是“未尝试（已直连）”）；④ 插件日志（ext.log）里应有 requestId 为 `net` 的代理决议行。**v0.0.79 的轮换验收判据（按顺序）：① 直连模式玩偶分类能不能出来；② 若仍失败，看报错——已试地址是只有一个还是多个（`（已试: …；仅 1 个地址，无处可换）` 说明解析只回了一个、轮换无处可换；`共 n 个地址` 说明轮换试过了 n 个），以及 `（解析: …）` 里的地址与上次（198.20.0.19:443）是否相同；③ 跟随系统也测一次并告知模式（跟随系统走代理路径、轮换不适用，报错应只有 Rust 的解析后缀、没有 Node 的已试后缀）。**v0.0.80 的 DoH 补充验收判据（按顺序）：① 直连模式玩偶分类能不能出来；② 若仍失败，看报错——已试地址里有没有出现 `DoH:` 那组、共几个地址（有 `DoH: …` 说明 DoH 补充生效、轮换试过了 DoH 给的地址；只有系统地址 + `DoH 无结果` 说明 DoH 没拿到可用地址；仍是 `仅 1 个地址，无处可换` 说明系统与 DoH 两边都只拿到那一个）；③ 跟随系统也测一次并告知模式；④ 探针：设置页代理测试旁点「网络诊断」，等十几秒后截图报告——DNS 对照里系统与 DoH 各回几个地址、逐边缘握手哪几个通/哪几个断、出口 IP 是什么（报告同时写进 ext.log）。**v0.0.81 的失效镜像重定向验收判据（按顺序）：① 直连模式玩偶分类能不能出来；② 若仍失败，看报错——`（解析: …）` 里的地址是不是改写后 host 的（.xyz 段的 104.x/162.x/172.x/108.x，而不是 198.20.0.x），以及 ext.log 里有没有 `net.request 失效镜像重定向：… → woggpan.888484.xyz` 的记录；③ 跟随系统也测一次并告知模式；④ 探针再跑一次（应仍报原始 .cf 的 TLS 失败——探针不重写）。**v0.0.82 的探针准确性升级验收判据（按顺序）：① 设置页点「网络诊断」前，先在对话框里选一个**未保存**的模式（例如自定义填一个地址、不点保存）再点诊断——报告头应写实际用的模式（自定义地址），而不是已保存的模式；② 报告顶部应有结论行 + 版本行（版本应是 0.0.82，改写状态应列出两个 .cf→.xyz 映射）；③ 报告末尾的真实分类请求检查：① .cf 那条应写 `改写：woggpan.xxooo.cf → woggpan.888484.xyz`，若 HTTP 200 且含列表内容则结论应写“经改写后成功”；若仍失败，看失败行里的完整错误——解析地址是不是 .xyz 段（104.x/162.x/172.x/108.x）而不是 198.20.0.x（是 .xyz 段说明改写已生效、失败在别处；仍是 198.20.0.x 说明请求根本没走到改写——对照 §7 v0.0.82 的只读诊断结论）；④ 探针的域名对照部分仍应报原始 .cf 的 TLS 失败（探针不重写对照部分）；⑤ 探针跑完后已保存的代理设置不应变（对照前对话框里选中的模式与已保存的模式是否一致）。**v0.0.83 的全部数据清理验收判据（按顺序）：① 设置页代理测试旁点「清理 WebHome 全部数据」，确认框应写清会清掉什么（监控快照、本站域名缓存、手动设置的域名、WebView 全部浏览数据、请求 Cookie）与不动什么（App 设置、插件、下载的视频）；② 清完结果页应写清掉了哪几个键（哪些本来就没有）、请求 Cookie 清掉了几条，并提示登录态已掉、请重登后再打开 WebHome 点一次分类；③ 清完后重开 WebHome、点一次玩偶分类——能不能出来（清完页面重新从监控同步域名；若监控同步仍失败，页面仍用默认域名列表，分类可能仍失败——那时看报错里的解析地址是不是 .xyz 段：是 .xyz 段说明旧快照已清掉、失败在别处；仍是 198.20.0.19 说明域名数据又从别处回来了）；④ WebHome 没打开过时点清理，应提示“请先打开一次 WebHome（任意站点），再回来清理”，不应新建窗口；⑤ 清完后 App 的代理设置、已导入的插件、下载好的视频应都在（主窗口本地偏好可能恢复默认，已知、见 §7）。**v0.0.84 的全 App 缓存清理验收判据（按顺序）：① 设置页 → 缓存清理 → 点「清理全部缓存」（v0.0.85 起入口从代理对话框移出，独立成「缓存清理」区块），确认框应写清会清掉什么（三个域名缓存、WebView 全部浏览数据、全部插件的请求 Cookie 与响应缓存、WebHome 缓存键、媒体预加载缓存、Python 字节码缓存、Python 边车运行时缓存、导入暂存文件、最后清日志）与不动什么（App 设置、插件与插件源文件、插件存储里自己的数据、下载的视频）；② 清完结果页应按类别写数字（哪几个键清掉了、哪些本来就没有、几个插件的 Cookie 清掉了几条、响应缓存丢了几条、`webhome:` 键删了几个、预加载缓存丢了几条、`__pycache__` 删了几个目录几个零散 .pyc、Python 边车几个蜘蛛清掉了几个、导入暂存清掉了几个文件、日志清掉了几行、App 调试日志文件是否已清空），并写“日志已清空，清完后日志从空开始，下次出问题时新日志更干净、更好定位”，提示登录态已掉、请重登后再打开 WebHome 点一次分类；③ 清完后重开 WebHome、点一次玩偶分类——能不能出来（清完页面重新从监控同步域名；若监控同步仍失败，页面仍用默认域名列表，分类可能仍失败——那时看报错里的解析地址是不是 .xyz 段：是 .xyz 段说明旧缓存已清掉、失败在别处；仍是 198.20.0.19 说明域名数据又从别处回来了）；④ WebHome 没打开过时点清理，应提示“请先打开一次 WebHome（任意站点），再回来清理”，不应新建窗口；⑤ 清完后 App 的代理设置、已导入的插件与插件源文件（.py、.js、HTML 源）、插件存储里自己的数据、下载好的视频应都在；⑥ 清完后插件日志面板（ext.log）应是空的，下次出问题时从空开始记。**v0.0.85 的清理按钮 ACL 验收判据：① 设置页 → 缓存清理 → 点「清理全部缓存」，确认框应正常弹出、点确认后清理真的跑起来——结果页按类别出数字，不应再报 `Command clear_webhome_data not allowed by ACL`（只看二进制里有 `clear_webhome_data` 字串不算数，v0.0.84 就是这么漏过去的）；② 清完后重开 WebHome、点一次玩偶分类，看 HTTP 500 还在不在——500 是源站回的，清理只能排除缓存因素，若 500 仍在再按 host 诊断，不在本版修；③ 其余按钮（网络诊断、测试连接、保存代理）应仍正常。**v0.0.86 的 Cookie 桥验收判据（按顺序）：① 清缓存 → 打开 WebHome → 等首页加载完 → 点玩偶分类——能不能出来（过了就是桥修好了；还被拦看下一步）；② 若仍失败，打开插件日志面板（ext.log），找 `net.request Cookie 桥接：<n> 个（名字：…）` 那行——有这行且 n>0 说明桥跑了、Cookie 过去了，失败在桥以外；只有 `0 个` 说明 WebView 里没有目标 URL 的 Cookie 可桥（桥本身没东西可搬），下一跳不是 Cookie；③ 若分类出来了，再点一次别的分类/换个站点（木偶）确认不是偶然一次；④ 其余功能（网络诊断、清理、py 源）应仍正常。****v0.0.87 的加宽桥与响应日志验收判据（按顺序）：① **这次不要先清缓存**（罐子已经是空的，清了只会抹掉日志）→ 打开 WebHome → 点一次玩偶电影分类——能不能出来（过了就是加宽桥修好了；还被拦看下一步）；② 若仍失败，打开插件日志面板（ext.log），找两行：加宽桥那行 `net.request Cookie 桥接（首页 <来源域>）：<n> 个（名字：…）` 与响应那行 `net.request 响应：HTTP <状态>；Set-Cookie：<名字或“无”>；内容：module-item=<是/否>，验证页=<是/否>，just-a-moment=<是/否>`，把这两行发回来——桥接行 n>0 说明首页的 Cookie 附过去了；响应行会直接说明源站到底回了什么（HTTP 200 + 验证页=是 = 源站仍在下验证页；HTTP 500 = 源站错误；module-item=是 = 其实回了列表、失败在别处）；③ 若分类出来了，再点一次别的分类/换个站点（木偶）确认不是偶然一次；④ 其余功能（网络诊断、清理、py 源）应仍正常。****

### 10.3 如果 Node 也被拦

如实回报：画像假说被证伪。下一跳候选（按顺序，不在本版做）：

1. **DNS 固定顺序（v0.0.74 已做，真机证伪）：**Node 路径改用系统解析器原顺序（`resolve_target_keep_order`），不做 IPv6 优先重排；固定本身不变。真机仍被拦，说明不是顺序问题，继续往下。
2. **持久化 Cookie 罐里的陈旧 `__cf_bm`（v0.0.75 已做，真机证伪）：**Rust 的 Cookie 罐落盘保存，WebHome 的 fm.req 会带上它（`credentials` 未设为 omit）；Cloudflare 拦截页的 `Set-Cookie: __cf_bm` 会被存下来，下次请求再发过去——失效的 bot 管理 Cookie 会让 Cloudflare 一直重新挑战。Arcane 的 net.request 不带 Cookie。改法：`receive_cookies` 不再存 `__cf_bm`，加载时把罐子里残留的清掉并回写（协议 04 §5 同步记了一行）。**这个嫌疑只能解释“响应被拦截”，解释不了 TLS 握手被断开。**TLS 那一层 v0.0.75 另有显式重试（§7）：直连时首个 pinned 地址 TLS 被断，Node 不回落，`doFetch` 自己换地址重试；重试只做直连路径，代理路径（HTTP CONNECT 由代理自己解析域名、SOCKS 只用首个 pinned 地址）不动。**v0.0.75 真机仍失败（2026-11-23 21:42–21:43），Cookie 假说被证伪**——清了 `__cf_bm` 拦截页照样来；TLS 重试也没派上用场（跟随系统走代理路径不适用、直连下的失败全在 HTTP 层）。下一跳是 v0.0.76 的去 pinning 实验，见下。
3. **去掉 pinning 的决定性实验（v0.0.76 已做，真机证伪）：**Node 路径不再把 Rust 解析结果钉给 Node（`pinnedAddrs` 传空），Node 用默认 lookup、不显式设 `autoSelectFamily`，与 Arcane 完全一致；Rust 侧的解析只做预检与诊断（失败时报错末尾带 `（解析: …）`）。依据见 §7（tokio 走系统解析器、真机 pinned 列表曾是 24 个 IPv4、Node 18 的 `autoSelectFamily` 会 250ms 后并行试下一个地址）。**v0.0.76 真机仍被拦（2026-11-23 23:52 录屏）**——玩偶 臻选 → 约 5 秒后 `分类失败：无响应被拦截`；木偶 动漫 → TLS 被断，报错带 `（解析: 198.20.19.443）`（解析到的就是单个真实边缘 IP，不是 Karing FakeIP）；随后骨架屏约 25 秒 → `响应被拦截`；电影 → 骨架屏转到录屏结束。结论：pinning 与 lookup 路径全部排除。下一跳是 ④。
4. WKWebView clearance（v0.0.77 已做，真机已验：对 502 无效）：见 §7（v0.0.77 条目）。机制是隐藏 iframe 导航 + 顶层导航兜底，不是页面 `fetch`（`fetch` 不执行挑战页的 JS，拿不到通行证）；读 Cookie 用的是 Tauri 2.12.5 自带的 `WebviewWindow::cookies_for_url`（底层即 vendored wry 的 `WKWebView::cookies()`），旧文“没有现成宿主接口”的说法已过时。**真机结果（2026-11-24 00:51 录屏）**：拦截页与 TLS 被断那两类失败没再出现，取而代之的是上游 502（玩偶分类约 5 秒内 `HTTP 502`，木偶/海报一直挂起）——502 不是拦截页也不是 TLS 被断，clearance 的触发条件碰不上它，**clearance 对 502 无效**；但这同时说明 clearance 那路本身没再被触发（没有证据说它坏了，只是与 502 无关）。下一跳是 ⑤。
5. 窄回落 + 诊断（v0.0.78 已做，真机已验：TLS 被断时按设计不触发）：见 §7（v0.0.78 条目）。只在跟随系统模式、只在 Api 的 Node 路径：代理路快速 502/503/504 或代理握手失败时直连重试一次；5xx 转 `UPSTREAM_ERROR`（带代理描述、解析地址、两次结果、Cookie 名字）；每次 net.request 的代理决议写进 ext.log。**不默认直连的规矩不变**——首跳永远先走代理，回落是窄的、有条件的实验，不是改默认路由。**真机结果（2026-11-24 06:25 截图）**：玩偶 臻选直连模式下 TLS 被断（解析 198.20.0.19:443），不是 502，回落按设计不触发；跟随系统也是一样的 TLS 被断、同样的解析地址（06:35 截图），代理彻底洗清——回落的定性仪器作用已经完成（不是代理的问题）。下一跳是 ⑥。**另立项（本版不修）**：https 代理明文 CONNECT 的潜 bug——host.js:206 不区分 scheme（诊断报告 §五），单独修。
6. 直连 TLS 被断换地址重试（v0.0.79 已做、真机已验：解析只回 1 个地址，轮换无处可换）：见 §7（v0.0.79 条目）。`doFetch` 自己拿系统顺序地址列表逐个试，不用 pinning、不用 `autoSelectFamily`；诊断带已试地址与解析总数，直连与跟随系统两种模式都能从报错里读出结论。**真机结果（2026-11-24 07:27 截图，跟随系统；07:36 截图，直连）**：仍是 TLS 被断，解析第四次仍是 198.20.0.19，且只回 1 个地址——轮换已正确触发但无处可换；07:27 报错缺 `（已试: …）` 后缀是符合设计（跟随系统轮换不适用），07:36 直连的报错后缀完整，后缀逻辑没有 bug。下一跳是 ⑦。
7. DoH 取全量边缘地址 + 轮换，同时带网络诊断探针（v0.0.80 已做，真机已验：探针证明 .cf 全死、分类打到死镜像）：见 §7（v0.0.80 条目）。系统列表不够用时才向 DoH 补地址（只直连路径，60 秒缓存，静默退回）；探针在设置页一键跑 4 个玩偶域名的系统解析 vs DoH 对照、逐边缘 TLS 握手、握手通的再 GET、出口 IP，报告应用内可看 + 写 ext.log。验收判据见 §10.2。**真机结果（2026-11-24 09:28 探针 + 09:30 截图）**：探针是决定性的——直连模式下 .xyz/.net 全通、.cf 全死（见 §7 v0.0.81 条目的完整报告）；09:30 分类仍失败在 198.20.0.19（.cf 段地址），说明分类请求打到了已死的 .cf 镜像，DoH 对 .cf 无能为力。下一跳是 ⑧。
8. 失效镜像重定向（v0.0.81 已做，真机已验：改写本身没被绕过，但分类仍失败在映射表外的 host 上）：见 §7（v0.0.81 条目）。Node Api 请求路径上把两个已死的 .cf 镜像 upfront 改写到活的 .xyz 镜像（只换 host，解析前改写，写 ext.log；探针不重写）。验收判据见 §10.2。**真机结果（2026-11-24 10:54 截图 + 11:08 重测，均直连、均在已确认的 0.0.81 上）**：分类仍失败，报错与 v0.0.80 的 09:30 一字不差（`已试: 198.20.0.19；DoH 无结果；仅 1 个地址`）——“旧版”推断被推翻；只读诊断（11:20）定案：Rust 侧改写不可能被绕过，两份后缀互证 11:08 那次请求的 host 根本不在映射表里，最可能是页面实际用的域名被改掉了（监控同步拉来的 best_url / 用户手动覆盖域名列表），子形态是 IP 字面量 URL 或第三个未列入映射表的 .cf 镜像。下一跳是 ⑨（探针当场验证改写）+ 核对页面实际请求的 URL（监控快照/用户覆盖），而不是继续改传输层。
9. 探针诊断准确性升级（v0.0.82 已做，真机已验：探针证明改写生效、页面仍失败在映射表外的 host 上）：见 §7（v0.0.82 条目）。探针用对话框当前选中模式跑、加两次真实分类请求检查（.cf 检验改写、.xyz 作对照）、报告头加版本号与改写状态、顶部加一句话结论；探针的域名对照部分仍不重写。验收判据见 §10.2。若探针的真实分类检查通过（.cf 经改写后 HTTP 200、含列表内容）但页面分类仍失败，说明页面实际请求的 URL 与探针测的两个 URL 不同（IP 字面量/第三镜像/监控快照）——下一跳是核对页面实际请求的 URL；若探针检查也失败，看失败行里的解析地址是 .xyz 段还是 198.20.0.x（前者说明改写已生效、失败在别处；后者说明请求根本没走到改写）。**真机结果（2026-11-24 11:40/11:41 探针视频 + 11:46–11:47 分类重测）**：探针的真实分类检查通过（.cf 经改写后 HTTP 200 含 module-item、.xyz 对照同样 200；.xyz 7/7、.net 4/4 全通，.cf 双镜像 TLS 全被断、DoH 无记录；出口 IP 仍 23.237.196.226 LAX）——改写在真实路径上生效；但页面分类仍失败、同一 198.20.0.19 错误，机制①彻底坐实。11:56–11:57 视频：用户点的监控同步约 2 秒就回来、列表一字未变（同步失败，见 §7 v0.0.83 条目），各站域名列表冻在旧快照上、无法刷新。下一跳是 ⑩。
10. WebHome 全部数据清理（v0.0.83，已做，待真机验收）：见 §7（v0.0.83 条目）。设置页一键清掉页面三个域名缓存键 + WebView 全部浏览数据 + 绑定插件的 Rust 请求 Cookie 罐，再重载页面；窗口不存在时提示先打开一次 WebHome。验收判据见 §10.2。若清完重开 WebHome、分类仍失败，看报错里的解析地址是 .xyz 段还是 198.20.0.x（前者说明旧快照已清掉、失败在别处；后者说明域名数据又从别处回来了）；若监控同步仍失败（11:56 视频里就是失败的），页面可能又拉不到新列表——同步本身仍失败是已知问题，不在本版修，下一跳是查监控同步为什么在真机上 2 秒就失败。
11. 全 App 缓存清理（v0.0.84，已做，待真机验收）：见 §7（v0.0.84 条目）。设置页一键清掉全部运行时缓存（页面三键 + WebView 浏览数据 + 全部插件的罐子/响应缓存/`webhome:` 键 + 媒体预加载 + `__pycache__`/`*.pyc` + Python 边车运行时缓存 + 导入暂存），再重载页面，最后清日志（ext.log + debug.log）；红线是插件、py源、HTML源、设置、下载一律不删。验收判据见 §10.2。若清完重开 WebHome、分类仍失败，看报错里的解析地址是 .xyz 段还是 198.20.0.x（前者说明旧缓存已清掉、失败在别处；后者说明域名数据又从别处回来了）；若监控同步仍失败（11:56 视频里就是失败的），页面可能又拉不到新列表——同步本身仍失败是已知问题，不在本版修，下一跳是查监控同步为什么在真机上 2 秒就失败。
12. 清理按钮的 ACL 权限漏了（v0.0.85，已做，待真机验收）：见 §7（v0.0.85 条目）。v0.0.84 真机上报：设置页点「清理全部缓存」报 `Command clear_webhome_data not allowed by ACL`，清理一次都没跑成——所以 v0.0.84 的“清缓存有没有用”这次测试没有结论；分类的 HTTP 500（源站回 500、直连重试也 500）是另一个故障，不在本版修。改法：default.json 补 `allow-clear-webhome-data` + app.toml 补权限定义（只加主窗口），另有 `scripts/check-acl.mjs` 回归检查（扫主窗口前端全部 invoke 调用点、断言权限都在白名单里）。验收判据见 §10.2。**真机结果（2026-11-24 14:36–14:37 视频）**：清理跑成了（无 ACL 报错），但分类仍失败——失败又变了：源站下发「最新@网址」验证页，页面报「响应被拦截」（500 不见了）；随后对照定性（Arcane 同机同时正常、HGhub 切直连也失败），出口 IP 洗清，下一跳是 ⑬。
13. WebView→引擎 Cookie 桥（v0.0.86，已做，真机已验：桥跑了但每次都是 0 个，杠杆错了）：见 §7（v0.0.86 条目）。v0.0.85 真机（2026-11-24 14:36–14:37 视频）：清理跑成了，但分类仍失败——源站下发「最新@网址」验证页（「当前访问人数过多，请验证密码」），页面报「响应被拦截」；连 WebView 也被下发同一页。对照定性（用户 14:48–14:54）：Arcane 同机同时浏览正常、玩偶「电影」分类完整出内容（14:49 截图 IMG_6150.jpg）；HGhub 切直连也还是失败——出口 IP 彻底洗清，差别只剩请求本身。头号假说仍是 Cookie（三点全合，见 §7）。改法：引擎请求前把 WebView 里目标 URL 的 Cookie 合并进 Rust 罐（只做 WebView→引擎），每次桥在 ext.log 里记一行（只报名字、不报值；0 个也记）。验收判据见 §10.2。**真机结果（2026-11-24 15:26 视频，插件 ext.log）**：桥跑了，但每次都是 `Cookie 桥接：0 个`；全部请求 `Cookie：未附带`（罐空）；代理直连；目标包括 ipapi.co/ip.sb、pan-site-monitor.vercel.app + site.920410.xyz、玩偶 4 个域名全 race；分类还是打不开。**杠杆错了**：WebView 只加载 homePage（https://wo.vivas.cc.cd），从不访问 wogg* 域名，所以它没有 wogg* 的 Cookie 可桥——按目标 URL 读 Cookie 永远是 0 个。另两点已核实：罐子会存响应里的 Set-Cookie，但整个会话里没有任何请求附带 Cookie，说明源站当前响应根本不下发可存的会话 Cookie；ext.log 只有请求行、没有响应日志，源站到底回了什么看不见。下一跳是 ⑭。
14. 加宽 Cookie 桥 + 引擎响应日志（v0.0.87，已做，待真机验收）：见 §7（v0.0.87 条目）。v0.0.86 的教训：WebView 里没有目标 URL 的 Cookie 可桥（每次 0 个），剩下唯一的 Cookie 来源是 WebView 里首页的 Cookie——如果验证页加载时落了会话 Cookie，而源站只认 Cookie 名不认它来自哪个域，那它就是能用的钥匙。改法：引擎请求前再读首页 URL 的 Cookie，附到当次请求上（只附当次、不进罐子、不落盘），记一行 ext.log（个数 + 名字 + 来源域；0 个也记）；每次 Node 引擎请求的响应也记一行（HTTP 状态 + Set-Cookie 名字 + 体标记：module-item / 验证页文本 / just-a-moment）。验收判据见 §10.2。若真机上加宽桥接行是 `0 个`、分类仍被拦，说明 WebView 里首页也没有 Cookie 可桥——那时看响应日志就知道源站到底回了什么（验证页/500/空体），下一跳按响应定；若桥接行 n>0、分类仍被拦，说明 Cookie 过去了也没用——Cookie 假说被证伪，下一跳是挖请求的其他差别（例如请求头里页面没给、引擎没带的字段，或 Node 与 WebView 的 TLS/行为差异）。

## 11. 影响的文档（与实现同一提交，AGENTS.md §7.4）

- `docs/plugin-protocol/04-host-api.md` §5：`__cf_bm` 不保存、不附加（v0.0.75）；`cf_clearance` 只认 WebView clearance 拿到的、只存内存、按主机匹配后附加（v0.0.77）。
- `docs/plugin-protocol/03-host-rules.md` §2：DNS 固定行加例外——Node 路径自 v0.0.76 起只做校验预检、连接时由 Node 重新解析（v0.0.76 去 pinning 实验；内网防护行的“只固定到允许的地址”改成校验时保留）。
- 本文档（新增）：`docs/developers/node-engine.md`。
- `docs/plugin-protocol/03-host-rules.md` §2：代理行改成“插件 net 请求经内嵌 Node 引擎（Node.js 18.20.4）发出；代理按用户设置（跟随系统/直连/自定义）由 Rust 侧解析、按请求传给 Node；TUN 层行为不变”；v0.0.78 加窄回落与 5xx 诊断（只在跟随系统模式、只在 Api 的 Node 路径；Node 路径 5xx 转 `UPSTREAM_ERROR`，wreq 路径仍当数据返回）。
- 本文档 §7：v0.0.78 窄回落与诊断条目；§10.1 新增测试；§10.2 v0.0.78 验收判据；§10.3 ④ 改成“已做、真机已验：对 502 无效”，新增 ⑤。
- 本文档 §7：v0.0.79 轮换条目；§10.1 新增测试（Rust 透传单测 + host.js 19/19）；§10.2 v0.0.79 验收判据；§10.3 ⑤ 改成“已做、真机已验：TLS 被断时按设计不触发”，新增 ⑥。
- 本文档 §7：v0.0.80 DoH 补充与探针条目；§10.1 新增测试（Rust：DoH 后缀透传单测 + `network_diagnose` 单测 + NodeClient 的 `doh` 字段与 `/diagnose` 单测；host.js 29 个用例，28 过、1 跳过——“补充地址→成功”用例本机不可测，见 §7 末尾的诚实边界）；§10.2 v0.0.80 验收判据；§10.3 ⑥ 改成“已做、真机已验：解析只回 1 个地址，轮换无处可换”，新增 ⑦。
- `docs/plugin-protocol/03-host-rules.md` §2：DNS 行补一句 DoH 补充规则（只直连路径、只在系统列表不够用时查、过同一套放行校验、静默退回）。
- 本文档 §7：v0.0.81 失效镜像重定向条目；§10.1 新增测试（Rust：纯函数单测 + mock Node 集成测试 4 个；host.js 没动）；§10.2 v0.0.81 验收判据；§10.3 ⑦ 改成“已做、真机已验：探针证明 .cf 全死、分类打到死镜像”，新增 ⑧。
- `docs/plugin-protocol/03-host-rules.md` §2：DNS 行补一句探针规则（v0.0.82：探针用对话框当前选中模式跑；真实分类请求检查走正常 net.request 路径；报告头带版本号与改写状态、顶部带结论行）。
- 本文档 §7：v0.0.82 探针准确性升级条目（含 11:20 只读诊断定论：改写不可能被绕过、11:08 失败请求的 host 不在映射表里、最可能是监控同步/用户覆盖改了页面实际域名，及探针测已知 URL 的诚实边界）；§10.1 新增测试（Rust：模式覆盖 + 真实分类请求 + 结论纯函数单测 7 个；host.js 新增小结用例 2 个）；§10.2 v0.0.82 验收判据；§10.3 ⑧ 改成“已做、真机已验：改写没被绕过、失败在映射表外的 host 上”，新增 ⑨。
- 本文档 §7：v0.0.83 全部数据清理条目；§10.1 新增测试（hpp webhome_data 12 个新用例；host.js 没动）；§10.2 v0.0.83 验收判据；§10.3 ⑨ 改成“已做、真机已验：探针证明改写生效、页面仍失败在映射表外的 host 上”，新增 ⑩。
- 本文档 §7：v0.0.84 全 App 缓存清理条目（含日志盘点：只限 App 自身的日志、ext.log 是内存缓冲、py/Node 边车无落盘日志、debug.log 是唯一落盘日志文件）；§10.1 新增测试（hpp：编排器中间 + 日志闭包、manager 全清、network 响应缓存、webhome 前缀、preload、logs；py 集成新增 clearCache 5 项）；§10.2 v0.0.84 验收判据；§10.3 新增 ⑪。
- 本文档 §7：v0.0.85 清理按钮 ACL 权限漏了条目（含回归检查 scripts/check-acl.mjs 与负向验证：临时去掉 allow-clear-webhome-data 后检查失败并点名、加回去后通过）；§10.2 v0.0.85 验收判据（按钮要点下去真的跑起来、结果页出数字，不能只看二进制里有字串）；§10.3 新增 ⑫。
- 本文档 §7：v0.0.86 Cookie 桥条目（含对照定性：Arcane 同机同时正常、HGhub 直连也失败，出口 IP 洗清；及诚实边界：WebView 里没有目标 URL 的 Cookie 时桥记 `0 个`）；§10.1 新增测试（hpp bridge_tests 6 个新用例；host.js 没动）；§10.2 v0.0.86 验收判据；§10.3 ⑫ 改成“已做、真机已验：清理跑成了、分类变验证页”，新增 ⑬。
- 本文档 §7：v0.0.87 加宽桥与响应日志条目（含 v0.0.86 杠杆错了的定论：WebView 只加载 homePage、没有 wogg* 的 Cookie 可桥，真机每次 0 个；及诚实边界：首页也没有 Cookie 时加宽桥记 `0 个`，那时看响应日志定下一跳）；§10.1 新增测试（hpp bridge_tests 6 个新用例 + response_log_tests 2 个新用例；host.js 没动）；§10.2 v0.0.87 验收判据（这次不要先清缓存）；§10.3 ⑬ 改成“已做、真机已验：桥跑了但每次都是 0 个，杠杆错了”，新增 ⑭。
- `docs/plugin-protocol/03-host-rules.md` §2：DNS 行补一句失效镜像重定向规则（只改 Node Api 路径、upfront 改写、探针不重写）。
- `docs/plugin-protocol/IMPLEMENTATION.md` HTTP 行：Node 式极简画像 → 内嵌 Node 引擎（`node_start` + 回环边车）。
- `docs/developers/status.md`：HTTP/Cookie 行与 WebHome 行同步更新（v0.0.70 走 Node 引擎；体积与 EOL 风险如实写）。
- 不改：协议签名（`fm.req` / `hg.fetch` 不变）、`src/plugins/protocol.ts`、07 兼容章（无 breaking change；非 iOS 行为不变）。

## 12. 风险清单（实现时逐项确认，不要静默绕过）

1. **EOL 与无人维护**（§3.4）：Node 18 已 EOL，nodejs-mobile 停滞；本版是 pin 死 v18.20.4 的假设检验，长期用要另做维护决策。
2. **体积**（§3.5）：IPA 预计 +25–35MB，以实际 IPA 为准。
3. **双运行时常驻内存**：Node（V8 堆 + libuv 池）与 CPython（解释器 + stdlib）各常驻数十 MB；iPhone 13 Pro Max 上要看真机内存峰值与后台被杀情况。
4. **单实例、不可重启、崩溃即全 App 崩溃**（§3.6、§8）。
5. **代理 agent 手写**（§6.3）：CONNECT 与 SOCKS 握手是手写协议代码，必须有 mock 代理的集成测试；任何一档实现错都会导致用户代理设置失效或流量走错出口。
6. **DNS 固定靠 Node 的 `lookup`**（§7）：`servername`/Host 必须仍是 hostname，实现时加断言测试。
7. **16KB 页对齐与签名**：framework 是 Xcode 15 / SDK 17.2 构建；若 CI 链接或打包时报对齐/签名问题，先查明再处理，不要静默降级。
8. **本机无法验证 iOS 链接**：`node_start` 的链接、framework 的 Embed 只能在 macOS CI 上验证；本地检查通过 ≠ iOS 能编过，交付时如实写。
