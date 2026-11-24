//! 响应日志测试（v0.0.87）：每次 Node 引擎请求的响应写进 ext.log
//!（HTTP 状态 + Set-Cookie 名字 + 响应体标记），只记布尔标记、
//! 只报 Cookie 名字，绝不记完整响应体与 Cookie 值。
use super::*;
use crate::node::NodeResponse;
use serde_json::json;

fn network(dir: &std::path::Path) -> Arc<Network> {
    let meta = Meta::parse(
        json!({"id":"fixture.response-log","name":"ResponseLog","protocol":1,"version":"1.0.0","allowInsecureHttp":true}),
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

fn response(status: u16, set_cookies: &[&str], body: &[u8]) -> NodeResponse {
    let mut headers = HeaderMap::new();
    for c in set_cookies {
        headers.append(
            "set-cookie",
            HeaderValue::from_str(c).unwrap(),
        );
    }
    NodeResponse {
        status,
        headers,
        body: body.to_vec(),
        doh: None,
    }
}

fn log_texts(logs: &crate::native::Logs) -> Vec<String> {
    logs.lock()
        .unwrap()
        .snapshot()
        .iter()
        .map(|e| e["text"].as_str().unwrap_or("").to_string())
        .collect()
}

#[test]
fn response_markers_detects_list_verify_and_challenge() {
    assert_eq!(
        response_markers(b"<div class=\"module-item\">x</div>"),
        (true, false, false)
    );
    assert_eq!(
        response_markers("请验证密码".as_bytes()),
        (false, true, false)
    );
    assert_eq!(
        response_markers("当前访问人数过多".as_bytes()),
        (false, true, false)
    );
    assert_eq!(
        response_markers(b"Just A Moment, please wait"),
        (false, false, true)
    );
    assert_eq!(response_markers(b"hello world"), (false, false, false));
    assert_eq!(response_markers(b""), (false, false, false));
}

#[test]
fn log_response_writes_ext_log_line() {
    let dir = tempfile::tempdir().unwrap();
    let network = network(dir.path());
    let logs: crate::native::Logs = Default::default();
    network.set_logs(logs.clone());
    let resp = response(
        200,
        &["a=1; Path=/", "b=2; Secure"],
        b"<div class=\"module-item\">x</div>",
    );
    network.log_response(&resp, "");
    let texts = log_texts(&logs);
    assert_eq!(texts.len(), 1);
    assert!(
        texts[0].contains(
            "net.request 响应：HTTP 200；Set-Cookie：a, b；内容：module-item=是，验证页=否，just-a-moment=否"
        ),
        "响应行应含状态、Set-Cookie 名字与体标记：{}",
        texts[0]
    );
    // 无 Set-Cookie 写“无”；直连重试那跳带标记。
    let retry = response(500, &[], "请验证密码".as_bytes());
    network.log_response(&retry, "直连重试");
    let texts = log_texts(&logs);
    assert_eq!(texts.len(), 2);
    assert!(
        texts[1].contains(
            "net.request 响应（直连重试）：HTTP 500；Set-Cookie：无；内容：module-item=否，验证页=是，just-a-moment=否"
        ),
        "重试响应行应带标记与“无”：{}",
        texts[1]
    );
}
