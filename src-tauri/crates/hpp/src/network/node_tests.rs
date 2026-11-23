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
    });
    let health = state.clone();
    let request = state.clone();
    let router = Router::new()
        .route(
            "/health",
            get(move || {
                let _ = health;
                async { Json(json!({"ok": true, "node": "v18.20.4"})) }
            }),
        )
        .route(
            "/request",
            post(move |Json(body): Json<Value>| {
                let state = request.clone();
                async move {
                    state.calls.fetch_add(1, Ordering::Relaxed);
                    *state.last.lock().unwrap() = Some(body);
                    if state.fail.load(Ordering::Relaxed) {
                        return (
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({"error": "mock node down", "code": "NETWORK"})),
                        );
                    }
                    (
                        axum::http::StatusCode::OK,
                        Json(json!({
                            "status": 200,
                            "headers": {"content-type": "text/plain", "x-node": "1"},
                            "bodyBase64": BASE64.encode("node-body"),
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
