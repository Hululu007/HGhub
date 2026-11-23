use hghub_hpp::{webhome, PluginManager};
use serde_json::{json, Value};
use std::sync::Arc;

const SOURCE: &str = include_str!("fixtures/webhome.js");
const ID: &str = "fixture.webhome";

async fn host() -> (tempfile::TempDir, Arc<PluginManager>) {
    let dir = tempfile::tempdir().unwrap();
    let manager = Arc::new(PluginManager::open(dir.path().into()).unwrap());
    let info = manager
        .import_code(SOURCE.into(), None, false)
        .await
        .unwrap();
    assert_eq!(info.status, "ready", "{:?}", info.error);
    assert_eq!(
        info.meta.webhome.as_ref().unwrap().home_page.as_deref(),
        Some("https://webhome.example/")
    );
    (dir, manager)
}

fn state() -> Value {
    json!({ "playing": false, "positionSec": 0, "durationSec": 0 })
}

async fn call(
    manager: &Arc<PluginManager>,
    method: &str,
    payload: Value,
) -> webhome::Outcome {
    webhome::dispatch(manager, ID, method, payload, state())
        .await
        .unwrap()
}

fn value(outcome: webhome::Outcome) -> Value {
    match outcome {
        webhome::Outcome::Value(v) => v,
        _ => panic!("expected a data outcome"),
    }
}

#[tokio::test]
async fn webhome_info_methods() {
    let (_dir, manager) = host().await;
    assert_eq!(
        value(call(&manager, "device.info", json!({})).await)["app"],
        "HGhub"
    );
    let config = value(call(&manager, "config.info", json!({})).await);
    assert_eq!(config["id"], ID);
    assert_eq!(config["url"], "https://webhome.example/");
    let site = value(call(&manager, "site.info", json!({})).await);
    assert_eq!(site["key"], ID);
    assert_eq!(site["type"], "webhome");
    let ext = value(call(&manager, "ext.info", json!({})).await);
    assert_eq!(ext["webhome"], 1);
}

#[tokio::test]
async fn webhome_search_and_vod() {
    let (_dir, manager) = host().await;
    let result = value(call(&manager, "search", json!({ "query": "虚构" })).await);
    assert_eq!(result["items"][0]["id"], "hit-1");
    match call(&manager, "player.playVod", json!({ "vodId": "one" })).await {
        webhome::Outcome::OpenVod(detail) => assert_eq!(detail["title"], "虚构作品"),
        _ => panic!("playVod should open the detail page"),
    }
    let cross = webhome::dispatch(
        &manager,
        ID,
        "player.playVod",
        json!({ "siteKey": "other.site", "vodId": "one" }),
        state(),
    )
    .await;
    assert!(cross.is_err(), "cross-site playVod must be rejected");
}

#[tokio::test]
async fn webhome_cache_round_trip() {
    let (_dir, manager) = host().await;
    let set = value(
        call(
            &manager,
            "cache.set",
            json!({ "k": "theme", "v": "dark" }),
        )
        .await,
    );
    assert_eq!(set["code"], 0);
    let got = value(call(&manager, "cache.get", json!({ "k": "theme" })).await);
    assert_eq!(got["found"], true);
    assert_eq!(got["value"], "dark");
    value(call(&manager, "cache.clear", json!({})).await);
    let gone = value(call(&manager, "cache.get", json!({ "k": "theme" })).await);
    assert_eq!(gone["found"], false);
}

#[tokio::test]
async fn webhome_sources_and_open() {
    let (_dir, manager) = host().await;
    let sources = value(call(&manager, "app.webhomeSources", json!({})).await);
    let list = sources.as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["id"], ID);
    assert_eq!(list[0]["name"], "虚构 WebHome 源");
    assert_eq!(list[0]["current"], true);
    assert_eq!(list[0]["ready"], true);
    match call(
        &manager,
        "app.openWebhome",
        json!({ "pluginId": "other.webhome" }),
    )
    .await
    {
        webhome::Outcome::OpenWebhome(id) => assert_eq!(id, "other.webhome"),
        _ => panic!("openWebhome should switch sources"),
    }
    let missing = webhome::dispatch(&manager, ID, "app.openWebhome", json!({}), state()).await;
    assert!(missing.is_err(), "openWebhome without pluginId must fail");
}

#[tokio::test]
async fn webhome_direct_play_and_unknown() {
    let (_dir, manager) = host().await;
    match call(
        &manager,
        "player.playUrl",
        json!({ "url": "https://media.example/clip.mp4", "title": "直链" }),
    )
    .await
    {
        webhome::Outcome::Play { media, title } => {
            assert_eq!(title.as_deref(), Some("直链"));
            assert_eq!(media.format, "progressive");
            assert!(media.url.contains("/hpp/"), "direct URL goes through the proxy");
            assert_eq!(media.provider_id, ID);
            manager.sessions.release(&media.session_id);
        }
        _ => panic!("playUrl should prepare a direct session"),
    }
    let unknown = webhome::dispatch(&manager, ID, "pan.play", json!({}), state()).await;
    assert!(unknown.is_err(), "pan is unsupported in v1");
    let missing = webhome::dispatch(&manager, "no.such", "device.info", json!({}), state()).await;
    assert!(missing.is_err(), "unknown plugin must fail");
}

#[tokio::test]
async fn local_page_served_over_loopback() {
    let (_dir, manager) = host().await;
    let files = tempfile::tempdir().unwrap();
    let html = "<!doctype html><html><head><title>本地</title></head><body>loopback</body></html>";
    let path = files.path().join("demo.html");
    std::fs::write(&path, html).unwrap();
    let preview = manager
        .preview_import(
            vec![path.to_string_lossy().into_owned()],
            "serve-local",
        )
        .await
        .unwrap();
    assert!(preview.items[0].error.is_none(), "{:?}", preview.items[0].error);
    let keys: Vec<String> = preview
        .items
        .iter()
        .filter(|i| i.error.is_none())
        .map(|i| i.key.clone())
        .collect();
    let report = manager
        .apply_import(&preview.token, keys, false, false, "install")
        .await
        .unwrap();
    assert!(report.items[0].plugin.is_some());
    // 本地页面的打开地址是当前回环地址，config.info 返回的也是它。
    let url = hghub_hpp::media::MediaSessions::webhome_page_url(&manager, "webhome.demo")
        .await
        .unwrap();
    assert!(url.starts_with("http://127.0.0.1:"));
    assert!(url.ends_with("/webhome/webhome.demo"));
    let config = value(
        webhome::dispatch(&manager, "webhome.demo", "config.info", json!({}), state())
            .await
            .unwrap(),
    );
    assert_eq!(config["url"], url);
    let response = wreq::get(&url).await.unwrap();
    assert_eq!(response.status(), 200);
    assert!(response.headers()["content-type"]
        .to_str()
        .unwrap()
        .contains("text/html"));
    assert_eq!(response.text().await.unwrap(), html);
    // 远程首页的插件没有本地页面，回环地址上查不到。
    assert_eq!(
        hghub_hpp::media::MediaSessions::webhome_page_url(&manager, ID)
            .await
            .unwrap(),
        "https://webhome.example/"
    );
    let missing = wreq::get(url.replace("webhome.demo", "webhome.nope"))
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);
}

#[tokio::test]
async fn webhome_net_request_get_without_body_reaches_network() {
    // 回归：页面 GET 不带 body 时，dispatch 不能把缺失的 body 写成 null，
    // 否则 fetch_once 会以 INVALID_ARGUMENT “GET/HEAD 不允许 body”拒绝，
    // 请求根本出不了宿主（v0.0.49 的“全部域名失败”就是这么来的）。
    // 用 .invalid 保留域名：能过目标校验、死在 DNS 层，刚好卡在 body 检查之后。
    let (_dir, manager) = host().await;
    let err = match webhome::dispatch(
        &manager,
        ID,
        "net.request",
        json!({
            "url": "https://example.invalid/",
            "method": "GET",
            "headers": {"User-Agent": "Mozilla/5.0"},
            "responseType": "text"
        }),
        state(),
    )
    .await
    {
        Ok(_) => panic!("expected the unresolvable host to fail"),
        Err(e) => e,
    };
    assert_eq!(err.code, "NETWORK", "{}", err.message);
}
