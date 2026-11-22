//! WebHome 页面桥（04 §14）：页面在独立 WebView 里运行，方法经宿主分发到
//! 绑定插件的已有能力（网络 / 存储 / 搜索 / 详情 / 直链会话），不经过插件
//! 运行时本身。页面只能操作绑定它的那个插件；跨站请求直接拒绝。

use crate::{
    media::PreparedMedia, wire::Transfer, PluginError, PluginManager, Result,
};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// 需要宿主窗口配合的动作：数据方法直接返回值，只有这几项要主窗口出面。
pub enum Outcome {
    /// 直接返回给页面的 JSON。
    Value(Value),
    /// 在主窗口播放器里播直链地址（已建好会话）。
    Play { media: PreparedMedia, title: Option<String> },
    /// 在主窗口打开作品详情页。
    OpenVod(Value),
    /// 在主窗口显示一句话提示。
    Toast(String),
    /// 播放器控制（play/pause/seek/stop），主窗口收到后转给当前播放器。
    PlayerControl(String, Option<f64>),
    /// 打开直播 / 收藏 / 设置入口，主窗口按现有导航处理。
    OpenLive,
    OpenKeep,
    OpenSetting,
    /// 切换 WebHome 窗口的关闭按钮显隐。
    SetToolbar(bool),
}

fn text<'a>(p: &'a Value, key: &str) -> Result<&'a str> {
    p[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| PluginError::invalid(format!("缺少 {key}")))
}

fn request_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// 方法名归一化：对象式与函数式别名都接受（04 §14）。
fn canonical(method: &str) -> &str {
    match method {
        "req" => "net.request",
        "res" => "net.resourceUrl",
        "play" => "player.playUrl",
        "vod" => "player.playVod",
        "vodInline" => "player.playVodInline",
        "search" => "app.search",
        "openVod" => "app.openVod",
        "openLive" => "app.openLive",
        "openKeep" => "app.openKeep",
        "openSetting" => "app.openSetting",
        "history" | "history_list" => "app.history",
        "config" | "config_info" => "config.info",
        "site" => "site.info",
        "device" => "device.info",
        "preloadArtwork" => "player.preloadArtwork",
        "ctrl" => "player.control",
        "stat" => "player.status",
        other => other,
    }
}

pub async fn dispatch(
    manager: &Arc<PluginManager>,
    plugin_id: &str,
    method: &str,
    payload: Value,
    player_state: Value,
) -> Result<Outcome> {
    let meta = manager.meta(plugin_id).await?;
    let method = canonical(method);
    let p = &payload;
    Ok(match method {
        "net.request" => {
            let network = manager.network(plugin_id).await?;
            let mut transfer_json = json!({
                "url": text(p, "url")?,
                "method": p["method"].as_str().unwrap_or("GET"),
                "headers": p["headers"].clone(),
                "responseType": p["responseType"].as_str().unwrap_or("text"),
                "timeoutMs": p["timeoutMs"].clone(),
                "redirect": p["redirect"].clone(),
                "maxBytes": p["maxBytes"].clone(),
                "cache": p["cache"].clone(),
                "retry": p["retry"].clone(),
                "credentials": p["credentials"].clone(),
            });
            // 页面 GET 不带 body：缺失的键不能写成 null，否则 fetch_once 会把
            // “body 键存在”当成带了请求体，GET 直接报 INVALID_ARGUMENT。
            if !p["body"].is_null() {
                transfer_json["body"] = p["body"].clone();
            }
            let transfer = Transfer::from_json(transfer_json);
            transfer.validate(8 * 1024 * 1024)?;
            Outcome::Value(network.fetch(transfer, CancellationToken::new()).await?.json())
        }
        "net.resourceUrl" => {
            // v1 不经代理取图：直接返回原地址，页面按自身 origin 加载。
            Outcome::Value(json!({ "url": text(p, "url")? }))
        }
        "player.playUrl" => {
            let url = text(p, "url")?;
            let headers = p["options"]["headers"].clone();
            let media = manager.prepare_direct(plugin_id, url, headers).await?;
            Outcome::Play {
                media,
                title: p["title"].as_str().map(String::from),
            }
        }
        "player.playVod" | "player.playVodInline" | "app.openVod" => {
            let vod_id = p["vodId"]
                .as_str()
                .or_else(|| p["id"].as_str())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| PluginError::invalid("缺少 vodId"))?;
            if let Some(site) = p["siteKey"].as_str() {
                if site != plugin_id {
                    return Err(PluginError::new(
                        "UNSUPPORTED",
                        "跨站 playVod 暂不支持",
                    ));
                }
            }
            let detail = manager
                .call(
                    plugin_id,
                    "media.getDetail",
                    json!({ "id": vod_id }),
                    request_id(),
                )
                .await?;
            Outcome::OpenVod(detail)
        }
        "player.preloadArtwork" => Outcome::Value(json!({ "code": 0 })),
        "player.control" => {
            let action = text(p, "action")?;
            if !matches!(action, "play" | "pause" | "toggle" | "seek" | "stop") {
                return Err(PluginError::invalid("未知播放控制动作"));
            }
            Outcome::PlayerControl(action.into(), p["seconds"].as_f64())
        }
        "player.status" => Outcome::Value(player_state),
        "app.search" => {
            let result = manager
                .call(
                    plugin_id,
                    "search.search",
                    json!({
                        "query": text(p, "query")?,
                        "cursor": p["cursor"].clone(),
                    }),
                    request_id(),
                )
                .await?;
            Outcome::Value(result)
        }
        "app.history" => {
            let list = manager.library.operation("list", &json!({}))?;
            let mut items: Vec<Value> = list
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|e| {
                    e["pluginId"].as_str() == Some(plugin_id)
                        && !e["lastProgress"].is_null()
                })
                .collect();
            items.sort_by_key(|e| {
                std::cmp::Reverse(e["lastProgress"]["at"].as_u64().unwrap_or(0))
            });
            items.truncate(50);
            Outcome::Value(json!({ "items": items }))
        }
        "cache.set" | "cache.get" | "cache.del" | "cache.clear" => {
            cache(manager, plugin_id, method, p).await.map(Outcome::Value)?
        }
        "config.info" => {
            // url 是页面的实际打开地址：本地页面是宿主现算的回环地址。
            let url = super::media::MediaSessions::webhome_page_url(manager, plugin_id)
                .await
                .ok();
            Outcome::Value(json!({
                "id": meta.id,
                "url": url,
                "desc": meta.description,
                "version": meta.version,
            }))
        }
        "site.info" => Outcome::Value(json!({
            "key": meta.id,
            "name": meta.name,
            "type": "webhome",
        })),
        "device.info" => Outcome::Value(json!({
            "platform": std::env::consts::OS,
            "app": "HGhub",
            "webhome": 1,
        })),
        "ext.info" => Outcome::Value(json!({
            "app": "HGhub",
            "webhome": 1,
            "protocol": "1.0.0-dev",
        })),
        "ext.log" => {
            let mut message = p["message"].as_str().unwrap_or("").to_string();
            if !p["data"].is_null() {
                message.push_str(&format!(" {}", p["data"]));
            }
            manager.webhome_log(plugin_id, &message).await?;
            Outcome::Value(json!({ "code": 0 }))
        }
        "ext.toast" => Outcome::Toast(text(p, "message")?.into()),
        "ui.setToolbar" => Outcome::SetToolbar(p["visible"].as_bool().unwrap_or(true)),
        "app.openLive" => Outcome::OpenLive,
        "app.openKeep" => Outcome::OpenKeep,
        "app.openSetting" => Outcome::OpenSetting,
        _ => {
            return Err(PluginError::new(
                "UNSUPPORTED",
                format!("WebHome 方法暂不支持：{method}"),
            ));
        }
    })
}

/// WebHome 缓存与绑定插件共用同一份存储，键名前缀 `webhome:`，与插件
/// 自身的键隔离；clear 只清这个前缀，不动插件的其他数据。
async fn cache(
    manager: &Arc<PluginManager>,
    plugin_id: &str,
    method: &str,
    p: &Value,
) -> Result<Value> {
    let store = manager.store(plugin_id).await?;
    // clear 不带 key：它按前缀清，不先取 key。
    if method == "cache.clear" {
        let keys = store
            .operation(
                "keys",
                &Transfer::from_json(json!({ "prefix": "webhome:" })),
            )?
            .json();
        if let Some(keys) = keys.as_array() {
            for k in keys {
                if let Some(k) = k.as_str() {
                    store.operation(
                        "delete",
                        &Transfer::from_json(json!({ "key": k })),
                    )?;
                }
            }
        }
        return Ok(json!({ "code": 0 }));
    }
    let key = format!("webhome:{}", text(p, "k")?);
    if key.len() > 256 {
        return Err(PluginError::invalid("缓存 key 过长"));
    }
    match method {
        "cache.set" => {
            store.operation(
                "set",
                &Transfer::from_json(json!({ "key": key, "value": p["v"].clone() })),
            )?;
            Ok(json!({ "code": 0 }))
        }
        "cache.get" => Ok(store
            .operation("get", &Transfer::from_json(json!({ "key": key })))?
            .json()),
        "cache.del" => {
            store.operation("delete", &Transfer::from_json(json!({ "key": key })))?;
            Ok(json!({ "code": 0 }))
        }
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_aliases() {
        assert_eq!(canonical("req"), "net.request");
        assert_eq!(canonical("play"), "player.playUrl");
        assert_eq!(canonical("vod"), "player.playVod");
        assert_eq!(canonical("search"), "app.search");
        assert_eq!(canonical("history_list"), "app.history");
        assert_eq!(canonical("config"), "config.info");
        assert_eq!(canonical("ctrl"), "player.control");
        assert_eq!(canonical("net.request"), "net.request");
    }
}
