//! 全清的响应缓存丢弃测试（v0.0.84）：`clear_response_cache` 丢掉全部
//! 条目并返回条数，Cookie 罐不动。
use super::*;
use serde_json::json;

fn network(dir: &std::path::Path) -> Arc<Network> {
    let meta = Meta::parse(
        json!({"id":"fixture.clear-cache","name":"ClearCache","protocol":1,"version":"1.0.0","allowInsecureHttp":true}),
    )
    .unwrap();
    Arc::new(
        Network::new(
            meta,
            dir.into(),
            Some(false),
            Default::default(),
            Arc::new(crate::node::UnavailableNode),
        )
        .unwrap(),
    )
}

#[test]
fn clear_response_cache_drops_all_entries_and_leaves_jar() {
    let dir = tempfile::tempdir().unwrap();
    let network = network(dir.path());
    assert_eq!(network.clear_response_cache(), 0);
    network.seed_response_cache("https://example.com/a", 10);
    network.seed_response_cache("https://example.com/b", 20);
    assert_eq!(network.clear_response_cache(), 2);
    assert_eq!(network.clear_response_cache(), 0);
    // Cookie 罐不受影响：种子一条 Cookie，清缓存后仍在。
    network
        .cookies(
            "set",
            json!({"url": "https://example.com/", "cookie": {"name": "sid", "value": "1", "domain": "example.com", "path": "/", "hostOnly": false, "secure": false, "httpOnly": false, "expiresAt": null}}),
        )
        .unwrap();
    network.seed_response_cache("https://example.com/c", 5);
    assert_eq!(network.clear_response_cache(), 1);
    let cookies: Value = network
        .cookies("get", json!({"url": "https://example.com/"}))
        .unwrap();
    assert_eq!(cookies.as_array().unwrap().len(), 1);
}
