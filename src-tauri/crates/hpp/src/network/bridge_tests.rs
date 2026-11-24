//! WebView→引擎 Cookie 桥测试（v0.0.86）：`merge_webview_cookies`
//! 把 WebView 里的 Cookie 合并进普通 Cookie 罐（落盘、与引擎收到
//! 的 Set-Cookie 同等待遇），`log_cookie_bridge` 把桥了几个、名字
//! 是什么写进 ext.log（只报名字，不报值）。
use super::*;
use serde_json::json;

fn network(dir: &std::path::Path) -> Arc<Network> {
    let meta = Meta::parse(
        json!({"id":"fixture.cookie-bridge","name":"CookieBridge","protocol":1,"version":"1.0.0","allowInsecureHttp":true}),
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

fn cookie(
    name: &str,
    value: &str,
    domain: &str,
    host_only: bool,
    expires_at: Option<u64>,
) -> WebViewCookie {
    WebViewCookie {
        name: name.into(),
        value: value.into(),
        domain: domain.into(),
        path: "/".into(),
        host_only,
        secure: false,
        http_only: false,
        expires_at,
    }
}

fn get_cookies(network: &Network, url: &str) -> Value {
    network.cookies("get", json!({"url": url})).unwrap()
}

#[test]
fn merge_upserts_into_ordinary_jar_and_persists() {
    let dir = tempfile::tempdir().unwrap();
    let net = network(dir.path());
    let merged = net.merge_webview_cookies(vec![cookie(
        "server_session",
        "abc",
        "example.com",
        true,
        None,
    )]);
    assert_eq!(merged, vec!["server_session".to_string()]);
    let cookies = get_cookies(&net, "https://example.com/");
    assert_eq!(cookies.as_array().unwrap().len(), 1);
    assert_eq!(cookies[0]["name"], "server_session");
    assert_eq!(cookies[0]["value"], "abc");
    // 落盘：重建 Network 读回同一条。
    drop(net);
    let rebuilt = network(dir.path());
    let cookies = get_cookies(&rebuilt, "https://example.com/");
    assert_eq!(cookies.as_array().unwrap().len(), 1);
    assert_eq!(cookies[0]["value"], "abc");
}

#[test]
fn merge_overwrites_same_name_domain_path() {
    let dir = tempfile::tempdir().unwrap();
    let network = network(dir.path());
    network.merge_webview_cookies(vec![cookie("sid", "1", "example.com", true, None)]);
    let merged = network.merge_webview_cookies(vec![cookie("sid", "2", "example.com", true, None)]);
    assert_eq!(merged, vec!["sid".to_string()]);
    let cookies = get_cookies(&network, "https://example.com/");
    assert_eq!(cookies.as_array().unwrap().len(), 1);
    assert_eq!(cookies[0]["value"], "2");
}

#[test]
fn merge_skips_bot_management_cookie() {
    let dir = tempfile::tempdir().unwrap();
    let network = network(dir.path());
    let merged = network.merge_webview_cookies(vec![
        cookie("__cf_bm", "x", "example.com", true, None),
        cookie("__CF_BM", "y", "example.com", true, None),
        cookie("sid", "1", "example.com", true, None),
    ]);
    assert_eq!(merged, vec!["sid".to_string()]);
    let cookies = get_cookies(&network, "https://example.com/");
    assert_eq!(cookies.as_array().unwrap().len(), 1);
}

#[test]
fn merge_skips_expired_and_empty() {
    let dir = tempfile::tempdir().unwrap();
    let network = network(dir.path());
    let past = now().saturating_sub(1000);
    let merged = network.merge_webview_cookies(vec![
        cookie("old", "1", "example.com", true, Some(past)),
        cookie("", "1", "example.com", true, None),
        cookie("novalue", "", "example.com", true, None),
        cookie("nodomain", "1", "", true, None),
        cookie("ok", "1", "example.com", true, None),
    ]);
    assert_eq!(merged, vec!["ok".to_string()]);
    let cookies = get_cookies(&network, "https://example.com/");
    assert_eq!(cookies.as_array().unwrap().len(), 1);
}

#[test]
fn merge_host_only_cookie_stays_on_its_host() {
    let dir = tempfile::tempdir().unwrap();
    let network = network(dir.path());
    network.merge_webview_cookies(vec![cookie("sid", "1", "example.com", true, None)]);
    assert_eq!(
        get_cookies(&network, "https://example.com/")
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        get_cookies(&network, "https://sub.example.com/")
            .as_array()
            .unwrap()
            .len(),
        0
    );
    // 域名 Cookie（非 host-only）则发给子域。
    network.merge_webview_cookies(vec![cookie("dom", "1", "example.com", false, None)]);
    assert_eq!(
        get_cookies(&network, "https://sub.example.com/")
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn log_cookie_bridge_writes_ext_log_line() {
    let dir = tempfile::tempdir().unwrap();
    let network = network(dir.path());
    let logs: crate::native::Logs = Default::default();
    network.set_logs(logs.clone());
    network.log_cookie_bridge(&["server_session".to_string(), "sid".to_string()]);
    let entries = logs.lock().unwrap().snapshot();
    let line = entries
        .iter()
        .find(|e| e["requestId"] == "net")
        .expect("应有一条 requestId 为 net 的桥接日志");
    assert_eq!(line["level"], "info");
    let text = line["text"].as_str().unwrap();
    assert!(
        text.contains("net.request Cookie 桥接：2 个（名字：server_session, sid）"),
        "日志应含桥接计数与名字：{text}"
    );
    // 0 个也记一行（桥跑了、窗口里没有该 URL 的 Cookie）。
    network.log_cookie_bridge(&[]);
    let entries = logs.lock().unwrap().snapshot();
    assert_eq!(entries.len(), 2);
    assert!(
        entries[1]["text"]
            .as_str()
            .unwrap()
            .contains("net.request Cookie 桥接：0 个"),
        "0 个也应记一行：{}",
        entries[1]["text"]
    );
}
