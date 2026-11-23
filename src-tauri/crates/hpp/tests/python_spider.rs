//! Python 蜘蛛宿主的端到端测试：真实 PluginManager + mock 边车。
//!
//! mock 边车只实现 HTTP 协议面（/health、/load、/call、/unload），
//! 返回写死的 TVBox JSON；真实的 CPython 边车由
//! src-tauri/python-sidecar/tests/test_sidecar.py 覆盖。
//! 这里验证 Rust 侧的分发、转换与错误映射。
use axum::{extract::Json, routing::{get, post}, Router};
use futures_util::future::BoxFuture;
use hghub_hpp::{
    media::MediaSessions,
    python::{PythonBootstrap, SidecarInfo},
    PluginManager,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::Arc,
};
use tokio::net::TcpListener;

const DEMO: &str = include_str!("../../../../examples/demo-spider.py");

/// 没有 getName 的蜘蛛：名字取文件词干。
const NAMELESS_SPIDER: &str = r#"
def homeContent(filter):
    return {"class": []}
def categoryContent(tid, pg, filter, extend):
    return {"list": []}
def detailContent(ids):
    return {"list": []}
def playerContent(flag, id, vipFlags):
    return {"parse": 0, "url": id, "header": {}}
"#;

#[derive(Clone, Copy, PartialEq)]
enum Behavior {
    Normal,
    /// /load 返回的方法面缺 detailContent + playerContent。
    MissingMethods,
    /// categoryContent 返回形状错误的 list。
    BadShape,
    /// 每次调用都报蜘蛛异常。
    SpiderError,
    /// /load 返回的方法面里没有 init：蜘蛛没实现 init 时宿主应跳过调用。
    NoInit,
}

struct MockSidecar {
    behavior: Behavior,
    /// /call 被调次数（按边车实例计）：断言重试确实换了边车。
    calls: std::sync::atomic::AtomicUsize,
}

impl MockSidecar {
    fn methods(&self) -> Vec<&'static str> {
        match self.behavior {
            Behavior::MissingMethods => vec![
                "init",
                "homeContent",
                "categoryContent",
                "searchContent",
            ],
            Behavior::NoInit => vec![
                "homeContent",
                "homeVideoContent",
                "categoryContent",
                "detailContent",
                "searchContent",
                "playerContent",
                "localProxy",
            ],
            _ => vec![
                "init",
                "homeContent",
                "homeVideoContent",
                "categoryContent",
                "detailContent",
                "searchContent",
                "playerContent",
                "localProxy",
            ],
        }
    }

    fn call(&self, method: &str) -> Result<Value, (u16, Value)> {
        self.calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        match self.behavior {
            Behavior::SpiderError => {
                return Err((
                    500,
                    json!({"error": "ValueError: boom（模拟蜘蛛异常）"}),
                ));
            }
            // 没实现 init 的蜘蛛：宿主根本不应调 init；调了就是回归。
            Behavior::NoInit if method == "init" => {
                return Err((500, json!({"error": "Method not found: init"})));
            }
            Behavior::BadShape if method == "categoryContent" => {
                return Ok(json!({"list": "不是数组", "page": 1}));
            }
            _ => {}
        }
        let vod = json!({
            "vod_id": "v1",
            "vod_name": "片一",
            "vod_pic": "https://example.invalid/c.jpg",
            "vod_remarks": "更新至1集",
            "vod_year": "2024",
            "vod_area": "大陆",
            "vod_actor": "演员一",
            "vod_director": "导演",
        });
        Ok(match method {
            "init" => json!(""),
            "homeContent" => json!({"class": [
                {"type_id": "movie", "type_name": "电影"},
                {"type_id": "tv", "type_name": "电视剧"},
            ]}),
            "homeVideoContent" => json!({"list": [vod.clone()]}),
            "categoryContent" => json!({
                "list": [vod.clone()], "page": 1, "pagecount": 1, "total": 1,
            }),
            "searchContent" => json!({
                "list": [vod.clone()], "page": 1, "pagecount": 1, "total": 1,
            }),
            "detailContent" => json!({"list": [{
                "vod_id": "v1",
                "vod_name": "片一",
                "vod_pic": "https://example.invalid/c.jpg",
                "vod_play_from": "线路A",
                "vod_play_url": "第1集$https://example.invalid/a.m3u8",
            }]}),
            "playerContent" => json!({
                "url": "https://example.invalid/a.m3u8", "parse": 0,
            }),
            "localProxy" => json!([200, "text/plain", "ok", {}]),
            _ => return Err((500, json!({"error": "Method not found"}))),
        })
    }
}

struct MockBootstrap {
    api_base: String,
}

impl PythonBootstrap for MockBootstrap {
    fn ensure_started(&self) -> BoxFuture<'static, hghub_hpp::Result<SidecarInfo>> {
        let api_base = self.api_base.clone();
        Box::pin(async move {
            Ok(SidecarInfo {
                api_base: api_base.clone(),
                proxy_base: format!("{api_base}/proxy"),
            })
        })
    }
}

async fn start_mock(behavior: Behavior) -> (String, Arc<MockSidecar>) {
    let (base, state, _server) = start_mock_handle(behavior).await;
    (base, state)
}

/// 可停的 mock 边车。shutdown 用优雅关闭：停监听的同时关掉已建立
/// 的 keep-alive 连接，模拟边车进程退出。只 abort 任务不够——
/// axum 给每个已建立连接单开任务，abort 监听任务后池里的旧连接照
/// 常用，客户端根本不会触发刷新，测不到换端口重试。
struct MockServer {
    notify: std::sync::Arc<tokio::sync::Notify>,
    task: tokio::task::JoinHandle<()>,
}

impl MockServer {
    async fn shutdown(self) {
        self.notify.notify_waiters();
        let mut task = self.task;
        if tokio::time::timeout(std::time::Duration::from_secs(3), &mut task)
            .await
            .is_err()
        {
            // 优雅关闭超时（不应发生）：强杀兜底。
            task.abort();
            let _ = task.await;
        }
    }
}

/// 带服务句柄的 mock 边车：调用方可以停掉它，模拟边车重启换端口。
async fn start_mock_handle(
    behavior: Behavior,
) -> (String, Arc<MockSidecar>, MockServer) {
    let state = Arc::new(MockSidecar {
        behavior,
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let health_state = state.clone();
    let load_state = state.clone();
    let call_state = state.clone();
    let app = Router::new()
        .route(
            "/health",
            get(move || {
                let state = health_state.clone();
                async move {
                    let _ = state;
                    Json(json!({"status": "ok", "proxyBase": "http://127.0.0.1:9978"}))
                }
            }),
        )
        .route(
            "/load",
            post(move |Json(_body): Json<Value>| {
                let state = load_state.clone();
                async move {
                    Json(json!({
                        "success": true,
                        "methods": state.methods(),
                        "style": "class",
                    }))
                }
            }),
        )
        .route(
            "/call",
            post(move |Json(body): Json<Value>| {
                let state = call_state.clone();
                async move {
                    let method = body["method"].as_str().unwrap_or("").to_string();
                    match state.call(&method) {
                        Ok(v) => (axum::http::StatusCode::OK, Json(v)),
                        Err((status, v)) => (
                            axum::http::StatusCode::from_u16(status).unwrap(),
                            Json(v),
                        ),
                    }
                }
            }),
        )
        .route("/unload", post(|Json(_body): Json<Value>| async { Json(json!({"ok": true})) }));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let notify = std::sync::Arc::new(tokio::sync::Notify::new());
    let signal = notify.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { signal.notified().await })
            .await
            .unwrap();
    });
    (base, state, MockServer { notify, task: server })
}

/// 每次 ensure_started 返回下一个地址（用完后固定在最后一个）：模拟
/// 边车重启后换了端口，info 文件已更新、旧客户端地址已失效。
struct SwitchingBootstrap {
    bases: Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
}

impl PythonBootstrap for SwitchingBootstrap {
    fn ensure_started(&self) -> BoxFuture<'static, hghub_hpp::Result<SidecarInfo>> {
        let bases = self.bases.clone();
        Box::pin(async move {
            let mut guard = bases.lock().unwrap();
            let api_base = if guard.len() > 1 {
                guard.pop_front().unwrap()
            } else {
                guard.front().unwrap().clone()
            };
            Ok(SidecarInfo {
                api_base: api_base.clone(),
                proxy_base: format!("{api_base}/proxy"),
            })
        })
    }
}

struct Documents(HashMap<String, Vec<u8>>);
impl hghub_hpp::manager::DocumentReader for Documents {
    fn read(&self, location: &str, limit: usize) -> hghub_hpp::Result<Vec<u8>> {
        let data = self
            .0
            .get(location)
            .ok_or_else(|| hghub_hpp::PluginError::new("NOT_FOUND", "Document not found"))?;
        Ok(data.iter().take(limit + 1).copied().collect())
    }
}

/// 建一个带 mock 边车的管理器，并预热回环服务（initialize 要读 base）。
/// 返回的 TempDir 必须在测试期间活着（插件文件落在里面）。
async fn manager_with_mock(
    behavior: Behavior,
) -> (Arc<PluginManager>, tempfile::TempDir) {
    manager_with_docs(
        behavior,
        HashMap::from([(
            "content://documents/demo-spider.py".to_string(),
            DEMO.as_bytes().to_vec(),
        )]),
    )
    .await
}

async fn manager_with_docs(
    behavior: Behavior,
    docs: HashMap<String, Vec<u8>>,
) -> (Arc<PluginManager>, tempfile::TempDir) {
    let (base, _state) = start_mock(behavior).await;
    let dir = tempfile::tempdir().unwrap();
    let bootstrap = MockBootstrap { api_base: base };
    let manager = Arc::new(
        PluginManager::with_mode(dir.path().into(), Some(false))
            .unwrap()
            .with_python_bootstrap(Arc::new(bootstrap))
            .with_document_reader(Arc::new(Documents(docs))),
    );
    MediaSessions::start(&manager).await.unwrap();
    (manager, dir)
}

async fn import_demo(manager: &Arc<PluginManager>) {
    let preview = manager
        .preview_import(
            vec!["content://documents/demo-spider.py".into()],
            "documents",
        )
        .await
        .unwrap();
    assert_eq!(preview.items.len(), 1);
    assert!(preview.items[0].error.is_none(), "{:?}", preview.items[0].error);
    assert_eq!(preview.items[0].id, "py.demo-spider");
    let keys: Vec<String> = preview.items.iter().map(|i| i.key.clone()).collect();
    manager
        .apply_import(&preview.token, keys, false, false, "test")
        .await
        .unwrap();
}

#[tokio::test]
async fn python_spider_import_and_calls() {
    let (manager, _dir) = manager_with_mock(Behavior::Normal).await;
    import_demo(&manager).await;

    let infos = manager.list().await;
    assert_eq!(infos.len(), 1);
    assert_eq!(infos[0].meta.id.as_str(), "py.demo-spider");
    assert_eq!(infos[0].meta.name.as_str(), "demo-spider");
    assert_eq!(infos[0].meta.runtime.as_str(), "python");
    assert_eq!(infos[0].status.as_str(), "ready");
    assert!(infos[0].methods.contains(&"catalog.getCategories".to_string()));
    assert!(infos[0].methods.contains(&"media.resolvePlayback".to_string()));
    assert!(!infos[0].methods.contains(&"live.getChannels".to_string()));

    let cats = manager
        .call("py.demo-spider", "catalog.getCategories", json!({}), "t1".into())
        .await
        .unwrap();
    assert_eq!(cats.as_array().unwrap().len(), 2);
    assert_eq!(cats[0]["id"], json!("movie"));

    let page = manager
        .call(
            "py.demo-spider",
            "catalog.list",
            json!({"categoryId": "movie", "page": 1}),
            "t2".into(),
        )
        .await
        .unwrap();
    assert_eq!(page["items"][0]["id"], json!("v1"));
    assert_eq!(page["next"], Value::Null);

    let sections = manager
        .call("py.demo-spider", "home.getSections", json!({}), "t3".into())
        .await
        .unwrap();
    assert_eq!(sections["items"].as_array().unwrap().len(), 1);
    assert_eq!(sections["items"][0]["items"][0]["id"], json!("v1"));

    let detail = manager
        .call(
            "py.demo-spider",
            "media.getDetail",
            json!({"id": "v1"}),
            "t4".into(),
        )
        .await
        .unwrap();
    assert_eq!(detail["playlists"][0]["id"], json!("线路A"));
    assert_eq!(detail["playlists"][0]["episodes"][0]["title"], json!("第1集"));

    let search = manager
        .call(
            "py.demo-spider",
            "search.search",
            json!({"query": "片一", "page": 1}),
            "t5".into(),
        )
        .await
        .unwrap();
    assert_eq!(search["items"][0]["id"], json!("v1"));

    let playback = manager
        .call(
            "py.demo-spider",
            "media.resolvePlayback",
            json!({"playlistId": "线路A", "episodeId": "https://example.invalid/a.m3u8"}),
            "t6".into(),
        )
        .await
        .unwrap();
    let streams = playback["streams"].as_array().unwrap();
    assert_eq!(streams.len(), 1);
    assert_eq!(streams[0]["url"], json!("https://example.invalid/a.m3u8"));
    assert_eq!(streams[0]["format"], json!("hls"));
}

#[tokio::test]
async fn python_import_percent_encoded_chinese_name() {
    let location =
        "content://documents/%E8%A5%BF%E7%93%9C%E8%A7%86%E9%A2%91%F0%9F%8D%89-1795392251000000000-0.py";
    let (manager, _dir) = manager_with_docs(
        Behavior::Normal,
        HashMap::from([(location.to_string(), NAMELESS_SPIDER.as_bytes().to_vec())]),
    )
    .await;
    let preview = manager
        .preview_import(vec![location.into()], "documents")
        .await
        .unwrap();
    assert_eq!(preview.items.len(), 1);
    assert!(preview.items[0].error.is_none(), "{:?}", preview.items[0].error);
    assert_eq!(preview.items[0].name, "西瓜视频🍉");
    let id = preview.items[0].id.clone();
    assert!(id.starts_with("py.spider-"), "unexpected id: {id}");
    let suffix = &id["py.spider-".len()..];
    assert_eq!(suffix.len(), 8);
    assert!(
        suffix.bytes().all(|b| b.is_ascii_hexdigit()),
        "unexpected id: {id}"
    );
    // 再预览一次：同一个文件得到同一个 id。
    let again = manager
        .preview_import(vec![location.into()], "documents")
        .await
        .unwrap();
    assert_eq!(again.items[0].id, id);
    // 落盘后名字与 id 不变。
    let keys: Vec<String> = preview.items.iter().map(|i| i.key.clone()).collect();
    manager
        .apply_import(&preview.token, keys, false, false, "test")
        .await
        .unwrap();
    let infos = manager.list().await;
    assert_eq!(infos.len(), 1);
    assert_eq!(infos[0].meta.name.as_str(), "西瓜视频🍉");
    assert_eq!(infos[0].meta.id.as_str(), id);

    // 换一个中文名：id 必须不同。
    let other =
        "content://documents/%E8%8D%89%E8%8E%93%E8%A7%86%E9%A2%91-1795392251000000000-0.py";
    let (manager, _dir) = manager_with_docs(
        Behavior::Normal,
        HashMap::from([(other.to_string(), NAMELESS_SPIDER.as_bytes().to_vec())]),
    )
    .await;
    let preview = manager
        .preview_import(vec![other.into()], "documents")
        .await
        .unwrap();
    assert!(preview.items[0].error.is_none(), "{:?}", preview.items[0].error);
    assert_eq!(preview.items[0].name, "草莓视频");
    assert_ne!(preview.items[0].id, id);
}

#[tokio::test]
async fn python_import_watermelon_id_is_frozen() {
    // 锁死 v0.0.59 的 id：同一文件重导必须得到同一个 id。id 由文件名
    // 词干“西瓜视频🍉”经 FNV-1a 派生；首页记住的片源（homeIds）、收藏
    // 与历史都按 id 存，v0.0.57→v0.0.59 换了词干就换了 id，首页才
    // 变空。这个值是写死的——现场算哈希锁不住规则变化；改
    // staged_name / inspect_python 让它跑红时，说明 id 派生输入变
    // 了，不能直接发版。
    let location =
        "content://documents/%E8%A5%BF%E7%93%9C%E8%A7%86%E9%A2%91%F0%9F%8D%89-1795392251000000000-0.py";
    let (manager, _dir) = manager_with_docs(
        Behavior::Normal,
        HashMap::from([(location.to_string(), NAMELESS_SPIDER.as_bytes().to_vec())]),
    )
    .await;
    let preview = manager
        .preview_import(vec![location.into()], "documents")
        .await
        .unwrap();
    assert_eq!(preview.items.len(), 1);
    assert!(preview.items[0].error.is_none(), "{:?}", preview.items[0].error);
    assert_eq!(preview.items[0].name, "西瓜视频🍉");
    assert_eq!(preview.items[0].id, "py.spider-d510afb9");
}

#[tokio::test]
async fn python_import_filename_beats_get_name() {
    // 演示源 getName() 声明的是“Python 演示源”，但文件名赢：名字按源文件
    // 自己的名字原样显示，含标点/emoji。
    let location =
        "content://documents/%E8%A5%BF%E7%93%9C%E8%A7%86%E9%A2%91%F0%9F%8D%89-1795392251000000000-0.py";
    let (manager, _dir) = manager_with_docs(
        Behavior::Normal,
        HashMap::from([(location.to_string(), DEMO.as_bytes().to_vec())]),
    )
    .await;
    let preview = manager
        .preview_import(vec![location.into()], "documents")
        .await
        .unwrap();
    assert_eq!(preview.items.len(), 1);
    assert!(preview.items[0].error.is_none(), "{:?}", preview.items[0].error);
    assert_eq!(preview.items[0].name, "西瓜视频🍉");
    assert!(preview.items[0].id.starts_with("py.spider-"));
}

#[tokio::test]
async fn python_spider_without_init_still_initializes() {
    let (manager, _dir) = manager_with_mock(Behavior::NoInit).await;
    import_demo(&manager).await;
    let infos = manager.list().await;
    assert_eq!(infos[0].status.as_str(), "ready");
    let cats = manager
        .call("py.demo-spider", "catalog.getCategories", json!({}), "t1".into())
        .await
        .unwrap();
    assert_eq!(cats.as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn python_spider_missing_required_methods_fails_clearly() {
    let (manager, _dir) = manager_with_mock(Behavior::MissingMethods).await;
    import_demo(&manager).await;
    let infos = manager.list().await;
    assert_eq!(infos[0].status.as_str(), "failed");
    let err = manager
        .call("py.demo-spider", "catalog.getCategories", json!({}), "t1".into())
        .await
        .unwrap_err();
    assert!(
        err.message.contains("detailContent") || err.message.contains("playerContent"),
        "unexpected error: {}",
        err.message
    );
}

#[tokio::test]
async fn python_spider_bad_shape_and_exception_are_internal_errors() {
    let (manager, _dir) = manager_with_mock(Behavior::BadShape).await;
    import_demo(&manager).await;
    let err = manager
        .call(
            "py.demo-spider",
            "catalog.list",
            json!({"categoryId": "movie", "page": 1}),
            "t1".into(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, "INTERNAL");

    let (manager, _dir) = manager_with_mock(Behavior::SpiderError).await;
    import_demo(&manager).await;
    let err = manager
        .call("py.demo-spider", "catalog.getCategories", json!({}), "t1".into())
        .await
        .unwrap_err();
    assert_eq!(err.code, "INTERNAL");
    assert!(err.message.contains("boom"), "unexpected error: {}", err.message);
}

#[tokio::test]
async fn python_import_rejects_non_spider_and_prefers_js() {
    let dir = tempfile::tempdir().unwrap();
    let manager = PluginManager::with_mode(dir.path().into(), Some(false))
        .unwrap()
        .with_document_reader(Arc::new(Documents(HashMap::from([
            (
                "content://documents/plain.py".to_string(),
                b"def hello():\n    return 1\n".to_vec(),
            ),
            (
                "content://documents/js-like.py".to_string(),
                b"export const meta = {id: 'x', name: 'X', version: '1.0.0', protocol: 1};\nexport default function(hg) { return {}; }\n".to_vec(),
            ),
        ]))));
    // 纯 Python 但没有蜘蛛面：预览即报错，错误要说明缺什么。
    let preview = manager
        .preview_import(vec!["content://documents/plain.py".into()], "documents")
        .await
        .unwrap();
    assert_eq!(preview.items.len(), 1);
    let err = preview.items[0].error.as_ref().expect("plain.py 应被拒绝");
    assert!(
        err.message.contains("蜘蛛"),
        "unexpected error: {}",
        err.message
    );
    // 含 export const meta 的文本一律按 JS 走（不会进 Python 派生分支）。
    let preview = manager
        .preview_import(vec!["content://documents/js-like.py".into()], "documents")
        .await
        .unwrap();
    assert!(preview.items[0].error.is_none(), "{:?}", preview.items[0].error);
    assert_eq!(preview.items[0].id, "x");

    // 内容判定本身（不依赖导入流程）：JS 优先、HTML 不认、演示蜘蛛认。
    let js_like = "export const meta = {id: 'x'};\nclass Spider { homeContent() {} }";
    assert!(!hghub_hpp::python::is_python(js_like));
    assert!(!hghub_hpp::python::is_python("<!doctype html><html><body>hi</body></html>"));
    assert!(hghub_hpp::python::is_python(DEMO));
}

#[tokio::test]
async fn python_sidecar_restart_on_new_port_retries_call() {
    // 边车重启换端口：旧客户端地址已死，调用应重探 bootstrap 拿到新
    // 地址并重试一次，而不是把 Connect 失败直接抛给分类页。模拟
    // v0.0.68 真机那次“Python 边车连接失败”——边车自愈重绑后 info
    // 文件已更新、Rust 侧旧客户端仍指旧端口。
    let (base_a, state_a, server_a) = start_mock_handle(Behavior::Normal).await;
    let (base_b, state_b, server_b) = start_mock_handle(Behavior::Normal).await;
    let dir = tempfile::tempdir().unwrap();
    let bootstrap = SwitchingBootstrap {
        bases: Arc::new(std::sync::Mutex::new(
            std::collections::VecDeque::from([base_a.clone(), base_b.clone()]),
        )),
    };
    let manager = Arc::new(
        PluginManager::with_mode(dir.path().into(), Some(false))
            .unwrap()
            .with_python_bootstrap(Arc::new(bootstrap))
            .with_document_reader(Arc::new(Documents(HashMap::from([(
                "content://documents/demo-spider.py".to_string(),
                DEMO.as_bytes().to_vec(),
            )])))),
    );
    MediaSessions::start(&manager).await.unwrap();
    import_demo(&manager).await;

    // 第一调走 A。
    let cats = manager
        .call("py.demo-spider", "catalog.getCategories", json!({}), "t1".into())
        .await
        .unwrap();
    assert_eq!(cats.as_array().unwrap().len(), 2);
    assert!(
        state_a
            .calls
            .load(std::sync::atomic::Ordering::Relaxed)
            >= 1,
        "第一调没落在边车 A 上，重试断言会空过"
    );

    // 停掉 A，等到 TCP 彻底连不上（确定性：轮询而不是睡固定时长）。
    server_a.shutdown().await;
    let port_a: u16 = base_a.rsplit(':').next().unwrap().parse().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        if std::net::TcpStream::connect(("127.0.0.1", port_a)).is_err() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "mock A 未在 3 秒内停下"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    // 第二调：旧地址已死，应重探到 B 并重试成功。
    let cats = manager
        .call("py.demo-spider", "catalog.getCategories", json!({}), "t2".into())
        .await
        .unwrap();
    assert_eq!(cats.as_array().unwrap().len(), 2);
    assert_eq!(cats[0]["id"], json!("movie"));
    assert!(
        state_b
            .calls
            .load(std::sync::atomic::Ordering::Relaxed)
            >= 1,
        "重试没落在边车 B 上"
    );
    server_b.shutdown().await;
}
