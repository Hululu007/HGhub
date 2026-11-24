//! Node 传输选择：Api 走 Node、Media 走 wreq、桩引导走 wreq、
//! 边车换端口后重探重试（照抄 v0.0.69 的
//! `python_sidecar_restart_on_new_port_retries_call`）。
use super::*;
use crate::node::{NodeBootstrap, NodeInfo, UnavailableNode};
use axum::{
    extract::Json,
    routing::{any, get, post},
    Router,
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use futures_util::future::BoxFuture;
use std::{
    collections::VecDeque,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

struct MockNode {
    calls: AtomicUsize,
    last: Mutex<Option<Value>>,
    /// 置 true 时 /request 回 500（v0.0.76：测 Node 报错的诊断后缀）。
    fail: AtomicBool,
    /// v0.0.78：proxyUrl 非空时回的上游状态（Some(502) 模拟代理路
    /// 502）；None 回 200。
    proxy_status: Mutex<Option<u16>>,
    /// v0.0.78：proxyUrl 为空（直连重试）时回的上游状态；None 回 200。
    direct_status: Mutex<Option<u16>>,
    /// v0.0.78：proxyUrl 非空时回的边车 500 错误文案（模拟代理握手
    /// 失败，如“代理拒绝 CONNECT（HTTP 502）”）；None 不触发。
    proxy_error: Mutex<Option<String>>,
    /// v0.0.78：proxyUrl 为空时回的边车 500 错误文案；None 不触发。
    direct_error: Mutex<Option<String>>,
    /// v0.0.78：每次调用的 proxyUrl（Some=经代理，None=直连），
    /// 按调用顺序记——断言回落只发生一次且第二跳是直连。
    proxy_urls: Mutex<Vec<Option<String>>>,
    /// v0.0.82：每次 /request 的 URL，按调用顺序记——断言探针的
    /// 真实分类请求到达 Node 时是改写后的 host。
    urls: Mutex<Vec<String>>,
    /// v0.0.82：最近一次 /diagnose 的请求体（与 /request 共用
    /// `last` 会互相覆盖——探针先调 /diagnose、再调两次 /request，
    /// 读 `last` 拿到的已是分类请求）。
    diagnose_last: Mutex<Option<Value>>,
}

struct MockUpstream {
    hits: AtomicUsize,
}

/// 带服务句柄的 mock Node 边车：调用方可以停掉它，模拟边车重启换端口。
/// shutdown 用优雅关闭：停监听的同时关掉已建立的 keep-alive 连接；
/// 只 abort 任务不够——axum 给每个已建立连接单开任务，abort 监听任务
/// 后池里的旧连接照常用，客户端根本不会触发刷新，测不到换端口重试。
struct MockServer {
    notify: Arc<tokio::sync::Notify>,
    task: tokio::task::JoinHandle<()>,
}

impl MockServer {
    async fn shutdown(self) {
        self.notify.notify_waiters();
        let mut task = self.task;
        if tokio::time::timeout(Duration::from_secs(3), &mut task)
            .await
            .is_err()
        {
            task.abort();
            let _ = task.await;
        }
    }
}

async fn start_mock_node() -> (String, Arc<MockNode>, MockServer) {
    let state = Arc::new(MockNode {
        calls: AtomicUsize::new(0),
        last: Mutex::new(None),
        fail: AtomicBool::new(false),
        proxy_status: Default::default(),
        direct_status: Default::default(),
        proxy_error: Default::default(),
        direct_error: Default::default(),
        proxy_urls: Default::default(),
        urls: Default::default(),
        diagnose_last: Default::default(),
    });
    let health = state.clone();
    let request = state.clone();
    let diagnose = state.clone();
    let router = Router::new()
        .route(
            "/health",
            get(move || {
                let _ = health;
                async { Json(json!({"ok": true, "node": "v18.20.4"})) }
            }),
        )
        .route(
            "/diagnose",
            post(move |Json(body): Json<Value>| {
                let state = diagnose.clone();
                async move {
                    *state.last.lock().unwrap() = Some(body.clone());
                    *state.diagnose_last.lock().unwrap() = Some(body.clone());
                    (
                        axum::http::StatusCode::OK,
                        // v0.0.82：/diagnose 除报告文本外再回域名小结
                        //（结论行用）；小结数字与报告文本自洽。
                        Json(json!({"report": "网络诊断 2026/11/24（代理：直连）\nexample.test：系统 1 个 [93.184.216.34]；DoH 2 个 [104.17.255.47, 172.66.199.198]\n  93.184.216.34：TCP 通，TLS 通（TLSv1.3 / TLS_AES_256_GCM_SHA384，ALPN 无 ALPN，10ms）；HTTP 200，非挑战页\n出口 IP：1.2.3.4（colo=HKG，cloudflare trace）", "summary": {"domains": [{"host": "example.test", "sys": 1, "doh": 2, "dohState": "ok", "edges": 3, "tlsOk": 1, "tlsFail": 0, "httpOk": 1}]}})),
                    )
                }
            }),
        )
        .route(
            "/request",
            post(move |Json(body): Json<Value>| {
                let state = request.clone();
                async move {
                    state.calls.fetch_add(1, Ordering::Relaxed);
                    *state.last.lock().unwrap() = Some(body.clone());
                    let proxy_url = body
                        .get("proxyUrl")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    state.proxy_urls.lock().unwrap().push(proxy_url.clone());
                    // v0.0.82：记每次 /request 的 URL（断言探针的真实
                    // 分类请求到达 Node 时是改写后的 host）。
                    state.urls.lock().unwrap().push(
                        body.get("url")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                    );
                    if state.fail.load(Ordering::Relaxed) {
                        return (
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({"error": "mock node down", "code": "NETWORK"})),
                        );
                    }
                    // v0.0.78：按有没有 proxyUrl 分流——代理路与直连
                    // 重试可以各自回不同的状态/错误，测窄回落。
                    let error = if proxy_url.is_some() {
                        state.proxy_error.lock().unwrap().clone()
                    } else {
                        state.direct_error.lock().unwrap().clone()
                    };
                    if let Some(msg) = error {
                        return (
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({"error": msg, "code": "NETWORK"})),
                        );
                    }
                    let status = if proxy_url.is_some() {
                        state.proxy_status.lock().unwrap().unwrap_or(200)
                    } else {
                        state.direct_status.lock().unwrap().unwrap_or(200)
                    };
                    let text = if status == 200 {
                        // v0.0.82：分类页 URL 回带列表内容标记的 HTML，
                        // 探针的真实分类请求检查靠它判定“含列表内容”。
                        if body
                            .get("url")
                            .and_then(|u| u.as_str())
                            .is_some_and(|u| u.contains("/vodshow/"))
                        {
                            "<html><body><div class=\"module-item\">x</div></body></html>"
                        } else {
                            "node-body"
                        }
                    } else {
                        "bad gateway"
                    };
                    (
                        axum::http::StatusCode::OK,
                        Json(json!({
                            "status": status,
                            "headers": {"content-type": "text/plain", "x-node": "1"},
                            "bodyBase64": BASE64.encode(text),
                        })),
                    )
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let notify = Arc::new(tokio::sync::Notify::new());
    let shutdown = notify.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async move { shutdown.notified().await })
            .await
            .unwrap();
    });
    (
        base,
        state,
        MockServer {
            notify,
            task: server,
        },
    )
}

async fn start_mock_upstream() -> (String, Arc<MockUpstream>, tokio::task::JoinHandle<()>) {
    let state = Arc::new(MockUpstream {
        hits: AtomicUsize::new(0),
    });
    let seen = state.clone();
    let router = Router::new().route(
        "/*path",
        any(move || {
            let seen = seen.clone();
            async move {
                seen.hits.fetch_add(1, Ordering::Relaxed);
                (
                    [("content-type", "text/plain"), ("x-wreq", "1")],
                    "wreq-body",
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (base, state, server)
}

struct FixedBootstrap {
    base: String,
}

impl NodeBootstrap for FixedBootstrap {
    fn ensure_started(&self) -> BoxFuture<'static, Result<NodeInfo>> {
        let base = self.base.clone();
        Box::pin(async move { Ok(NodeInfo { api_base: base }) })
    }
}

struct SwitchingBootstrap {
    bases: Arc<Mutex<VecDeque<String>>>,
}

impl NodeBootstrap for SwitchingBootstrap {
    fn ensure_started(&self) -> BoxFuture<'static, Result<NodeInfo>> {
        let bases = self.bases.clone();
        Box::pin(async move {
            let mut guard = bases.lock().unwrap();
            let api_base = if guard.len() > 1 {
                guard.pop_front().unwrap()
            } else {
                guard.front().unwrap().clone()
            };
            Ok(NodeInfo { api_base })
        })
    }
}

fn node_network(dir: &std::path::Path, bootstrap: Arc<dyn NodeBootstrap>) -> Arc<Network> {
    let meta = Meta::parse(
        json!({"id":"fixture.node","name":"Node","protocol":1,"version":"1.0.0","allowInsecureHttp":true}),
    )
    .unwrap();
    Arc::new(Network::new(meta, dir.into(), Some(false), Default::default(), bootstrap).unwrap())
}

#[tokio::test]
async fn api_fetch_goes_through_node_with_empty_pinned_addrs_and_no_proxy() {
    let (node_base, node, node_server) = start_mock_node().await;
    let (upstream, upstream_state, upstream_server) = start_mock_upstream().await;
    let dir = tempfile::tempdir().unwrap();
    let network = node_network(dir.path(), Arc::new(FixedBootstrap { base: node_base }));
    let result = network
        .fetch(
            Transfer::from_json(json!({"url": format!("{upstream}/x"), "responseType": "text"})),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result.json()["data"], json!("node-body"));
    assert_eq!(node.calls.load(Ordering::Relaxed), 1);
    assert_eq!(upstream_state.hits.load(Ordering::Relaxed), 0);
    // Node 收到的是 Rust 已过滤的头 + 本跳代理（默认跟随
    // 系统，本机无代理 → null 直连）；v0.0.76 决定性实验：
    // pinnedAddrs 为空，Node 用默认 lookup（与 Arcane 完全一致），
    // Rust 侧的解析只做预检与诊断，不再钉地址。
    let last = node.last.lock().unwrap().clone().unwrap();
    assert_eq!(last["url"], json!(format!("{upstream}/x")));
    assert_eq!(last["method"], json!("GET"));
    assert_eq!(last["proxyUrl"], Value::Null);
    assert_eq!(
        last["headers"]["user-agent"],
        json!("HGhub/HPP-dev"),
        "默认 UA 由 Rust 的 outgoing() 补，Node 不自己加头"
    );
    let pinned = last["pinnedAddrs"].as_array().unwrap();
    assert!(
        pinned.is_empty(),
        "v0.0.76：pinnedAddrs 应为空，Node 用默认 lookup：{pinned:?}"
    );
    node_server.shutdown().await;
    upstream_server.abort();
}

#[tokio::test]
async fn node_error_carries_resolved_addresses() {
    // v0.0.76：Node 请求失败时，Rust 侧预检解析到的地址附在报错末尾
    // （解析: …），直连/代理、NETWORK/TIMEOUT 都带；拦截页是 HTTP 200，
    // 没有宿主错误，本来就带不上——实验本身是那一路的判据。
    let (node_base, node, node_server) = start_mock_node().await;
    node.fail.store(true, Ordering::Relaxed);
    let (upstream, _upstream_state, upstream_server) = start_mock_upstream().await;
    let dir = tempfile::tempdir().unwrap();
    let network = node_network(dir.path(), Arc::new(FixedBootstrap { base: node_base }));
    let err = network
        .fetch(
            Transfer::from_json(json!({"url": format!("{upstream}/x"), "responseType": "text"})),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, "NETWORK");
    assert!(
        err.message.contains("（解析: 127.0.0.1:"),
        "Node 报错应带上预检解析到的地址：{}",
        err.message
    );
    node_server.shutdown().await;
    upstream_server.abort();
}

#[tokio::test]
async fn node_tried_suffix_passes_through_with_resolved_count() {
    // v0.0.79：Node 侧直连轮换的已试后缀（已试: …；共 n 个地址／仅 1
    // 个地址，无处可换）由 Rust 原样透传，Rust 再 append 自己的
    // （解析: …；共 n 个地址）——直连与跟随系统两种模式都要能从
    // 这条报错里直接读出“解析回了几个地址、轮换试了几个”。
    let (node_base, node, node_server) = start_mock_node().await;
    let (upstream, _, upstream_server) = start_mock_upstream().await;
    let dir = tempfile::tempdir().unwrap();
    let network = node_network(dir.path(), Arc::new(FixedBootstrap { base: node_base }));
    *node.direct_error.lock().unwrap() = Some(
        "Client network socket disconnected before secure TLS connection was established（已试: 127.0.0.2；仅 1 个地址，无处可换）"
            .to_string(),
    );
    let err = network
        .fetch(
            Transfer::from_json(json!({"url": format!("{upstream}/x"), "responseType": "text"})),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, "NETWORK");
    // Node 的后缀原样在前，Rust 的后缀 append 在后。
    let node_suffix = err
        .message
        .find("（已试: 127.0.0.2；仅 1 个地址，无处可换）")
        .expect("Node 的已试后缀应原样透传");
    let rust_suffix = err
        .message
        .find("（解析: 127.0.0.1:")
        .expect("Rust 的解析后缀应 append 在后");
    assert!(
        node_suffix < rust_suffix,
        "Node 后缀应在 Rust 后缀之前：{}",
        err.message
    );
    assert!(
        err.message.contains("共 1 个地址）"),
        "Rust 后缀应带解析总数：{}",
        err.message
    );
    node_server.shutdown().await;
    upstream_server.abort();
}

#[tokio::test]
async fn node_doh_suffix_passes_through_with_resolved_count() {
    // v0.0.80：Node 侧 DoH 补充后的已试后缀（已试: <系统地址>；DoH:
    // <DoH 地址>；共 n 个地址）由 Rust 原样透传，Rust 再 append 自
    // 己的（解析: …；共 n 个地址）——真机测试能直接读出系统与 DoH
    // 各试了哪几个地址。
    let (node_base, node, node_server) = start_mock_node().await;
    let (upstream, _, upstream_server) = start_mock_upstream().await;
    let dir = tempfile::tempdir().unwrap();
    let network = node_network(dir.path(), Arc::new(FixedBootstrap { base: node_base }));
    *node.direct_error.lock().unwrap() = Some(
        "Client network socket disconnected before secure TLS connection was established（已试: 198.20.0.19；DoH: 104.17.255.47, 172.66.199.198；共 3 个地址）"
            .to_string(),
    );
    let err = network
        .fetch(
            Transfer::from_json(json!({"url": format!("{upstream}/x"), "responseType": "text"})),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, "NETWORK");
    let node_suffix = err
        .message
        .find("（已试: 198.20.0.19；DoH: 104.17.255.47, 172.66.199.198；共 3 个地址）")
        .expect("Node 的 DoH 已试后缀应原样透传");
    let rust_suffix = err
        .message
        .find("（解析: 127.0.0.1:")
        .expect("Rust 的解析后缀应 append 在后");
    assert!(
        node_suffix < rust_suffix,
        "Node 后缀应在 Rust 后缀之前：{}",
        err.message
    );
    node_server.shutdown().await;
    upstream_server.abort();
}

#[tokio::test]
async fn dead_mirror_request_reaches_live_mirror() {
    // v0.0.81：请求 https://woggpan.xxooo.cf/... 时，mock Node 收
    // 到的是改写后的 https://woggpan.888484.xyz/...（路径/查询不
    // 变）；Node 只从 URL 取 hostname 做 SNI/Host，所以收到改写后
    // 的 URL 就是 SNI/Host 已改写的证明。两个 .cf 域名都改，非列表
    // 内 host 不动。
    let (node_base, node, node_server) = start_mock_node().await;
    let dir = tempfile::tempdir().unwrap();
    let network = node_network(dir.path(), Arc::new(FixedBootstrap { base: node_base }));
    let result = fetch_text(
        &network,
        "https://woggpan.xxooo.cf/vodshow/1--------1---.html?x=1",
    )
    .await
    .unwrap();
    // v0.0.82：mock 对分类页 URL 回带 module-item 的 HTML（探针
    // 的真实分类请求检查靠它判定“含列表内容”）。
    assert_eq!(
        result.json()["data"],
        json!("<html><body><div class=\"module-item\">x</div></body></html>")
    );
    let last = node.last.lock().unwrap().clone().unwrap();
    assert_eq!(
        last["url"],
        json!("https://woggpan.888484.xyz/vodshow/1--------1---.html?x=1"),
        "第一个 .cf 镜像应改写到 .xyz 镜像：{last}"
    );
    fetch_text(&network, "https://wogg.xxooo.cf/vodshow/2--------1---.html")
        .await
        .unwrap();
    let last = node.last.lock().unwrap().clone().unwrap();
    assert_eq!(
        last["url"],
        json!("https://woggpan.888484.xyz/vodshow/2--------1---.html"),
        "第二个 .cf 镜像应改写到同一 .xyz 镜像：{last}"
    );
    fetch_text(&network, "https://www.wogg.net/vodshow/2--------1---.html")
        .await
        .unwrap();
    let last = node.last.lock().unwrap().clone().unwrap();
    assert_eq!(
        last["url"],
        json!("https://www.wogg.net/vodshow/2--------1---.html"),
        "非列表内 host 不应改写：{last}"
    );
    node_server.shutdown().await;
}

#[tokio::test]
async fn dead_mirror_error_suffix_resolves_rewritten_host() {
    // v0.0.81：Node 报错时 Rust append 的（解析: …）后缀是改写后
    // host 的解析结果——本机解析器给 .xyz 与 .cf 各回不同的 FakeIP，
    // 用同一解析器现查两边作对照（不硬编码地址：FakeIP 的具体值
    // 随环境变）。
    let (node_base, node, node_server) = start_mock_node().await;
    let dir = tempfile::tempdir().unwrap();
    let network = node_network(dir.path(), Arc::new(FixedBootstrap { base: node_base }));
    *node.direct_error.lock().unwrap() = Some("mock node down".to_string());
    let err = fetch_text(
        &network,
        "https://woggpan.xxooo.cf/vodshow/1--------1---.html",
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, "NETWORK");
    let xyz = Url::parse("https://woggpan.888484.xyz/").unwrap();
    let xyz_addrs = network_policy::resolve_target_keep_order(&xyz, false)
        .await
        .expect("改写后的 host 应能解析");
    assert!(
        !xyz_addrs.is_empty(),
        "改写后的 host 应解析到地址"
    );
    assert!(
        err.message.contains(&xyz_addrs[0].to_string()),
        "解析后缀应是改写后 host 的地址（{}）：{}",
        xyz_addrs[0],
        err.message
    );
    // .cf 那边的地址不应出现在后缀里（若本机解析器对两边回了相同
    // 地址，或 .cf 已查无记录，则跳过否定断言）。
    let cf = Url::parse("https://woggpan.xxooo.cf/").unwrap();
    if let Ok(cf_addrs) = network_policy::resolve_target_keep_order(&cf, false).await {
        for a in &cf_addrs {
            if !xyz_addrs.contains(a) {
                assert!(
                    !err.message.contains(&a.to_string()),
                    "解析后缀不应含原 .cf host 的地址（{a}）：{}",
                    err.message
                );
            }
        }
    }
    node_server.shutdown().await;
}

#[tokio::test]
async fn dead_mirror_redirect_is_logged_to_ext_log() {
    // v0.0.81：每次改写在 ext.log 里记一行（requestId "net"，
    // from → to）；非列表内 host 不记重定向行。
    let (node_base, _node, node_server) = start_mock_node().await;
    let dir = tempfile::tempdir().unwrap();
    let network = node_network(dir.path(), Arc::new(FixedBootstrap { base: node_base }));
    let logs: crate::native::Logs = Default::default();
    network.set_logs(logs.clone());
    fetch_text(
        &network,
        "https://woggpan.xxooo.cf/vodshow/1--------1---.html",
    )
    .await
    .unwrap();
    let entries = logs.lock().unwrap().snapshot();
    let line = entries
        .iter()
        .find(|e| {
            e["requestId"] == "net"
                && e["text"]
                    .as_str()
                    .unwrap_or("")
                    .contains("失效镜像重定向")
        })
        .expect("应有一条 requestId 为 net 的重定向日志");
    assert_eq!(line["level"], "info");
    assert!(
        line["text"]
            .as_str()
            .unwrap()
            .contains("woggpan.xxooo.cf → woggpan.888484.xyz"),
        "日志应含 from → to：{}",
        line["text"]
    );
    fetch_text(&network, "https://www.wogg.net/vodshow/2--------1---.html")
        .await
        .unwrap();
    let entries = logs.lock().unwrap().snapshot();
    assert_eq!(
        entries
            .iter()
            .filter(|e| e["text"]
                .as_str()
                .unwrap_or("")
                .contains("失效镜像重定向"))
            .count(),
        1,
        "非列表内 host 不应再记重定向行"
    );
    node_server.shutdown().await;
}

#[tokio::test]
async fn diagnose_domain_check_does_not_rewrite_but_category_check_does() {
    // v0.0.81：探针的域名对照部分走 /diagnose，不经过 send_api_hop——
    // 请求体里的域名仍是原始 .cf 域名（探针就是要测原始域名）。
    // v0.0.82：探针新增的真实分类请求检查走正常 net.request 路径，
    // .cf 分类 URL 应在 send_api_hop 里改写、写一行 ext.log（那是
    // 检查的目的，不是探针“重写了对照”）。
    let (node_base, node, node_server) = start_mock_node().await;
    let dir = tempfile::tempdir().unwrap();
    let network = fallback_network(dir.path(), node_base, crate::proxy::Mode::Off, None);
    let logs: crate::native::Logs = Default::default();
    network.set_logs(logs.clone());
    let _report = network
        .diagnose(&CancellationToken::new(), "", "", "")
        .await
        .unwrap();
    let body = node
        .diagnose_last
        .lock()
        .unwrap()
        .clone()
        .expect("应调过 /diagnose");
    let domains: Vec<String> = serde_json::from_value(body["domains"].clone()).unwrap();
    assert!(
        domains.contains(&"woggpan.xxooo.cf".to_string()),
        "探针域名应保持原始 .cf：{domains:?}"
    );
    assert!(
        domains.contains(&"wogg.xxooo.cf".to_string()),
        "探针域名应保持原始 .cf：{domains:?}"
    );
    let entries = logs.lock().unwrap().snapshot();
    assert_eq!(
        entries
            .iter()
            .filter(|e| e["text"]
                .as_str()
                .unwrap_or("")
                .contains("失效镜像重定向"))
            .count(),
        1,
        "真实分类请求检查里 .cf 应改写一次、记一行；域名对照部分不应改写"
    );
    node_server.shutdown().await;
}

#[tokio::test]
async fn media_request_stays_on_wreq() {
    let (node_base, node, node_server) = start_mock_node().await;
    let (upstream, upstream_state, upstream_server) = start_mock_upstream().await;
    let dir = tempfile::tempdir().unwrap();
    let network = node_network(dir.path(), Arc::new(FixedBootstrap { base: node_base }));
    let cancel = CancellationToken::new();
    let response = network
        .request(
            &format!("{upstream}/movie.mp4"),
            Method::GET,
            HeaderMap::new(),
            None,
            false,
            false,
            Queue::Media,
            &cancel,
        )
        .await
        .unwrap();
    assert_eq!(response.text().await.unwrap(), "wreq-body");
    assert_eq!(node.calls.load(Ordering::Relaxed), 0);
    assert_eq!(upstream_state.hits.load(Ordering::Relaxed), 1);
    node_server.shutdown().await;
    upstream_server.abort();
}

#[tokio::test]
async fn unavailable_node_falls_back_to_wreq() {
    let (upstream, upstream_state, upstream_server) = start_mock_upstream().await;
    let dir = tempfile::tempdir().unwrap();
    let network = node_network(dir.path(), Arc::new(UnavailableNode));
    let result = network
        .fetch(
            Transfer::from_json(json!({"url": format!("{upstream}/x"), "responseType": "text"})),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result.json()["data"], json!("wreq-body"));
    assert_eq!(upstream_state.hits.load(Ordering::Relaxed), 1);
    upstream_server.abort();
}

#[tokio::test]
async fn node_restart_on_new_port_retries_request() {
    // 边车重启换端口：旧客户端地址已死，请求应重探 bootstrap 拿到新
    // 地址并重试一次，而不是把 Connect 失败直接抛给分类页。
    let (base_a, state_a, server_a) = start_mock_node().await;
    let (base_b, state_b, server_b) = start_mock_node().await;
    let (upstream, _, upstream_server) = start_mock_upstream().await;
    let dir = tempfile::tempdir().unwrap();
    let bootstrap = SwitchingBootstrap {
        bases: Arc::new(Mutex::new(VecDeque::from([base_a.clone(), base_b.clone()]))),
    };
    let network = node_network(dir.path(), Arc::new(bootstrap));
    let url = format!("{upstream}/x");

    // 第一调走 A。
    let first = network
        .fetch(
            Transfer::from_json(json!({"url": url.clone(), "responseType": "text"})),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(first.json()["data"], json!("node-body"));
    assert!(
        state_a.calls.load(Ordering::Relaxed) >= 1,
        "第一调没落在边车 A 上，重试断言会空过"
    );

    // 停掉 A，等到 TCP 彻底连不上（确定性：轮询而不是睡固定时长）。
    server_a.shutdown().await;
    let port_a: u16 = base_a.rsplit(':').next().unwrap().parse().unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        if std::net::TcpStream::connect(("127.0.0.1", port_a)).is_err() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "mock A 未在 3 秒内停下"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // 第二调：旧地址已死，应重探到 B 并重试成功。
    let second = network
        .fetch(
            Transfer::from_json(json!({"url": url, "responseType": "text"})),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(second.json()["data"], json!("node-body"));
    assert!(
        state_b.calls.load(Ordering::Relaxed) >= 1,
        "重试没落在边车 B 上"
    );
    server_b.shutdown().await;
    upstream_server.abort();
}

/// v0.0.77：WebView clearance 通行证只认 `cf_clearance`；过期不带；
/// `credentials: omit` 时不带；已有 Cookie 头时追加、不重复。
fn clearance_fixture() -> ClearanceCookie {
    ClearanceCookie {
        name: "cf_clearance".into(),
        value: "abc123".into(),
        domain: "127.0.0.1".into(),
        path: "/".into(),
        // mock 上游是 http：Secure 的通行证按浏览器规则不能发给 http
        // 目标，真站点的 cf_clearance 本来就是 Secure 的（配 https 用）。
        secure: false,
        http_only: false,
        expires_at: None,
    }
}

async fn fetch_cookie_seen_by_node(
    network: &Arc<Network>,
    node: &Arc<MockNode>,
    url: &str,
    extra: Value,
) -> Value {
    let mut req = json!({"url": url, "responseType": "text"});
    if let Some(o) = extra.as_object() {
        for (k, v) in o {
            req[k] = v.clone();
        }
    }
    network
        .fetch(Transfer::from_json(req), CancellationToken::new())
        .await
        .unwrap();
    node.last.lock().unwrap().clone().unwrap()["headers"]["cookie"].clone()
}

#[tokio::test]
async fn clearance_cookie_attached_to_node_requests() {
    let (node_base, node, node_server) = start_mock_node().await;
    let (upstream, _, upstream_server) = start_mock_upstream().await;
    let dir = tempfile::tempdir().unwrap();
    let network = node_network(dir.path(), Arc::new(FixedBootstrap { base: node_base }));
    network.set_clearance(clearance_fixture());
    let cookie =
        fetch_cookie_seen_by_node(&network, &node, &format!("{upstream}/x"), json!({})).await;
    assert_eq!(cookie, json!("cf_clearance=abc123"));
    node_server.shutdown().await;
    upstream_server.abort();
}

#[tokio::test]
async fn clearance_cookie_expired_or_bot_or_omitted_is_not_attached() {
    let (node_base, node, node_server) = start_mock_node().await;
    let (upstream, _, upstream_server) = start_mock_upstream().await;
    let dir = tempfile::tempdir().unwrap();
    let network = node_network(dir.path(), Arc::new(FixedBootstrap { base: node_base }));
    // 过期的通行证不带。
    let mut expired = clearance_fixture();
    expired.expires_at = Some(now().saturating_sub(1000));
    network.set_clearance(expired);
    let cookie =
        fetch_cookie_seen_by_node(&network, &node, &format!("{upstream}/x"), json!({})).await;
    assert_eq!(cookie, Value::Null);
    // `__cf_bm` 不收进内存存储（v0.0.75 的教训），所以也带不出去。
    let mut bot = clearance_fixture();
    bot.name = "__cf_bm".into();
    network.set_clearance(bot);
    let cookie =
        fetch_cookie_seen_by_node(&network, &node, &format!("{upstream}/x"), json!({})).await;
    assert_eq!(cookie, Value::Null);
    // Secure 的通行证不发给 http 目标（浏览器同款规则）。
    let mut secure = clearance_fixture();
    secure.secure = true;
    network.set_clearance(secure);
    let cookie =
        fetch_cookie_seen_by_node(&network, &node, &format!("{upstream}/x"), json!({})).await;
    assert_eq!(cookie, Value::Null);
    // credentials: omit 时连通行证也不带（与罐子同一门禁）。
    network.set_clearance(clearance_fixture());
    let cookie = fetch_cookie_seen_by_node(
        &network,
        &node,
        &format!("{upstream}/x"),
        json!({"credentials": "omit"}),
    )
    .await;
    assert_eq!(cookie, Value::Null);
    node_server.shutdown().await;
    upstream_server.abort();
}

#[tokio::test]
async fn clearance_cookie_appends_to_existing_cookie_header() {
    let (node_base, node, node_server) = start_mock_node().await;
    let (upstream, _, upstream_server) = start_mock_upstream().await;
    let dir = tempfile::tempdir().unwrap();
    let network = node_network(dir.path(), Arc::new(FixedBootstrap { base: node_base }));
    network.set_clearance(clearance_fixture());
    let cookie = fetch_cookie_seen_by_node(
        &network,
        &node,
        &format!("{upstream}/x"),
        json!({"headers": {"cookie": "session=xyz"}}),
    )
    .await;
    assert_eq!(cookie, json!("session=xyz; cf_clearance=abc123"));
    // 已有同名 Cookie 时不重复带。
    let cookie = fetch_cookie_seen_by_node(
        &network,
        &node,
        &format!("{upstream}/x"),
        json!({"headers": {"cookie": "cf_clearance=from-page"}}),
    )
    .await;
    assert_eq!(cookie, json!("cf_clearance=from-page"));
    node_server.shutdown().await;
    upstream_server.abort();
}

#[test]
fn looks_like_challenge_matches_cloudflare_markers() {
    // v0.0.77：与页面 `_getInner` 同一组标记（另加拦截页常见的两项）。
    assert!(looks_like_challenge("<title>Just a moment...</title>"));
    assert!(looks_like_challenge("Attention Required! | Cloudflare"));
    assert!(looks_like_challenge("cf-browser-verification"));
    assert!(looks_like_challenge("Access Denied"));
    assert!(looks_like_challenge("var _cf_chl_opt={cvId:'2'}"));
    // 正常页面不误判。
    assert!(!looks_like_challenge("<html><body>hello</body></html>"));
    assert!(!looks_like_challenge(""));
    assert!(!looks_like_challenge(r#"{"code":0,"data":[]}"#));
}

// ── v0.0.78：窄回落（只在跟随系统模式、只在 Api 的 Node 路径）──
// mock Node 按有没有 proxyUrl 分流（见 MockNode 的 proxy_status /
// direct_status / proxy_error / direct_error）；代理决议用
// `set_proxy_decision` 注入（不改进程全局代理状态）。

async fn fetch_text(network: &Arc<Network>, url: &str) -> Result<Transfer> {
    network
        .fetch(
            Transfer::from_json(json!({"url": url, "responseType": "text"})),
            CancellationToken::new(),
        )
        .await
}

fn fallback_network(
    dir: &std::path::Path,
    node_base: String,
    mode: crate::proxy::Mode,
    proxy_url: Option<&str>,
) -> Arc<Network> {
    let network = node_network(dir, Arc::new(FixedBootstrap { base: node_base }));
    network.set_proxy_decision(mode, proxy_url.map(str::to_string));
    network
}

#[tokio::test]
async fn proxy_502_falls_back_to_direct_once() {
    // 代理路 502 → 直连重试一次，成功正常返回；只调两次，首跳经
    // 代理、第二跳直连。
    let (node_base, node, node_server) = start_mock_node().await;
    let (upstream, _, upstream_server) = start_mock_upstream().await;
    let dir = tempfile::tempdir().unwrap();
    let network = fallback_network(
        dir.path(),
        node_base,
        crate::proxy::Mode::System,
        Some("http://127.0.0.1:7890"),
    );
    *node.proxy_status.lock().unwrap() = Some(502);
    let result = fetch_text(&network, &format!("{upstream}/x"))
        .await
        .unwrap();
    assert_eq!(result.json()["data"], json!("node-body"));
    assert_eq!(
        node.calls.load(Ordering::Relaxed),
        2,
        "代理 502 后应直连重试一次"
    );
    assert_eq!(
        *node.proxy_urls.lock().unwrap(),
        vec![Some("http://127.0.0.1:7890".to_string()), None],
        "首跳经代理，第二跳直连"
    );
    node_server.shutdown().await;
    upstream_server.abort();
}

#[tokio::test]
async fn proxy_502_and_direct_502_returns_first_error() {
    // 直连重试也 502 → 返回首跳（代理）的原始错误，转 UPSTREAM_ERROR，
    // 消息含状态 + 代理描述 + 解析地址 + 两次结果 + Cookie 名字。
    let (node_base, node, node_server) = start_mock_node().await;
    let (upstream, _, upstream_server) = start_mock_upstream().await;
    let dir = tempfile::tempdir().unwrap();
    let network = fallback_network(
        dir.path(),
        node_base,
        crate::proxy::Mode::System,
        Some("http://127.0.0.1:7890"),
    );
    *node.proxy_status.lock().unwrap() = Some(502);
    *node.direct_status.lock().unwrap() = Some(502);
    let err = fetch_text(&network, &format!("{upstream}/x"))
        .await
        .unwrap_err();
    assert_eq!(err.code, "UPSTREAM_ERROR");
    assert!(
        err.message.contains("上游返回 HTTP 502"),
        "应报原始状态：{}",
        err.message
    );
    assert!(
        err.message.contains("代理: 跟随系统 http://127.0.0.1:7890"),
        "应带代理描述：{}",
        err.message
    );
    assert!(
        err.message.contains("解析: 127.0.0.1:"),
        "应带解析地址：{}",
        err.message
    );
    assert!(
        err.message.contains("直连重试: 同样 502"),
        "应带两次尝试结果：{}",
        err.message
    );
    assert!(
        err.message.contains("Cookie: 未附带"),
        "应带 Cookie 附带情况：{}",
        err.message
    );
    assert_eq!(err.details.as_ref().unwrap()["status"], 502);
    assert_eq!(node.calls.load(Ordering::Relaxed), 2);
    assert_eq!(
        *node.proxy_urls.lock().unwrap(),
        vec![Some("http://127.0.0.1:7890".to_string()), None]
    );
    node_server.shutdown().await;
    upstream_server.abort();
}

#[tokio::test]
async fn proxy_503_returns_upstream_error_with_diagnosis() {
    // 503 同样转 UPSTREAM_ERROR（不是只报 502）。
    let (node_base, node, node_server) = start_mock_node().await;
    let (upstream, _, upstream_server) = start_mock_upstream().await;
    let dir = tempfile::tempdir().unwrap();
    let network = fallback_network(
        dir.path(),
        node_base,
        crate::proxy::Mode::System,
        Some("http://127.0.0.1:7890"),
    );
    *node.proxy_status.lock().unwrap() = Some(503);
    *node.direct_status.lock().unwrap() = Some(503);
    let err = fetch_text(&network, &format!("{upstream}/x"))
        .await
        .unwrap_err();
    assert_eq!(err.code, "UPSTREAM_ERROR");
    assert!(err.message.contains("上游返回 HTTP 503"), "{}", err.message);
    assert!(
        err.message.contains("直连重试: 同样 503"),
        "{}",
        err.message
    );
    assert_eq!(err.details.as_ref().unwrap()["status"], 503);
    assert_eq!(node.calls.load(Ordering::Relaxed), 2);
    node_server.shutdown().await;
    upstream_server.abort();
}

#[tokio::test]
async fn custom_proxy_never_falls_back() {
    // 自定义代理是用户显式路由，不擅自绕过：代理路 502 直接报错，
    // 只调一次，消息写明未回落的原因。
    let (node_base, node, node_server) = start_mock_node().await;
    let (upstream, _, upstream_server) = start_mock_upstream().await;
    let dir = tempfile::tempdir().unwrap();
    let network = fallback_network(
        dir.path(),
        node_base,
        crate::proxy::Mode::Custom,
        Some("http://127.0.0.1:7890"),
    );
    *node.proxy_status.lock().unwrap() = Some(502);
    let err = fetch_text(&network, &format!("{upstream}/x"))
        .await
        .unwrap_err();
    assert_eq!(err.code, "UPSTREAM_ERROR");
    assert!(
        err.message.contains("代理: 自定义 http://127.0.0.1:7890"),
        "{}",
        err.message
    );
    assert!(
        err.message.contains("直连重试: 未尝试（自定义代理）"),
        "{}",
        err.message
    );
    assert_eq!(node.calls.load(Ordering::Relaxed), 1, "自定义代理永不回落");
    node_server.shutdown().await;
    upstream_server.abort();
}

#[tokio::test]
async fn direct_mode_never_falls_back() {
    // 直连模式无代理可回落：502 直接报错，只调一次。
    let (node_base, node, node_server) = start_mock_node().await;
    let (upstream, _, upstream_server) = start_mock_upstream().await;
    let dir = tempfile::tempdir().unwrap();
    let network = fallback_network(dir.path(), node_base, crate::proxy::Mode::Off, None);
    *node.direct_status.lock().unwrap() = Some(502);
    let err = fetch_text(&network, &format!("{upstream}/x"))
        .await
        .unwrap_err();
    assert_eq!(err.code, "UPSTREAM_ERROR");
    assert!(err.message.contains("代理: 直连"), "{}", err.message);
    assert!(
        err.message.contains("直连重试: 未尝试（已直连）"),
        "{}",
        err.message
    );
    assert_eq!(node.calls.load(Ordering::Relaxed), 1, "直连模式无回落");
    node_server.shutdown().await;
    upstream_server.abort();
}

#[tokio::test]
async fn system_without_proxy_never_falls_back() {
    // 跟随系统但没解析出代理：502 直接报错，只调一次。
    let (node_base, node, node_server) = start_mock_node().await;
    let (upstream, _, upstream_server) = start_mock_upstream().await;
    let dir = tempfile::tempdir().unwrap();
    let network = fallback_network(dir.path(), node_base, crate::proxy::Mode::System, None);
    *node.direct_status.lock().unwrap() = Some(502);
    let err = fetch_text(&network, &format!("{upstream}/x"))
        .await
        .unwrap_err();
    assert_eq!(err.code, "UPSTREAM_ERROR");
    assert!(
        err.message.contains("代理: 跟随系统（无代理）"),
        "{}",
        err.message
    );
    assert!(
        err.message.contains("直连重试: 未尝试（无代理）"),
        "{}",
        err.message
    );
    assert_eq!(node.calls.load(Ordering::Relaxed), 1, "无代理时无回落");
    node_server.shutdown().await;
    upstream_server.abort();
}

#[tokio::test]
async fn proxy_handshake_error_falls_back_to_direct() {
    // 代理握手失败（CONNECT 被拒）→ 直连重试一次，成功正常返回。
    let (node_base, node, node_server) = start_mock_node().await;
    let (upstream, _, upstream_server) = start_mock_upstream().await;
    let dir = tempfile::tempdir().unwrap();
    let network = fallback_network(
        dir.path(),
        node_base,
        crate::proxy::Mode::System,
        Some("http://127.0.0.1:7890"),
    );
    *node.proxy_error.lock().unwrap() = Some("代理拒绝 CONNECT（HTTP 502）".to_string());
    let result = fetch_text(&network, &format!("{upstream}/x"))
        .await
        .unwrap();
    assert_eq!(result.json()["data"], json!("node-body"));
    assert_eq!(node.calls.load(Ordering::Relaxed), 2);
    assert_eq!(
        *node.proxy_urls.lock().unwrap(),
        vec![Some("http://127.0.0.1:7890".to_string()), None]
    );
    node_server.shutdown().await;
    upstream_server.abort();
}

#[tokio::test]
async fn socks_unreachable_error_falls_back_to_direct() {
    // SOCKS 连不上代理：host.js 文案是裸 ECONNREFUSED + 代理地址
    // （不含“代理”二字），靠代理 host 识别，同样回落。
    let (node_base, node, node_server) = start_mock_node().await;
    let (upstream, _, upstream_server) = start_mock_upstream().await;
    let dir = tempfile::tempdir().unwrap();
    let network = fallback_network(
        dir.path(),
        node_base,
        crate::proxy::Mode::System,
        Some("socks5://127.0.0.1:1080"),
    );
    *node.proxy_error.lock().unwrap() = Some("connect ECONNREFUSED 127.0.0.1:1080".to_string());
    let result = fetch_text(&network, &format!("{upstream}/x"))
        .await
        .unwrap();
    assert_eq!(result.json()["data"], json!("node-body"));
    assert_eq!(node.calls.load(Ordering::Relaxed), 2);
    node_server.shutdown().await;
    upstream_server.abort();
}

#[tokio::test]
async fn proxy_handshake_error_and_direct_failure_returns_first_error() {
    // 直连重试也失败（传输层）→ 返回首跳（代理）的原始错误，附
    // 两次尝试结果。
    let (node_base, node, node_server) = start_mock_node().await;
    let (upstream, _, upstream_server) = start_mock_upstream().await;
    let dir = tempfile::tempdir().unwrap();
    let network = fallback_network(
        dir.path(),
        node_base,
        crate::proxy::Mode::System,
        Some("http://127.0.0.1:7890"),
    );
    *node.proxy_error.lock().unwrap() = Some("代理拒绝 CONNECT（HTTP 502）".to_string());
    *node.direct_error.lock().unwrap() =
        Some("connect ECONNREFUSED 93.184.216.34:443".to_string());
    let err = fetch_text(&network, &format!("{upstream}/x"))
        .await
        .unwrap_err();
    assert_eq!(err.code, "NETWORK");
    assert!(
        err.message.contains("代理拒绝 CONNECT（HTTP 502）"),
        "应返回首跳原始错误：{}",
        err.message
    );
    assert!(
        err.message.contains("代理: 跟随系统 http://127.0.0.1:7890"),
        "{}",
        err.message
    );
    assert!(
        err.message
            .contains("直连重试: 失败：Node 边车请求失败：connect ECONNREFUSED 93.184.216.34:443"),
        "{}",
        err.message
    );
    assert!(
        err.message.contains("（解析: 127.0.0.1:"),
        "{}",
        err.message
    );
    assert_eq!(node.calls.load(Ordering::Relaxed), 2);
    node_server.shutdown().await;
    upstream_server.abort();
}

#[tokio::test]
async fn upstream_error_reports_cookie_names_only() {
    // Cookie 附带情况只报名字：值绝不进报错。
    let (node_base, node, node_server) = start_mock_node().await;
    let (upstream, _, upstream_server) = start_mock_upstream().await;
    let dir = tempfile::tempdir().unwrap();
    let network = fallback_network(
        dir.path(),
        node_base,
        crate::proxy::Mode::System,
        Some("http://127.0.0.1:7890"),
    );
    *node.proxy_status.lock().unwrap() = Some(502);
    *node.direct_status.lock().unwrap() = Some(502);
    let err = network
        .fetch(
            Transfer::from_json(json!({
                "url": format!("{upstream}/x"),
                "responseType": "text",
                "headers": {"cookie": "session=xyz; cf_clearance=abc"},
            })),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, "UPSTREAM_ERROR");
    assert!(
        err.message
            .contains("Cookie: 已附带（名字：session, cf_clearance）"),
        "{}",
        err.message
    );
    assert!(
        !err.message.contains("xyz"),
        "Cookie 值绝不进报错：{}",
        err.message
    );
    assert!(
        !err.message.contains("abc"),
        "Cookie 值绝不进报错：{}",
        err.message
    );
    node_server.shutdown().await;
    upstream_server.abort();
}

#[tokio::test]
async fn proxy_decision_is_logged_to_ext_log() {
    // 每次 net.request 的代理决议写进 ext.log（requestId "net"）。
    let (node_base, _node, node_server) = start_mock_node().await;
    let (upstream, _, upstream_server) = start_mock_upstream().await;
    let dir = tempfile::tempdir().unwrap();
    let network = fallback_network(
        dir.path(),
        node_base,
        crate::proxy::Mode::System,
        Some("http://127.0.0.1:7890"),
    );
    let logs: crate::native::Logs = Default::default();
    network.set_logs(logs.clone());
    fetch_text(&network, &format!("{upstream}/x"))
        .await
        .unwrap();
    let entries = logs.lock().unwrap().snapshot();
    let line = entries
        .iter()
        .find(|e| e["requestId"] == "net")
        .expect("应有一条 requestId 为 net 的日志");
    assert_eq!(line["level"], "info");
    let text = line["text"].as_str().unwrap();
    assert!(
        text.contains("跟随系统 http://127.0.0.1:7890"),
        "日志应含代理决议：{text}"
    );
    assert!(
        text.contains("目标：127.0.0.1"),
        "日志应含目标 host：{text}"
    );
    node_server.shutdown().await;
    upstream_server.abort();
}

#[tokio::test]
async fn network_diagnose_returns_report_and_logs_it_to_ext_log() {
    // v0.0.80：探针走 Node 边车 /diagnose，报告返回给调用方，同
    // 时写进 ext.log（requestId "net"）；请求体里带 4 个玩偶域名、
    // 代理描述与出口查询地址。
    // v0.0.82：报告顶部多了结论行与版本行，末尾多了两次真实分类
    // 请求检查（mock /request 对 vodshow URL 回带 module-item 的
    // HTML）；/diagnose 请求体改读 diagnose_last（两次分类请求
    // 后 `last` 已是分类请求）。
    let (node_base, node, node_server) = start_mock_node().await;
    let dir = tempfile::tempdir().unwrap();
    let network = fallback_network(dir.path(), node_base, crate::proxy::Mode::Off, None);
    let logs: crate::native::Logs = Default::default();
    network.set_logs(logs.clone());
    let report = network
        .diagnose(&CancellationToken::new(), "off", "", "0.0.82-test")
        .await
        .unwrap();
    assert!(report.starts_with("结论："), "{report}");
    assert!(report.contains("版本：0.0.82-test（失效镜像改写："), "{report}");
    assert!(report.contains("网络诊断 2026/11/24（代理：直连）"), "{report}");
    assert!(report.contains("出口 IP：1.2.3.4"), "{report}");
    assert!(report.contains("真实分类请求"), "{report}");
    assert!(
        report.contains("① https://woggpan.xxooo.cf/vodshow/1--------1---.html"),
        "{report}"
    );
    assert!(
        report.contains("② https://woggpan.888484.xyz/vodshow/1--------1---.html"),
        "{report}"
    );
    assert!(report.contains("含列表内容（module-item）"), "{report}");
    let body = node
        .diagnose_last
        .lock()
        .unwrap()
        .clone()
        .expect("应调过 /diagnose");
    let domains: Vec<String> = serde_json::from_value(body["domains"].clone()).unwrap();
    assert_eq!(
        domains,
        vec![
            "woggpan.888484.xyz",
            "www.wogg.net",
            "woggpan.xxooo.cf",
            "wogg.xxooo.cf"
        ]
    );
    assert_eq!(body["proxyDesc"].as_str().unwrap(), "直连");
    assert!(
        body["egressUrls"].as_array().unwrap().len() == 2,
        "应带 2 个出口查询地址：{body}"
    );
    let entries = logs.lock().unwrap().snapshot();
    // v0.0.82：真实分类请求检查的 .cf 改写行先写进 ext.log，报告
    // 行在后——找含“网络诊断”的那条，不取第一条 requestId 为 net 的。
    let line = entries
        .iter()
        .find(|e| {
            e["requestId"] == "net"
                && e["text"]
                    .as_str()
                    .is_some_and(|t| t.contains("网络诊断 "))
        })
        .expect("应有一条 requestId 为 net 的探针报告日志");
    assert_eq!(line["level"], "info");
    assert!(
        line["text"].as_str().unwrap().contains("网络诊断 "),
        "日志应含完整探针报告：{}",
        line["text"]
    );
    node_server.shutdown().await;
}

#[tokio::test]
async fn diagnose_uses_dialog_selected_proxy_mode_not_saved() {
    // v0.0.82：已保存的是直连（fallback_network Mode::Off），但
    // 对话框当前选的是自定义——探针的域名对照与两次真实分类请求
    // 都必须走自定义代理，不能静默用已保存的直连跑。
    let (node_base, node, node_server) = start_mock_node().await;
    let dir = tempfile::tempdir().unwrap();
    let network = fallback_network(dir.path(), node_base, crate::proxy::Mode::Off, None);
    let report = network
        .diagnose(
            &CancellationToken::new(),
            "custom",
            "http://127.0.0.1:7890",
            "0.0.82",
        )
        .await
        .unwrap();
    let body = node
        .diagnose_last
        .lock()
        .unwrap()
        .clone()
        .expect("应调过 /diagnose");
    assert_eq!(
        body["proxyUrl"].as_str().unwrap(),
        "http://127.0.0.1:7890",
        "域名对照应带对话框选中的自定义代理：{body}"
    );
    assert_eq!(
        body["proxyDesc"].as_str().unwrap(),
        "自定义 http://127.0.0.1:7890",
        "报告头代理描述应是自定义：{body}"
    );
    // 两次真实分类请求各调一次 /request，且都经自定义代理。
    assert_eq!(node.calls.load(Ordering::Relaxed), 2);
    assert_eq!(
        *node.proxy_urls.lock().unwrap(),
        vec![
            Some("http://127.0.0.1:7890".to_string()),
            Some("http://127.0.0.1:7890".to_string())
        ],
        "两次真实分类请求都应用对话框选中的代理，不用已保存的直连"
    );
    assert!(
        report.contains("版本：0.0.82（失效镜像改写："),
        "报告头应带传入的版本号：{report}"
    );
    node_server.shutdown().await;
}

#[tokio::test]
async fn diagnose_category_check_reports_rewrite_and_content() {
    // v0.0.82：两次真实分类请求走正常 net.request 路径——① .cf
    // 到达 Node 时已是改写后的 .xyz URL（SNI/Host 的证明），报告
    // 里写明改写、解析地址、HTTP 200、含列表内容；② 对照不改写。
    let (node_base, node, node_server) = start_mock_node().await;
    let dir = tempfile::tempdir().unwrap();
    let network = fallback_network(dir.path(), node_base, crate::proxy::Mode::Off, None);
    let report = network
        .diagnose(&CancellationToken::new(), "off", "", "0.0.82")
        .await
        .unwrap();
    assert_eq!(node.calls.load(Ordering::Relaxed), 2, "应正好发两次分类请求");
    assert_eq!(
        *node.urls.lock().unwrap(),
        vec![
            "https://woggpan.888484.xyz/vodshow/1--------1---.html".to_string(),
            "https://woggpan.888484.xyz/vodshow/1--------1---.html".to_string(),
        ],
        "① .cf 到达 Node 时应已改写为 .xyz（与 ② 同 URL）"
    );
    assert!(
        report.contains("改写：woggpan.xxooo.cf → woggpan.888484.xyz"),
        "① 应写明改写：{report}"
    );
    assert!(report.contains("改写：无"), "② 对照不应改写：{report}");
    assert!(report.contains("HTTP 200"), "{report}");
    assert!(report.contains("含列表内容（module-item）"), "{report}");
    assert!(
        report.contains("真实分类请求：.cf 经改写（→woggpan.888484.xyz）后成功（HTTP 200，含列表内容）"),
        "结论应点名改写后成功：{report}"
    );
    assert!(
        report.contains("对照 .xyz 成功（HTTP 200，含列表内容）"),
        "结论应有点名对照成功：{report}"
    );
    node_server.shutdown().await;
}

#[tokio::test]
async fn diagnose_category_check_reports_failure_verbatim() {
    // v0.0.82：两次真实分类请求都失败时，报告原样写错误消息
    //（含已试/DoH/解析后缀——真机上的后缀来自 Node，本用例只断
    // 言失败原样上报、不重试、不吞掉），结论点名失败。
    let (node_base, node, node_server) = start_mock_node().await;
    let dir = tempfile::tempdir().unwrap();
    let network = fallback_network(dir.path(), node_base, crate::proxy::Mode::Off, None);
    node.fail.store(true, Ordering::Relaxed);
    let report = network
        .diagnose(&CancellationToken::new(), "off", "", "0.0.82")
        .await
        .unwrap();
    assert_eq!(node.calls.load(Ordering::Relaxed), 2, "失败也不重试");
    assert!(
        report.contains("失败：Node 边车请求失败：mock node down"),
        "失败应原样写进报告：{report}"
    );
    assert!(
        report.contains("真实分类请求：.cf 经改写（→woggpan.888484.xyz）后失败（失败）"),
        "结论应点名改写后失败：{report}"
    );
    assert!(report.contains("对照 .xyz 失败（失败）"), "{report}");
    node_server.shutdown().await;
}

#[tokio::test]
async fn diagnose_rejects_invalid_custom_proxy() {
    // v0.0.82：自定义地址无效时直接报错，不静默换成已保存的设置跑。
    let (node_base, _node, node_server) = start_mock_node().await;
    let dir = tempfile::tempdir().unwrap();
    let network = fallback_network(dir.path(), node_base, crate::proxy::Mode::Off, None);
    let err = network
        .diagnose(&CancellationToken::new(), "custom", "not a url", "0.0.82")
        .await
        .unwrap_err();
    assert!(err.message.contains("代理地址"), "{}", err.message);
    let err = network
        .diagnose(&CancellationToken::new(), "bogus", "", "0.0.82")
        .await
        .unwrap_err();
    assert!(err.message.contains("代理模式"), "{}", err.message);
    node_server.shutdown().await;
}

fn diagnose_domain_summary(
    host: &str,
    sys: usize,
    doh: usize,
    doh_state: &str,
    edges: usize,
    tls_ok: usize,
    tls_fail: usize,
    http_ok: usize,
) -> crate::node::DiagnoseDomainSummary {
    crate::node::DiagnoseDomainSummary {
        host: host.to_string(),
        sys,
        doh,
        doh_state: doh_state.to_string(),
        edges,
        tls_ok,
        tls_fail,
        http_ok,
    }
}

fn diagnose_check(
    rewritten_to: Option<&str>,
    status: Option<u16>,
    has_content: bool,
    challenge: bool,
    error: Option<&str>,
) -> super::api::CategoryCheckResult {
    super::api::CategoryCheckResult {
        rewritten_to: rewritten_to.map(str::to_string),
        status,
        has_content,
        challenge,
        error: error.map(str::to_string),
    }
}

#[test]
fn diagnose_conclusion_all_live_and_rewrite_success() {
    // v0.0.82：结论行——.xyz/.net 全通、.cf TLS 全被断（DoH 无记录）、
    // 真实分类请求经改写后成功、对照成功。按 09:28 真机探针的数字拼。
    let summary = crate::node::DiagnoseSummary {
        domains: vec![
            diagnose_domain_summary("woggpan.888484.xyz", 5, 2, "ok", 7, 7, 0, 7),
            diagnose_domain_summary("www.wogg.net", 2, 2, "ok", 4, 4, 0, 4),
            diagnose_domain_summary("woggpan.xxooo.cf", 1, 0, "empty", 1, 0, 1, 0),
            diagnose_domain_summary("wogg.xxooo.cf", 1, 0, "empty", 1, 0, 1, 0),
        ],
    };
    let checks = vec![
        diagnose_check(Some("woggpan.888484.xyz"), Some(200), true, false, None),
        diagnose_check(None, Some(200), true, false, None),
    ];
    assert_eq!(
        super::Network::diagnose_conclusion(&summary, &checks),
        "woggpan.888484.xyz 全通（7/7 边缘）；www.wogg.net 全通（4/4 边缘）；woggpan.xxooo.cf TLS 全被断（DoH 无记录）；wogg.xxooo.cf TLS 全被断（DoH 无记录）；真实分类请求：.cf 经改写（→woggpan.888484.xyz）后成功（HTTP 200，含列表内容）；对照 .xyz 成功（HTTP 200，含列表内容）"
    );
}

#[test]
fn diagnose_conclusion_real_request_failure_names_reason() {
    // v0.0.82：结论行——改写后仍失败时点名原因（TLS 被断），对照成功。
    let summary = crate::node::DiagnoseSummary {
        domains: vec![diagnose_domain_summary(
            "woggpan.888484.xyz",
            5,
            2,
            "ok",
            7,
            7,
            0,
            7,
        )],
    };
    let checks = vec![
        diagnose_check(
            Some("woggpan.888484.xyz"),
            None,
            false,
            false,
            Some("Node 边车请求失败：Client network socket disconnected before secure TLS connection was established（已试: 198.20.0.19；DoH 无结果；仅 1 个地址，无处可换）（解析: 198.20.0.19:443；共 1 个地址）"),
        ),
        diagnose_check(None, Some(200), true, false, None),
    ];
    let conclusion = super::Network::diagnose_conclusion(&summary, &checks);
    assert!(
        conclusion.contains("真实分类请求：.cf 经改写（→woggpan.888484.xyz）后失败（TLS 被断）"),
        "{conclusion}"
    );
    assert!(
        conclusion.contains("对照 .xyz 成功（HTTP 200，含列表内容）"),
        "{conclusion}"
    );
}

#[test]
fn diagnose_domain_verdict_covers_no_address_partial_and_doh_failed() {
    // v0.0.82：结论分句——无可用地址、部分通、DoH 查询失败三种分支。
    assert_eq!(
        super::Network::diagnose_domain_verdict(&diagnose_domain_summary(
            "x.test", 0, 0, "empty", 0, 0, 0, 0
        )),
        "x.test 无可用地址"
    );
    assert_eq!(
        super::Network::diagnose_domain_verdict(&diagnose_domain_summary(
            "x.test", 2, 1, "ok", 3, 2, 0, 1
        )),
        "x.test 部分通（1/3 边缘）"
    );
    assert_eq!(
        super::Network::diagnose_domain_verdict(&diagnose_domain_summary(
            "x.test", 1, 0, "failed", 1, 0, 1, 0
        )),
        "x.test TLS 全被断（DoH 查询失败）"
    );
}

#[test]
fn rate_limit_cooldown_still_written_on_503_error() {
    // v0.0.78：503 转 UPSTREAM_ERROR 时，Retry-After 冷却仍要落
    // （协议 03 §3）；502 不写；429 也写。
    let dir = tempfile::tempdir().unwrap();
    let network = node_network(dir.path(), Arc::new(UnavailableNode));
    let mut headers = HeaderMap::new();
    headers.insert("retry-after", HeaderValue::from_static("60"));
    network.note_rate_limit("example.com", 503, &headers);
    assert!(
        network.cooldown.lock().unwrap().contains_key("example.com"),
        "503 + Retry-After 应落冷却"
    );
    network.note_rate_limit("example.org", 502, &headers);
    assert!(
        !network.cooldown.lock().unwrap().contains_key("example.org"),
        "502 不落冷却"
    );
    network.note_rate_limit("example.net", 429, &headers);
    assert!(
        network.cooldown.lock().unwrap().contains_key("example.net"),
        "429 + Retry-After 应落冷却"
    );
    // Retry-After 缺失或非法时不写。
    network.note_rate_limit("example.io", 503, &HeaderMap::new());
    assert!(
        !network.cooldown.lock().unwrap().contains_key("example.io"),
        "无 Retry-After 时不落冷却"
    );
}
