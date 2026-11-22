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
        info.meta.webhome.as_ref().unwrap().home_page,
        "https://webhome.example/"
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
