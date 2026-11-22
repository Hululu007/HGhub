//! TVBox 蜘蛛返回值 → HPP 形状的纯函数转换。
//!
//! 不依赖解释器：输入是边车 /call 返回的 JSON，输出直接复用现有的
//! `model::validate_result` 校验。单个坏条目丢弃并告警（02 §9），
//! 不让整页失败；详情无播放列表、播放无地址时报明错。
use crate::{PluginError, Result};
use serde_json::{json, Map, Value};

/// 上游明确失败：蜘蛛返回的 dict 带非 0 的 `code` 且没有 `list`。
/// 蜘蛛自己把失败吞成空列表时宿主无法区分，按正常空页处理。
pub fn upstream_error(v: &Value) -> Option<PluginError> {
    let code = v.get("code")?.as_i64()?;
    if code == 0 || v.get("list").is_some() {
        return None;
    }
    let message = v
        .get("message")
        .or_else(|| v.get("msg"))
        .and_then(Value::as_str)
        .unwrap_or("上游返回失败");
    let message: String = message.chars().take(200).collect();
    Some(PluginError::new(
        "UPSTREAM_ERROR",
        format!("上游返回失败（code {code}）：{message}"),
    ))
}

fn page_number(v: &Value, key: &str, default: u64) -> u64 {
    match v.get(key) {
        Some(Value::Number(n)) => n.as_u64().unwrap_or(default),
        Some(Value::String(s)) => s.parse::<u64>().unwrap_or(default),
        _ => default,
    }
}

fn non_empty_str(v: &Value, key: &str) -> Option<String> {
    let s = v.get(key)?.as_str()?.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// `vod_*` 条目 → MediaItem。缺 `vod_id` 或 `vod_name` 的坏条目返回 None。
pub fn vod_to_item(vod: &Value) -> Option<Value> {
    let id = non_empty_str(vod, "vod_id")?;
    let title = non_empty_str(vod, "vod_name")?;
    let mut item = Map::new();
    item.insert("id".into(), json!(id));
    item.insert("title".into(), json!(title));
    if let Some(pic) = non_empty_str(vod, "vod_pic") {
        item.insert("cover".into(), json!(pic));
    }
    // 短备注作徽标（≤8 字），长备注进简介。
    if let Some(remarks) = non_empty_str(vod, "vod_remarks") {
        if remarks.chars().count() <= 8 {
            item.insert("badge".into(), json!(remarks));
        } else {
            item.insert("description".into(), json!(remarks));
        }
    }
    if let Some(content) = non_empty_str(vod, "vod_content") {
        item.insert("description".into(), json!(content));
    }
    if let Some(year) = vod.get("vod_year").and_then(Value::as_str) {
        if let Ok(y) = year.trim().parse::<i64>() {
            item.insert("year".into(), json!(y));
        }
    }
    let mut tags: Vec<String> = vec![];
    for key in ["vod_area", "vod_lang", "vod_class"] {
        if let Some(v) = non_empty_str(vod, key) {
            tags.push(v);
        }
    }
    if !tags.is_empty() {
        item.insert("tags".into(), json!(tags));
    }
    if let Some(n) = vod.get("vod_episode_count").and_then(Value::as_u64) {
        item.insert("episodeCount".into(), json!(n));
    }
    Some(Value::Object(item))
}

fn items_page(v: &Value) -> Result<(Vec<Value>, u64, u64, u64)> {
    if let Some(e) = upstream_error(v) {
        return Err(e);
    }
    let list = v
        .get("list")
        .and_then(Value::as_array)
        .ok_or_else(|| PluginError::new("INTERNAL", "蜘蛛返回缺少 list"))?;
    let items: Vec<Value> = list.iter().filter_map(vod_to_item).collect();
    let page = page_number(v, "page", 1).max(1);
    let pagecount = page_number(v, "pagecount", 1).max(1);
    let total = page_number(
        v,
        "total",
        page_number(v, "limit", 0).saturating_mul(pagecount),
    );
    Ok((items, page, pagecount, total))
}

/// 分类/搜索/信息流的分页结果 → `Page<MediaItem>`。
/// 游标就是页码字符串：`next = page < pagecount ? str(page+1) : null`。
pub fn page(v: &Value) -> Result<Value> {
    let (items, page, pagecount, total) = items_page(v)?;
    let next = if page < pagecount {
        Value::String((page + 1).to_string())
    } else {
        Value::Null
    };
    Ok(json!({"items": items, "next": next, "total": total}))
}

/// `homeContent` 的 `class[]` → `Category[]`。
pub fn categories(v: &Value) -> Result<Value> {
    if let Some(e) = upstream_error(v) {
        return Err(e);
    }
    let class = v
        .get("class")
        .and_then(Value::as_array)
        .ok_or_else(|| PluginError::new("INTERNAL", "蜘蛛返回缺少 class"))?;
    let mut out = vec![];
    for c in class {
        let Some(id) = non_empty_str(c, "type_id") else {
            continue;
        };
        let Some(label) = non_empty_str(c, "type_name") else {
            continue;
        };
        out.push(json!({"id": id, "label": label}));
    }
    Ok(Value::Array(out))
}

/// 首页：`homeVideoContent` / `homeContent` 的 `list[]` 包成一个 Section，
/// 再装进 `Page<Section>`。
/// 把 homeContent/homeVideoContent 的 list 包成一个 section。
/// home.getSections 返回 `Page<Section>`：items 里放 section，section
/// 里再放作品条目（见 protocol.ts）。
pub fn sections(v: &Value, section_id: &str, layout: &str) -> Result<Value> {
    let (items, _page, _pagecount, total) = items_page(v)?;
    Ok(json!({
        "items": [{
            "id": section_id,
            "layout": layout,
            "items": items,
        }],
        "next": null,
        "total": total,
    }))
}

fn parse_episodes(play_url: &str) -> Vec<Value> {
    let mut episodes = vec![];
    for (i, part) in play_url.split('#').enumerate() {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (label, id) = match part.split_once('$') {
            Some((l, r)) => (l.trim(), r.trim()),
            None => (part, part),
        };
        if id.is_empty() {
            continue;
        }
        episodes.push(json!({
            "id": id,
            "index": i + 1,
            "title": if label.is_empty() { format!("第 {} 集", i + 1) } else { label.to_string() },
        }));
    }
    episodes
}

/// `detailContent` 的 `{list:[vod]}` → MediaDetail。
/// `vod_play_from` 以 `$$$` 切多线路，`vod_play_url` 以 `#` 切集、
/// 以 `$` 切 `标题$地址`。播放列表 id 取线路名（playerContent 的 flag）。
pub fn detail(v: &Value) -> Result<Value> {
    if let Some(e) = upstream_error(v) {
        return Err(e);
    }
    let list = v
        .get("list")
        .and_then(Value::as_array)
        .ok_or_else(|| PluginError::new("INTERNAL", "蜘蛛返回缺少 list"))?;
    let vod = list
        .first()
        .ok_or_else(|| PluginError::new("NOT_FOUND", "未找到该作品"))?;
    let mut item = vod_to_item(vod)
        .ok_or_else(|| PluginError::new("INTERNAL", "详情条目缺少 vod_id 或 vod_name"))?;
    let play_from = non_empty_str(vod, "vod_play_from").unwrap_or_else(|| "默认".to_string());
    let play_url = non_empty_str(vod, "vod_play_url").unwrap_or_default();
    let sources: Vec<&str> = play_from.split("$$$").collect();
    let url_groups: Vec<&str> = play_url.split("$$$").collect();
    let mut playlists = vec![];
    for (i, source) in sources.iter().enumerate() {
        let source = source.trim();
        if source.is_empty() {
            continue;
        }
        let group = url_groups.get(i).copied().unwrap_or("");
        let episodes = parse_episodes(group);
        if episodes.is_empty() {
            continue;
        }
        playlists.push(json!({
            "id": source,
            "label": source,
            "episodes": episodes,
        }));
    }
    if playlists.is_empty() {
        return Err(PluginError::new(
            "INTERNAL",
            "详情没有可播放的分集（vod_play_url 为空）",
        ));
    }
    let obj = item.as_object_mut().unwrap();
    obj.insert("playlists".into(), json!(playlists));
    // 演职员：detail 独有，列表条目不带。
    let mut people = vec![];
    for (key, role) in [("vod_actor", "演员"), ("vod_director", "导演")] {
        if let Some(names) = non_empty_str(vod, key) {
            for name in names.split(|c| c == ',' || c == '，' || c == '/') {
                let name = name.trim();
                if !name.is_empty() {
                    people.push(json!({"name": name, "role": role}));
                }
            }
        }
    }
    if !people.is_empty() {
        obj.insert("people".into(), json!(people));
    }
    Ok(item)
}

fn is_media_url(url: &str) -> bool {
    let base = url.split(['?', '#']).next().unwrap_or("").to_ascii_lowercase();
    base.ends_with(".m3u8")
        || base.ends_with(".mp4")
        || base.ends_with(".m4v")
        || base.ends_with(".mov")
        || base.ends_with(".ts")
        || base.ends_with(".mkv")
        || base.ends_with(".flv")
}

/// `playerContent` 的 `{parse, url, header}` → PlaybackInfo。
/// parse==0：直链。parse==1：url 是直链媒体地址时按 parse==0 处理，
/// 否则 UNSUPPORTED（TVBox 的 parse:1 是开 WebView 嗅探，HPP 无对应物）。
pub fn playback(v: &Value, flag: &str) -> Result<Value> {
    if let Some(e) = upstream_error(v) {
        return Err(e);
    }
    let parse = v.get("parse").and_then(Value::as_i64).unwrap_or(0);
    let url = non_empty_str(v, "url")
        .ok_or_else(|| PluginError::new("CONTENT_LOCKED", "蜘蛛没有返回播放地址"))?;
    if parse == 1 && !is_media_url(&url) {
        return Err(PluginError::new(
            "UNSUPPORTED",
            "该地址需要 WebView 嗅探播放，当前版本不支持",
        ));
    }
    let base = url.split(['?', '#']).next().unwrap_or("");
    let format = if base.to_ascii_lowercase().ends_with(".m3u8") {
        "hls"
    } else {
        "progressive"
    };
    let mut stream = Map::new();
    stream.insert("url".into(), json!(url));
    stream.insert("format".into(), json!(format));
    if let Some(headers) = v.get("header").and_then(Value::as_object) {
        let clean: Map<String, Value> = headers
            .iter()
            .filter(|(_, val)| val.is_string())
            .map(|(k, val)| (k.clone(), val.clone()))
            .collect();
        if !clean.is_empty() {
            stream.insert("headers".into(), Value::Object(clean));
        }
    }
    let label = if flag.is_empty() { "默认" } else { flag };
    Ok(json!({
        "streams": [Value::Object(stream)],
        "servers": [{"id": label, "label": label}],
    }))
}

/// HPP 方法 → 蜘蛛方法 + 宿主侧参数（传给边车 /call 的 args）。
/// 返回 None 表示该 HPP 方法 Python v1 不支持。
pub fn dispatch(method: &str, params: &Value) -> Option<(&'static str, Vec<Value>)> {
    match method {
        "catalog.getCategories" => Some(("homeContent", vec![])),
        // home.getSections 不走这里：worker 按蜘蛛实际有的方法
        //（homeVideoContent 优先，否则 homeContent）选来源。
        "feed.list" => Some(("homeVideoContent", vec![])),
        "catalog.list" => {
            let tid = params
                .get("categoryId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let pg = params
                .get("cursor")
                .and_then(Value::as_str)
                .unwrap_or("1")
                .to_string();
            let filter = params.get("filters").cloned().unwrap_or(json!({}));
            let extend = match params.get("sort").and_then(Value::as_str) {
                Some(sort) => json!({"sort": sort}),
                None => Value::Null,
            };
            Some(("categoryContent", vec![json!(tid), json!(pg), filter, extend]))
        }
        "search.search" => {
            let query = params
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let pg = params
                .get("cursor")
                .and_then(Value::as_str)
                .unwrap_or("1")
                .to_string();
            Some(("searchContent", vec![json!(query), json!(pg)]))
        }
        "media.getDetail" => {
            let id = params
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            Some(("detailContent", vec![json!(id)]))
        }
        "media.resolvePlayback" => {
            let flag = params
                .get("playlistId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let id = params
                .get("episodeId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            Some(("playerContent", vec![json!(flag), json!(id)]))
        }
        _ => None,
    }
}

/// 蜘蛛方法 → HPP 方法面（worker spawn 时的探测产物）。
/// 只列 v1 映射的方法；localProxy / init / action 不进能力面。
pub fn hpp_methods(spider_methods: &[String]) -> Vec<String> {
    let has = |m: &str| spider_methods.iter().any(|x| x == m);
    let mut out = vec![];
    if has("detailContent") {
        out.push("media.getDetail".to_string());
    }
    if has("playerContent") {
        out.push("media.resolvePlayback".to_string());
    }
    if has("homeContent") {
        out.push("catalog.getCategories".to_string());
        // home.getSections 的实际来源在调用时按 homeVideoContent 有无决定；
        // 有 homeContent 即有首页内容面。
        out.push("home.getSections".to_string());
    }
    if has("homeVideoContent") {
        if !out.iter().any(|m| m == "home.getSections") {
            out.push("home.getSections".to_string());
        }
        out.push("feed.list".to_string());
    }
    if has("categoryContent") {
        out.push("catalog.list".to_string());
    }
    if has("searchContent") {
        out.push("search.search".to_string());
    }
    out
}

/// v1 必需的方法面：detailContent + playerContent 缺一不可，
/// 且至少一个内容入口（home / homeVideo / category / search）。
pub fn check_required(spider_methods: &[String]) -> Result<()> {
    let has = |m: &str| spider_methods.iter().any(|x| x == m);
    if !has("detailContent") || !has("playerContent") {
        return Err(PluginError::new(
            "INTERNAL",
            "蜘蛛缺少必需方法：detailContent 与 playerContent 必须同时实现",
        ));
    }
    if !["homeContent", "homeVideoContent", "categoryContent", "searchContent"]
        .iter()
        .any(|m| has(m))
    {
        return Err(PluginError::new(
            "INTERNAL",
            "蜘蛛缺少内容入口：homeContent / homeVideoContent / categoryContent / searchContent 至少实现一个",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vod() -> Value {
        json!({
            "vod_id": "v1",
            "vod_name": "测试片",
            "vod_pic": "https://example.invalid/cover.jpg",
            "vod_remarks": "更新至3集",
            "vod_year": "2024",
            "vod_area": "大陆",
            "vod_actor": "演员一,演员二",
            "vod_director": "导演",
            "vod_play_from": "线路A$$$线路B",
            "vod_play_url": "第1集$https://example.invalid/a1.m3u8#第2集$https://example.invalid/a2.m3u8$$$正片$https://example.invalid/b1.mp4",
        })
    }

    #[test]
    fn page_next_is_page_plus_one() {
        let v = json!({"list": [vod(), vod()], "page": 1, "pagecount": 3, "limit": 20, "total": 50});
        let out = page(&v).unwrap();
        assert_eq!(out["next"], json!("2"));
        assert_eq!(out["total"], json!(50));
        assert_eq!(out["items"].as_array().unwrap().len(), 2);
        let v = json!({"list": [], "page": 3, "pagecount": 3, "limit": 20, "total": 50});
        assert_eq!(page(&v).unwrap()["next"], Value::Null);
    }

    #[test]
    fn bad_rows_dropped() {
        let v = json!({"list": [vod(), {"vod_name": "缺 id"}, {"vod_id": "x"}, vod()], "page": 1, "pagecount": 1, "total": 4});
        let out = page(&v).unwrap();
        assert_eq!(out["items"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn categories_map() {
        let v = json!({"class": [{"type_id": "movie", "type_name": "电影"}, {"type_id": "", "type_name": "坏的"}]});
        let out = categories(&v).unwrap();
        assert_eq!(out.as_array().unwrap().len(), 1);
        assert_eq!(out[0]["id"], json!("movie"));
    }

    #[test]
    fn detail_splits_sources_and_episodes() {
        let out = detail(&json!({"list": [vod()]})).unwrap();
        let playlists = out["playlists"].as_array().unwrap();
        assert_eq!(playlists.len(), 2);
        assert_eq!(playlists[0]["id"], json!("线路A"));
        assert_eq!(playlists[0]["episodes"].as_array().unwrap().len(), 2);
        assert_eq!(playlists[1]["episodes"][0]["id"], json!("https://example.invalid/b1.mp4"));
        assert_eq!(out["people"][0]["role"], json!("演员"));
    }

    #[test]
    fn detail_empty_list_is_not_found() {
        let err = detail(&json!({"list": []})).unwrap_err();
        assert_eq!(err.code, "NOT_FOUND");
    }

    #[test]
    fn playback_hls_and_progressive() {
        let out = playback(
            &json!({"parse": 0, "url": "https://example.invalid/x.m3u8?token=1", "header": {"User-Agent": "UA"}}),
            "线路A",
        )
        .unwrap();
        assert_eq!(out["streams"][0]["format"], json!("hls"));
        assert_eq!(out["streams"][0]["headers"]["User-Agent"], json!("UA"));
        assert_eq!(out["servers"][0]["id"], json!("线路A"));
        let out = playback(&json!({"parse": 0, "url": "https://example.invalid/x.mp4"}), "").unwrap();
        assert_eq!(out["streams"][0]["format"], json!("progressive"));
    }

    #[test]
    fn playback_parse1_non_media_unsupported() {
        let err = playback(
            &json!({"parse": 1, "url": "https://example.invalid/page.html"}),
            "",
        )
        .unwrap_err();
        assert_eq!(err.code, "UNSUPPORTED");
        // parse:1 但 url 本身是直链时按 parse:0 处理。
        assert!(playback(&json!({"parse": 1, "url": "https://example.invalid/x.m3u8"}), "").is_ok());
    }

    #[test]
    fn playback_empty_url_locked() {
        let err = playback(&json!({"parse": 0, "url": ""}), "").unwrap_err();
        assert_eq!(err.code, "CONTENT_LOCKED");
    }

    #[test]
    fn upstream_error_code() {
        let err = upstream_error(&json!({"code": -1, "message": "token 失效"})).unwrap();
        assert_eq!(err.code, "UPSTREAM_ERROR");
        assert!(upstream_error(&json!({"code": 0})).is_none());
        // 带 list 的 code 字段不触发（蜘蛛已自行处理成列表）。
        assert!(upstream_error(&json!({"code": -1, "list": []})).is_none());
    }

    #[test]
    fn required_methods() {
        let all: Vec<String> = ["homeContent", "detailContent", "playerContent"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(check_required(&all).is_ok());
        let no_play: Vec<String> = ["homeContent", "detailContent"].iter().map(|s| s.to_string()).collect();
        assert!(check_required(&no_play).is_err());
        let no_entry: Vec<String> = ["detailContent", "playerContent"].iter().map(|s| s.to_string()).collect();
        assert!(check_required(&no_entry).is_err());
    }

    #[test]
    fn dispatch_args() {
        let (method, args) = dispatch("catalog.list", &json!({"categoryId": "movie", "cursor": "2"})).unwrap();
        assert_eq!(method, "categoryContent");
        assert_eq!(args[0], json!("movie"));
        assert_eq!(args[1], json!("2"));
        let (method, args) = dispatch("search.search", &json!({"query": "abc"})).unwrap();
        assert_eq!(method, "searchContent");
        assert_eq!(args, vec![json!("abc"), json!("1")]);
        assert!(dispatch("transformResource", &json!({})).is_none());
    }
}
