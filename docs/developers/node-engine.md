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
- `pinnedAddrs`：Rust 侧 `network_policy::resolve_target` 校验+固定后的地址列表（见 §7）；为空时 Node 用默认 DNS（正常情况下不应发生，Rust 侧先保证非空）。

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

协议 03 §2 要求"通过检查的解析结果直接用于连接，不在检查后重新解析"。Node 路径保留这条规则：

- Rust 侧在每跳发 Node 请求前调 `network_policy::resolve_target(target, strict)`（与今天 `Network::client()` 里同一调用）：校验（内网/保留地址拒绝、FakeIP 198.18.0.0/15 放行、混合答案只留允许的地址）+ IPv6 优先排序，得到 pinned 地址列表。
- 把列表（`"ip:port"` 字符串数组）随请求发给 Node；Node 的 `https.request` 用自定义 `lookup(hostname, opts, cb)` 返回该列表。**lookup 必须按 `opts.all` 回值**：`all: true` 时回 `[{address, family}]` 数组，`all: false` 时回首个地址字符串 + family。**v0.0.73 的教训**：旧代码无条件回数组；本机 Node 24 上 `autoSelectFamily` 默认开（lookup 以 `all:true` 被调用），测试全过；真机 Node 18.20.4 上 `autoSelectFamily` 默认关（lookup 以 `all:false` 被调用），Node 把数组当成单个 IP，直接抛 `Invalid IP address: [ [Object], ... ]`（个数即 pinned 个数）。所以 `doFetch` 里显式传 `autoSelectFamily: true`（Node 18 也走 Happy Eyeballs，逐个试 pinned 列表，不只试首个），同时 `makeLookup` 按 `all` 回值——两处缺一不可，缺前者 Node 18 只试首个地址，缺后者违反 lookup 契约。
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
- **host.js 的测试（本机可跑）：本机有 Node v24.22.1**，host.js 只用核心模块，可直接用系统 Node 跑集成测试：对本地 HTTP/HTTPS 服务器发请求（断言只发了页面给的头、无 sec-ch-ua、HTTP/1.1）；对本地 mock 代理发 CONNECT 与 SOCKS5/SOCKS4 握手（断言握手字节与隧道建立）；自定义 `lookup` 返回固定 IP 时断言 SNI/Host 仍是 hostname；**lookup 契约测试**（`all:true` 回数组、`all:false` 回单个地址字符串 + family，用真机上出现过的 pinned 列表形态：24 个 IPv4 与单个 IPv4/IPv6）与 **pinned 首个地址不通时试下一个**（`autoSelectFamily:true` 下首个死地址后回落到第二个）。注意：本机 Node 是 v24，目标是 Node 18.20.4——所用核心 API（`http`/`https`/`net`/`tls`、自定义 agent、`lookup`）在 18→24 间稳定，仍要在真机上以 Node 18 复核。**但 `autoSelectFamily` 的默认值 18 与 24 不同（18 默认关、24 默认开）**：`all:false` 的 lookup 路径端到端测不到，只能把 `makeLookup` 导出后用子进程单独调（见测试文件）；不要只凭本机端到端通过就认定 Node 18 没问题——v0.0.72 就是这么漏过去的。
- 前端 `tsc` + `vite`（两份 tsc + vite build）、`docs:build` + `docs:test`（`PYTHON=python3`）。
- 验包脚本：沿用 `verify-0.0.69.py` 的模板改 `verify-0.0.70.py`（推送后由交付流程跑；本阶段不跑 CI 验包，但实现时要留出可验证的标记：`NodeMobile` framework、`.app/node-app/host.js`、`process.version`、Rust 侧新标记 `is_node_transport` / `send_api_hop` / `node_start`（v0.0.71 起是 `dlsym` 的符号名字符串，不再是链接符号）/ `dlopen` 等）。

### 10.2 只有 CI / 真机能验证的

- iOS 构建：framework 的 Embed（仍由 `sync-ios-node.mjs --install-app` 拷进 `.app/Frameworks/`）与签名、Swift 不参与——只在 macOS CI（release.yml，tag 触发）上验证。**v0.0.71 起不再有链接期依赖**：主二进制里不应再有 `LC_LOAD_DYLIB … NodeMobile`（`dlopen` 是运行时加载，验包时核对）；`dlopen` 的路径与 `node_start` 解析只能在真机（或 CI 产出的包）上验证。本机 Linux 编不出 iOS 包，**不要把本机检查说成 iOS 验证通过**。
- 真机验收（用户做）：打开 WebHome 玩偶站，分类与海报能加载、不再报“分类失败：响应被拦截”；代理测试仍连接正常；py 源能播；跟随系统与直连两种模式各测一次；`fm.req("https://api.ipify.org")` 看 Node 实际出口 IP（§6.5）。

### 10.3 如果 Node 也被拦

如实回报：画像假说被证伪。下一跳候选（按顺序，不在本版做）：

1. WKWebView `fetch`（页面直接用浏览器 `fetch` 发请求，走 WebKit 完整浏览器网络栈；代价是过消息通道回传、受 CORS/页面上下文限制）。
2. 真浏览器拿 clearance cookie（复杂，Arcane 的证据表明它不是 Arcane 的做法）。
3. 停止在 Node 上继续投入（不要换个画像再试一版 wreq）。

## 11. 影响的文档（与实现同一提交，AGENTS.md §7.4）

- 本文档（新增）：`docs/developers/node-engine.md`。
- `docs/plugin-protocol/03-host-rules.md` §2：代理行改成“插件 net 请求经内嵌 Node 引擎（Node.js 18.20.4）发出；代理按用户设置（跟随系统/直连/自定义）由 Rust 侧解析、按请求传给 Node；TUN 层行为不变”。
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
