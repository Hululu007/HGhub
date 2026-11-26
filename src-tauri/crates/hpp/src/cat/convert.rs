//! 猫源 HTTP 服务返回值 → HPP 形状的纯函数转换。
//!
//! 复用 `crate::python::convert` 的 `vod_to_item` / `page` /
//! `categories` / `sections` / `detail`（猫源的数据格式与之一致），
//! 猫源自己的部分只有：信封解包、方法分发、播放转换、站点 id 编解码
//! 与结果改写。
//!
//! 一个订阅含多个站点时，站点 key 编进返回的 id 里（`{site}::{raw}`，
//! 按第一个 `::` 切分）：分类 id、作品 id、线路 id、选集 id 都如此；
//! 播放时再解出站点与原始值。
use crate::{
    node::CatSite,
    python::convert as py,
    PluginError, Result,
};
use serde_json::{json, Value};

/// worker 在合并多站分类前给每条 `class` 打的临时站点标记：
/// `prefix_categories` 据此编码 id，改写完即删掉，不进 HPP 返回值。
const SITE_MARK_KEY: &str = "_cat_site";

fn truncate(text: &str, limit: usize) -> String {
    let out: String = text.chars().take(limit).collect();
    if text.chars().count() > limit {
        format!("{out}…")
    } else {
        out
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

/// 猫源响应信封 `{code: 0, data: …}` 解包：`code != 0` 转
/// `UPSTREAM_ERROR`（文案取 `message`/`msg`，截断 200 字）；`code == 0`
/// 时返回 `data`（缺 `data` 报 INTERNAL）。没有 `code` 字段的响应
/// （非信封的裸数据）原样当作 `data` 返回。
pub fn unwrap_envelope(v: &Value) -> Result<Value> {
    let Some(code) = v.get("code").and_then(Value::as_i64) else {
        return Ok(v.clone());
    };
    if code != 0 {
        let message = v
            .get("message")
            .or_else(|| v.get("msg"))
            .and_then(Value::as_str)
            .unwrap_or("上游返回失败");
        return Err(PluginError::new(
            "UPSTREAM_ERROR",
            format!("上游返回失败（code {code}）：{}", truncate(message, 200)),
        ));
    }
    v.get("data")
        .cloned()
        .ok_or_else(|| PluginError::new("INTERNAL", "猫源返回缺少 data"))
}

/// 一次对源 HTTP 服务的调用：单站点，或向全部 type-3 站点扇出再合并。
pub enum CatCall {
    Single {
        site: CatSite,
        endpoint: &'static str,
        body: Value,
    },
    FanOut {
        endpoint: &'static str,
        body: Value,
    },
    /// 只返回已知的站点列表（`id` 是站 key、`label` 是站名、`children`
    /// 为空数组），不调源服务：分类页先靠它把资源列表画出来，点某个站
    /// 后再用带 `site` 参数的 `catalog.getCategories` 取该站的分类，
    /// 不用一打开就把 93 个站的 `/home` 全扇出等一遍。
    Shallow,
}

/// HPP 方法 → 猫源 HTTP 调用（复用 `python::convert::dispatch` 的模式）。
/// 返回 None 表示该 HPP 方法猫源不支持。
pub fn dispatch(method: &str, params: &Value, sites: &[CatSite]) -> Option<CatCall> {
    match method {
        "catalog.getCategories" => {
            if params.get("shallow").and_then(Value::as_bool) == Some(true) {
                return Some(CatCall::Shallow);
            }
            if let Some(key) = params.get("site").and_then(Value::as_str) {
                let site = sites.iter().find(|s| s.key == key)?.clone();
                return Some(CatCall::Single {
                    site,
                    endpoint: "/home",
                    body: json!({}),
                });
            }
            Some(CatCall::FanOut {
                endpoint: "/home",
                body: json!({}),
            })
        }
        "home.getSections" | "feed.list" => {
            Some(CatCall::FanOut {
                endpoint: "/home",
                body: json!({}),
            })
        }
        "catalog.list" => {
            let raw = params
                .get("categoryId")
                .and_then(Value::as_str)
                .unwrap_or("");
            let (site, tid) = decode_id(raw, sites).ok()?;
            let pg = params
                .get("cursor")
                .and_then(Value::as_str)
                .unwrap_or("1")
                .to_string();
            // filter/filters/sort 的取法照抄 python 的 dispatch：filters
            // 对象作 filters，sort 有则并进 filters；filter 是“是否开启筛选”。
            let mut filters = params.get("filters").cloned().unwrap_or(json!({}));
            if !filters.is_object() {
                filters = json!({});
            }
            if let Some(sort) = params.get("sort").and_then(Value::as_str) {
                filters
                    .as_object_mut()
                    .unwrap()
                    .insert("sort".into(), json!(sort));
            }
            let filter = filters.as_object().is_some_and(|o| !o.is_empty());
            Some(CatCall::Single {
                site,
                endpoint: "/category",
                body: json!({"id": tid, "page": pg, "filter": filter, "filters": filters}),
            })
        }
        "search.search" => {
            let wd = params
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let pg = params
                .get("cursor")
                .and_then(Value::as_str)
                .unwrap_or("1")
                .to_string();
            Some(CatCall::FanOut {
                endpoint: "/search",
                body: json!({"wd": wd, "page": pg}),
            })
        }
        "media.getDetail" => {
            let raw = params.get("id").and_then(Value::as_str).unwrap_or("");
            let (site, id) = decode_id(raw, sites).ok()?;
            Some(CatCall::Single {
                site,
                endpoint: "/detail",
                body: json!({"id": id}),
            })
        }
        "media.resolvePlayback" => {
            // 优先用 episodeId 的站点；flag 与 id 都要先去掉站点前缀。
            let raw = params
                .get("episodeId")
                .and_then(Value::as_str)
                .unwrap_or("");
            let (site, id) = decode_id(raw, sites).ok()?;
            let flag_raw = params
                .get("playlistId")
                .and_then(Value::as_str)
                .unwrap_or("");
            let flag = decode_id(flag_raw, sites)
                .map(|(_, raw)| raw)
                .unwrap_or_else(|_| flag_raw.to_string());
            Some(CatCall::Single {
                site,
                endpoint: "/play",
                body: json!({"flag": flag, "id": id}),
            })
        }
        _ => None,
    }
}

/// `dispatch` 返回 None 时，调用方用它区分“方法不支持”与“id 解不出
/// 站点”：后者返回 `decode_id` 的原始错误（只针对单站点方法）。
pub fn decode_id_error(method: &str, params: &Value, sites: &[CatSite]) -> Option<PluginError> {
    let raw = match method {
        "catalog.list" => params.get("categoryId")?.as_str()?,
        "media.getDetail" => params.get("id")?.as_str()?,
        "media.resolvePlayback" => params.get("episodeId")?.as_str()?,
        _ => return None,
    };
    decode_id(raw, sites).err()
}

/// 猫源插件的方法面：内容源的 7 个方法（与 Python 蜘蛛一致）。
pub fn hpp_methods() -> Vec<String> {
    [
        "catalog.getCategories",
        "home.getSections",
        "feed.list",
        "catalog.list",
        "search.search",
        "media.getDetail",
        "media.resolvePlayback",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// 至少一个 `type == 3` 的站点，否则该源没有可用站点。
pub fn check_required(sites: &[CatSite]) -> Result<()> {
    if sites.iter().any(|s| s.site_type == 3) {
        Ok(())
    } else {
        Err(PluginError::new(
            "INTERNAL",
            "该源没有可用站点",
        ))
    }
}

/// `/play` 的 `{url, header, extra}` → PlaybackInfo。
/// `url` 是 `[画质名, 地址, 画质名, 地址, …]` 的扁平数组，两两成对；
/// `header` 里的字符串头随每条流带上；`extra.danmaku` / `extra.audio`
/// 第一版接不上，忽略、不报错。
pub fn playback(v: &Value, flag: &str) -> Result<Value> {
    let data = unwrap_envelope(v)?;
    let mut pairs: Vec<(String, String)> = vec![];
    match data.get("url") {
        Some(Value::Array(a)) => {
            let mut it = a.iter();
            while let Some(label) = it.next() {
                let Some(addr) = it.next() else {
                    break;
                };
                let label = label.as_str().unwrap_or("").to_string();
                let Some(addr) = addr.as_str() else {
                    continue;
                };
                pairs.push((label, addr.to_string()));
            }
        }
        // 个别源直接给单个地址字符串：按一对处理。
        Some(Value::String(s)) if !s.trim().is_empty() => {
            pairs.push((flag.to_string(), s.clone()));
        }
        _ => {}
    }
    if pairs.is_empty() {
        return Err(PluginError::new("CONTENT_LOCKED", "猫源没有返回播放地址"));
    }
    if pairs
        .iter()
        .any(|(_, addr)| addr.starts_with("push://"))
    {
        return Err(PluginError::new(
            "UNSUPPORTED",
            "该源的播放地址需要推送，暂不支持",
        ));
    }
    let headers: Option<Value> = data
        .get("header")
        .and_then(Value::as_object)
        .map(|h| {
            Value::Object(
                h.iter()
                    .filter(|(_, val)| val.is_string())
                    .map(|(k, val)| (k.clone(), val.clone()))
                    .collect(),
            )
        })
        .filter(|h| h.as_object().is_some_and(|o| !o.is_empty()));
    let label_default = if flag.is_empty() { "默认" } else { flag };
    let mut streams = vec![];
    for (label, url) in pairs {
        let base = url.split(['?', '#']).next().unwrap_or("");
        let format = if base.to_ascii_lowercase().ends_with(".m3u8") {
            "hls"
        } else {
            "progressive"
        };
        let mut stream = serde_json::Map::new();
        stream.insert("url".into(), json!(url));
        stream.insert("format".into(), json!(format));
        stream.insert(
            "label".into(),
            json!(if label.is_empty() {
                label_default
            } else {
                label.as_str()
            }),
        );
        if let Some(headers) = &headers {
            stream.insert("headers".into(), headers.clone());
        }
        streams.push(Value::Object(stream));
    }
    Ok(json!({
        "streams": streams,
        "servers": [{"id": label_default, "label": label_default}],
    }))
}

/// 站点 key 与原始 id 编成一个 HPP id。
pub fn encode_id(site: &str, raw: &str) -> String {
    format!("{site}::{raw}")
}

/// HPP id 解回 `(站点, 原始 id)`：无 `::` 前缀且只剩一个 type-3 站点
/// 时用该站点；无前缀且多站点时报明错。
pub fn decode_id(id: &str, sites: &[CatSite]) -> Result<(CatSite, String)> {
    match id.split_once("::") {
        Some((key, raw)) => sites
            .iter()
            .find(|s| s.key == key)
            .cloned()
            .map(|site| (site, raw.to_string()))
            .ok_or_else(|| PluginError::invalid("无法确定该内容的站点")),
        None => {
            if sites.len() == 1 {
                Ok((sites[0].clone(), id.to_string()))
            } else {
                Err(PluginError::invalid("无法确定该内容的站点"))
            }
        }
    }
}

/// 分类改写：每条 `class` 的 `type_id` 加上站点前缀；多站点时 label
/// 加站点名前缀（`"{站点名} · {分类名}"`），单站点时保持原样。
/// 站点按每条自带的临时标记（worker 合并时打上）识别，缺标记时单站点
/// 用唯一站点、多站点跳过该条。返回与输入同形的 `{"class": [...]}`。
pub fn prefix_categories(data: &Value, sites: &[CatSite]) -> Result<Value> {
    let class = data
        .get("class")
        .and_then(Value::as_array)
        .ok_or_else(|| PluginError::new("INTERNAL", "猫源返回缺少 class"))?;
    let multi = sites.len() > 1;
    let mut out = vec![];
    for c in class {
        let Some(raw_id) = non_empty_str(c, "type_id") else {
            continue;
        };
        let Some(raw_label) = non_empty_str(c, "type_name") else {
            continue;
        };
        let site = match c.get(SITE_MARK_KEY).and_then(Value::as_str) {
            Some(key) => sites.iter().find(|s| s.key == key),
            None => {
                if sites.len() == 1 {
                    sites.first()
                } else {
                    None
                }
            }
        };
        let Some(site) = site else {
            continue;
        };
        let label = if multi {
            format!("{} · {raw_label}", site.name)
        } else {
            raw_label
        };
        let mut c = c.clone();
        let obj = c.as_object_mut().unwrap();
        obj.insert("type_id".into(), json!(encode_id(&site.key, &raw_id)));
        obj.insert("type_name".into(), json!(label));
        obj.remove(SITE_MARK_KEY);
        out.push(c);
    }
    let mut data = data.clone();
    if let Some(obj) = data.as_object_mut() {
        obj.insert("class".into(), Value::Array(out));
    }
    Ok(data)
}

/// 多站点分类按站点分组：顶层一个节点一个站（`id` 是站 key、`label`
/// 是站名），子节点是该站的 class（`id` 是 `站::type_id`、`label`
/// 是原始 `type_name`，不带站名前缀——站名已在父节点上）。
/// 空站（`/home` 无 class 或调用失败）不出现，与今天扁平列表丢站
/// 的行为一致。单站订阅不走这里（`convert_single` 仍用扁平的
/// `prefix_categories`，一个站的分类直接平铺）。
/// 分类页只在切到某个站后才展示该站的分类（2026-11-26 用户要求：
/// 猫源下的资源子分类不默认全量展示）。
pub fn nest_categories(data: &Value, sites: &[CatSite]) -> Result<Value> {
    let class = data
        .get("class")
        .and_then(Value::as_array)
        .ok_or_else(|| PluginError::new("INTERNAL", "猫源返回缺少 class"))?;
    let mut out = vec![];
    for site in sites {
        let mut children = vec![];
        for c in class {
            let is_site =
                c.get(SITE_MARK_KEY).and_then(Value::as_str) == Some(site.key.as_str());
            if !is_site {
                continue;
            }
            let Some(raw_id) = non_empty_str(c, "type_id") else {
                continue;
            };
            let Some(raw_label) = non_empty_str(c, "type_name") else {
                continue;
            };
            children.push(json!({"id": encode_id(&site.key, &raw_id), "label": raw_label}));
        }
        if children.is_empty() {
            continue;
        }
        out.push(json!({"id": site.key, "label": site.name, "children": children}));
    }
    Ok(Value::Array(out))
}

/// 分页列表改写：先把原始 `list[]` 里每条的 `vod_id` 加上站点前缀，
/// 再调 `python::convert::page`。
pub fn prefix_page_items(data: &Value, site: &CatSite) -> Result<Value> {
    let mut data = data.clone();
    if let Some(list) = data
        .get_mut("list")
        .and_then(Value::as_array_mut)
    {
        for item in list.iter_mut() {
            if let Some(id) = item.get("vod_id").and_then(Value::as_str).map(str::to_string) {
                if let Some(obj) = item.as_object_mut() {
                    obj.insert("vod_id".into(), json!(encode_id(&site.key, &id)));
                }
            }
        }
    }
    Ok(data)
}

/// 详情改写：先调 `python::convert::detail`，再把每个 playlist 的 `id`
/// 改成 `"{site}::{flag}"`（label 不动）、每个 episode 的 `id` 改成
/// `"{site}::{rawId}"`。
pub fn rewrite_detail(data: &Value, site: &CatSite) -> Result<Value> {
    let mut detail = py::detail(data)?;
    if let Some(playlists) = detail
        .get_mut("playlists")
        .and_then(Value::as_array_mut)
    {
        for playlist in playlists.iter_mut() {
            if let Some(id) = playlist.get("id").and_then(Value::as_str).map(str::to_string) {
                playlist
                    .as_object_mut()
                    .unwrap()
                    .insert("id".into(), json!(encode_id(&site.key, &id)));
            }
            if let Some(episodes) = playlist
                .get_mut("episodes")
                .and_then(Value::as_array_mut)
            {
                for ep in episodes.iter_mut() {
                    if let Some(id) = ep.get("id").and_then(Value::as_str).map(str::to_string) {
                        ep.as_object_mut()
                            .unwrap()
                            .insert("id".into(), json!(encode_id(&site.key, &id)));
                    }
                }
            }
        }
    }
    Ok(detail)
}

/// 详情诊断摘要：每条播放列表取（名、集数、最长分集 id 字节数）。
/// 长度按字节计，与 `model.rs` 校验的 `.len()` 对得上；只量数量与长度，
/// 不返回地址原文——网盘分集 id 可能是分享链接，绝不记进日志（见 cat-source.md §2.8）。
pub fn detail_summary(v: &Value) -> Vec<(String, usize, usize)> {
    let mut out = vec![];
    if let Some(playlists) = v.get("playlists").and_then(Value::as_array) {
        for p in playlists {
            let label = p
                .get("label")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .or_else(|| p.get("id").and_then(Value::as_str))
                .unwrap_or("未命名")
                .to_string();
            let episodes = p.get("episodes").and_then(Value::as_array);
            let count = episodes.map(|e| e.len()).unwrap_or(0);
            let max_len = episodes
                .map(|eps| {
                    eps.iter()
                        .filter_map(|e| e.get("id").and_then(Value::as_str))
                        .map(|s| s.len())
                        .max()
                        .unwrap_or(0)
                })
                .unwrap_or(0);
            out.push((label, count, max_len));
        }
    }
    out
}

/// 列表诊断摘要：条目总数、缺 id 或标题的条目数、封面形态计数。
/// 只量数量与形态，不返回地址原文——封面可能是回环 `/pic?url=…`
/// 签名地址，绝不记进日志（见 cat-source.md §2.8）。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ListSummary {
    /// 转换后的条目总数（`items` 长度）。
    pub total: usize,
    /// 缺 `id` 或缺 `title` 的条目数（校验必丢）。
    pub missing_id_or_title: usize,
    /// 封面是 `https:` 地址的条目数。
    pub cover_https: usize,
    /// 封面是回环 `http:` 地址（`127.0.0.1` / `localhost`）的条目数。
    pub cover_http_loopback: usize,
    /// 封面是其它 `http:` 地址的条目数。
    pub cover_http_other: usize,
    /// 没有封面（缺 `cover`、空值或空串）的条目数。
    pub cover_none: usize,
    /// 封面存在但不是 `http(s):` 地址（相对路径等）的条目数。
    pub cover_other: usize,
}

impl ListSummary {
    /// 只含封面形态计数的半句，供校验丢条目那行复用。
    pub fn cover_part(&self) -> String {
        format!(
            "https {} 条、回环 http {} 条、其它 http {} 条、无封面 {} 条、非 http(s) {} 条",
            self.cover_https,
            self.cover_http_loopback,
            self.cover_http_other,
            self.cover_none,
            self.cover_other
        )
    }
}

impl std::fmt::Display for ListSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "共 {} 条，缺 id 或标题 {} 条；封面 {}",
            self.total, self.missing_id_or_title, self.cover_part()
        )
    }
}

/// 从 `py::page` 输出（`{items, next, total}`）里提炼 [`ListSummary`]。
/// 封面只按形态计数：字符串本身，或 `{url}` 对象的 `url` 字段。
pub fn list_summary(v: &Value) -> ListSummary {
    let mut s = ListSummary::default();
    if let Some(items) = v.get("items").and_then(Value::as_array) {
        for item in items {
            s.total += 1;
            let id_ok = item
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|t| !t.is_empty());
            let title_ok = item
                .get("title")
                .and_then(Value::as_str)
                .is_some_and(|t| !t.is_empty());
            if !id_ok || !title_ok {
                s.missing_id_or_title += 1;
            }
            match cover_url(item) {
                None => s.cover_none += 1,
                Some(u) if u.starts_with("https://") => s.cover_https += 1,
                Some(u) if is_loopback_http(u) => s.cover_http_loopback += 1,
                Some(u) if u.starts_with("http://") => s.cover_http_other += 1,
                Some(_) => s.cover_other += 1,
            }
        }
    }
    s
}

fn cover_url(item: &Value) -> Option<&str> {
    match item.get("cover") {
        Some(Value::String(s)) if !s.is_empty() => Some(s),
        Some(Value::Object(o)) => o
            .get("url")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty()),
        _ => None,
    }
}

fn is_loopback_http(u: &str) -> bool {
    u.starts_with("http://127.0.0.1") || u.starts_with("http://localhost")
}

/// 分区诊断摘要：分区数、条目总数、空分区（只记分区 id，即站点 key，
/// 不记地址原文；`py::sections` 产的分区本来就没有标题，只有回退分区
/// 才带分类名标题）。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SectionsSummary {
    /// 转换后的分区数。
    pub sections: usize,
    /// 各分区条目数之和。
    pub total_items: usize,
    /// 条目数为 0 的分区个数。
    pub empty_sections: usize,
    /// 空分区的 id（最多记 10 个）。
    pub empty_ids: Vec<String>,
}

impl std::fmt::Display for SectionsSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "共 {} 个分区，合计 {} 条", self.sections, self.total_items)?;
        if self.empty_sections > 0 {
            write!(f, "；空分区 {} 个", self.empty_sections)?;
            if !self.empty_ids.is_empty() {
                write!(f, "（{}）", self.empty_ids.join("、"))?;
            }
        }
        Ok(())
    }
}

/// 从 `home.getSections` 的转换输出（`{items: [Section], next, total}`）
/// 里提炼 [`SectionsSummary`]。
pub fn sections_summary(v: &Value) -> SectionsSummary {
    let mut s = SectionsSummary::default();
    if let Some(items) = v.get("items").and_then(Value::as_array) {
        for section in items {
            s.sections += 1;
            let n = section
                .get("items")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            s.total_items += n;
            if n == 0 {
                s.empty_sections += 1;
                if s.empty_ids.len() < 10 {
                    if let Some(id) = section
                        .get("id")
                        .and_then(Value::as_str)
                        .filter(|t| !t.is_empty())
                    {
                        s.empty_ids.push(id.to_string());
                    }
                }
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site(key: &str, name: &str) -> CatSite {
        CatSite {
            key: key.into(),
            name: name.into(),
            site_type: 3,
            api: format!("/spider/{key}/3"),
        }
    }

    fn vod() -> Value {
        json!({
            "vod_id": "v1",
            "vod_name": "测试片",
            "vod_pic": "https://example.invalid/cover.jpg",
            "vod_remarks": "更新至3集",
        })
    }

    #[test]
    fn envelope_unwrap_ok_and_missing_data() {
        let v = unwrap_envelope(&json!({"code": 0, "data": {"class": []}})).unwrap();
        assert_eq!(v, json!({"class": []}));
        // 没有 code 字段的裸数据原样当作 data。
        let v = unwrap_envelope(&json!({"class": []})).unwrap();
        assert_eq!(v, json!({"class": []}));
        let err = unwrap_envelope(&json!({"code": 0})).unwrap_err();
        assert_eq!(err.code, "INTERNAL");
    }

    #[test]
    fn envelope_nonzero_is_upstream_error() {
        let err = unwrap_envelope(&json!({"code": -1, "message": "token 失效"})).unwrap_err();
        assert_eq!(err.code, "UPSTREAM_ERROR");
        assert!(err.message.contains("token 失效"), "{}", err.message);
        let err = unwrap_envelope(&json!({"code": 500, "msg": "错了"})).unwrap_err();
        assert!(err.message.contains("错了"), "{}", err.message);
    }

    #[test]
    fn nest_categories_groups_by_site_without_label_prefix() {
        let sites = vec![
            site("nodejs_a", "站A"),
            site("nodejs_b", "站B"),
            site("nodejs_empty", "空站"),
        ];
        let merged = json!({"class": [
            {"type_id": "movie", "type_name": "电影", "_cat_site": "nodejs_a"},
            {"type_id": "tv", "type_name": "剧集", "_cat_site": "nodejs_a"},
            {"type_id": "movie", "type_name": "电影", "_cat_site": "nodejs_b"},
            {"type_id": "", "type_name": "坏的", "_cat_site": "nodejs_b"},
        ]});
        let out = nest_categories(&merged, &sites).unwrap();
        let arr = out.as_array().unwrap();
        // 空站不出现；顺序按站点顺序。
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["id"], json!("nodejs_a"));
        assert_eq!(arr[0]["label"], json!("站A"));
        let kids = arr[0]["children"].as_array().unwrap();
        assert_eq!(kids.len(), 2);
        // 子节点 label 是原始分类名，不带站名前缀；id 带站前缀。
        assert_eq!(kids[0]["id"], json!("nodejs_a::movie"));
        assert_eq!(kids[0]["label"], json!("电影"));
        assert_eq!(kids[1]["id"], json!("nodejs_a::tv"));
        assert_eq!(arr[1]["id"], json!("nodejs_b"));
        assert_eq!(arr[1]["children"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn playback_quality_pairs_and_hls() {
        let out = playback(
            &json!({"code": 0, "data": {"url": ["高清", "https://example.invalid/a.m3u8", "流畅", "https://example.invalid/b.mp4"], "header": {"User-Agent": "UA"}}}),
            "线路A",
        )
        .unwrap();
        let streams = out["streams"].as_array().unwrap();
        assert_eq!(streams.len(), 2);
        assert_eq!(streams[0]["format"], json!("hls"));
        assert_eq!(streams[0]["label"], json!("高清"));
        assert_eq!(streams[0]["headers"]["User-Agent"], json!("UA"));
        assert_eq!(streams[1]["format"], json!("progressive"));
        assert_eq!(streams[1]["label"], json!("流畅"));
        assert_eq!(streams[1]["headers"]["User-Agent"], json!("UA"));
        assert_eq!(out["servers"][0]["id"], json!("线路A"));
    }

    #[test]
    fn playback_push_rejected_and_empty_locked() {
        let err = playback(
            &json!({"code": 0, "data": {"url": ["推送", "push://http://example.invalid/x"]}}),
            "",
        )
        .unwrap_err();
        assert_eq!(err.code, "UNSUPPORTED");
        let err = playback(&json!({"code": 0, "data": {"url": []}}), "").unwrap_err();
        assert_eq!(err.code, "CONTENT_LOCKED");
        // url 不是数组也不是字符串时同样无地址。
        let err = playback(&json!({"code": 0, "data": {}}), "").unwrap_err();
        assert_eq!(err.code, "CONTENT_LOCKED");
    }

    #[test]
    fn playback_flag_default() {
        let out = playback(
            &json!({"code": 0, "data": {"url": ["", "https://example.invalid/x.mp4"]}}),
            "",
        )
        .unwrap();
        assert_eq!(out["servers"][0]["id"], json!("默认"));
        assert_eq!(out["streams"][0]["label"], json!("默认"));
    }

    #[test]
    fn id_encode_decode() {
        let sites = vec![site("a", "站点 A")];
        assert_eq!(encode_id("a", "v1"), "a::v1");
        // 单站点：无前缀回落到唯一站点。
        let (s, raw) = decode_id("v1", &sites).unwrap();
        assert_eq!(s.key, "a");
        assert_eq!(raw, "v1");
        // 原始 id 里可含 ::，按第一个切分。
        let (s, raw) = decode_id("a::x::y", &sites).unwrap();
        assert_eq!(s.key, "a");
        assert_eq!(raw, "x::y");
        // 未知站点前缀报明错。
        assert!(decode_id("zzz::v1", &sites).is_err());
    }

    #[test]
    fn id_decode_multi_site_without_prefix_errors() {
        let sites = vec![site("a", "站点 A"), site("b", "站点 B")];
        let err = decode_id("v1", &sites).unwrap_err();
        assert_eq!(err.code, "INVALID_ARGUMENT");
        assert!(err.message.contains("无法确定该内容的站点"), "{}", err.message);
        let (s, raw) = decode_id("b::v1", &sites).unwrap();
        assert_eq!(s.key, "b");
        assert_eq!(raw, "v1");
    }

    #[test]
    fn categories_prefix_single_and_multi() {
        let one = vec![site("a", "站点 A")];
        let data = json!({"class": [{"type_id": "movie", "type_name": "电影"}]});
        let prefixed = prefix_categories(&data, &one).unwrap();
        assert_eq!(prefixed["class"][0]["type_id"], json!("a::movie"));
        assert_eq!(prefixed["class"][0]["type_name"], json!("电影"));
        let cats = py::categories(&prefixed).unwrap();
        assert_eq!(cats[0]["id"], json!("a::movie"));

        // 多站点：label 加站点名前缀，按临时标记识别站点。
        let two = vec![site("a", "站点 A"), site("b", "站点 B")];
        let data = json!({"class": [
            {"type_id": "movie", "type_name": "电影", "_cat_site": "a"},
            {"type_id": "tv", "type_name": "剧集", "_cat_site": "b"},
        ]});
        let prefixed = prefix_categories(&data, &two).unwrap();
        assert_eq!(prefixed["class"][0]["type_id"], json!("a::movie"));
        assert_eq!(prefixed["class"][0]["type_name"], json!("站点 A · 电影"));
        assert_eq!(prefixed["class"][1]["type_name"], json!("站点 B · 剧集"));
        assert!(prefixed["class"][0].get("_cat_site").is_none());
    }

    #[test]
    fn page_items_and_detail_rewrite() {
        let s = site("a", "站点 A");
        let data = json!({"list": [vod()], "page": 1, "pagecount": 1, "total": 1});
        let page = py::page(&prefix_page_items(&data, &s).unwrap()).unwrap();
        assert_eq!(page["items"][0]["id"], json!("a::v1"));

        let detail_data = json!({"list": [{
            "vod_id": "v1", "vod_name": "测试片",
            "vod_play_from": "线路A", "vod_play_url": "第1集$ep1#第2集$ep2",
        }]});
        let out = rewrite_detail(&detail_data, &s).unwrap();
        let playlists = out["playlists"].as_array().unwrap();
        assert_eq!(playlists[0]["id"], json!("a::线路A"));
        assert_eq!(playlists[0]["label"], json!("线路A"));
        assert_eq!(playlists[0]["episodes"][0]["id"], json!("a::ep1"));
        assert_eq!(playlists[0]["episodes"][1]["id"], json!("a::ep2"));
    }

    #[test]
    fn detail_summary_counts_lengths_only() {
        let v = json!({"playlists": [
            {"id": "a::线路A", "label": "线路A", "episodes": [
                {"id": "a::ep1", "index": 1},
                {"id": "a::ep22222", "index": 2},
            ]},
            {"id": "a::线路B", "label": "线路B", "episodes": []},
        ]});
        assert_eq!(
            detail_summary(&v),
            vec![
                ("线路A".to_string(), 2, 10),
                ("线路B".to_string(), 0, 0),
            ]
        );
        assert!(detail_summary(&json!({})).is_empty());
        assert!(detail_summary(&json!({"playlists": "not-an-array"})).is_empty());
        // 长度按字节计（中文 1 字 3 字节），与校验的 `.len()` 对得上。
        let v = json!({"playlists": [
            {"id": "a::线路C", "label": "线路C", "episodes": [{"id": "a::中文", "index": 1}]},
        ]});
        assert_eq!(
            detail_summary(&v),
            vec![("线路C".to_string(), 1, 9)]
        );
    }

    #[test]
    fn list_summary_counts_shapes_only() {
        let v = json!({"items": [
            {"id": "a::1", "title": "片一", "cover": "https://example.invalid/1.jpg"},
            {"id": "a::2", "title": "片二", "cover": "http://127.0.0.1:53659/pic?url=https%3A%2F%2Fexample.invalid%2F2.jpg"},
            {"id": "a::3", "title": "片三", "cover": {"url": "http://127.0.0.1:53659/pic?url=x"}},
            {"id": "a::4", "title": "片四"},
            {"title": "缺 id 的条目"},
            {"id": "a::6"},
            {"id": "a::7", "title": "片七", "cover": "http://example.invalid/7.jpg"},
            {"id": "a::8", "title": "片八", "cover": "/pic?url=x"},
        ], "next": null, "total": 8});
        let s = list_summary(&v);
        assert_eq!(s.total, 8);
        assert_eq!(s.missing_id_or_title, 2);
        assert_eq!(s.cover_https, 1);
        assert_eq!(s.cover_http_loopback, 2);
        assert_eq!(s.cover_http_other, 1);
        // 无封面：片四、缺 id 的条目、缺标题的条目。
        assert_eq!(s.cover_none, 3);
        assert_eq!(s.cover_other, 1);
        assert_eq!(
            format!("{s}"),
            "共 8 条，缺 id 或标题 2 条；封面 https 1 条、回环 http 2 条、其它 http 1 条、无封面 3 条、非 http(s) 1 条"
        );
        assert_eq!(list_summary(&json!({})), ListSummary::default());
        assert_eq!(
            list_summary(&json!({"items": "not-an-array"})),
            ListSummary::default()
        );
    }

    #[test]
    fn sections_summary_counts_and_empty_ids() {
        let v = json!({"items": [
            {"id": "a", "layout": "grid", "items": [{"id": "a::1"}, {"id": "a::2"}]},
            {"id": "b", "layout": "grid", "items": []},
            {"id": "c", "layout": "row"},
        ], "next": null, "total": 2});
        let s = sections_summary(&v);
        assert_eq!(s.sections, 3);
        assert_eq!(s.total_items, 2);
        assert_eq!(s.empty_sections, 2);
        // 没 items 字段的分区也算空；带 id 的空分区都记进 empty_ids。
        assert_eq!(s.empty_ids, vec!["b".to_string(), "c".to_string()]);
        assert_eq!(
            format!("{s}"),
            "共 3 个分区，合计 2 条；空分区 2 个（b、c）"
        );
        assert_eq!(sections_summary(&json!({})), SectionsSummary::default());
        assert_eq!(
            sections_summary(&json!({"items": "not-an-array"})),
            SectionsSummary::default()
        );
    }

    #[test]
    fn dispatch_maps_methods() {
        let sites = vec![site("a", "站点 A")];
        assert!(matches!(
            dispatch("catalog.getCategories", &json!({}), &sites),
            Some(CatCall::FanOut { endpoint: "/home", .. })
        ));
        assert!(matches!(
            dispatch("catalog.getCategories", &json!({"shallow": true}), &sites),
            Some(CatCall::Shallow)
        ));
        let Some(CatCall::Single { endpoint, .. }) = dispatch(
            "catalog.getCategories",
            &json!({"site": "a"}),
            &sites,
        ) else {
            panic!("带 site 的 getCategories 应为 Single");
        };
        assert_eq!(endpoint, "/home");
        // 不存在的站 key 不分发。
        assert!(dispatch("catalog.getCategories", &json!({"site": "nope"}), &sites).is_none());
        assert!(matches!(
            dispatch("feed.list", &json!({}), &sites),
            Some(CatCall::FanOut { endpoint: "/home", .. })
        ));
        let Some(CatCall::Single { endpoint, body, .. }) = dispatch(
            "catalog.list",
            &json!({"categoryId": "a::movie", "cursor": "2", "filters": {"area": "大陆"}, "sort": "time"}),
            &sites,
        ) else {
            panic!("catalog.list 应为 Single");
        };
        assert_eq!(endpoint, "/category");
        assert_eq!(body["id"], json!("movie"));
        assert_eq!(body["page"], json!("2"));
        assert_eq!(body["filter"], json!(true));
        assert_eq!(body["filters"]["sort"], json!("time"));
        let Some(CatCall::Single { endpoint, body, .. }) = dispatch(
            "media.resolvePlayback",
            &json!({"playlistId": "a::线路A", "episodeId": "a::ep1"}),
            &sites,
        ) else {
            panic!("resolvePlayback 应为 Single");
        };
        assert_eq!(endpoint, "/play");
        assert_eq!(body["flag"], json!("线路A"));
        assert_eq!(body["id"], json!("ep1"));
        assert!(dispatch("transformResource", &json!({}), &sites).is_none());
        assert_eq!(hpp_methods().len(), 7);
    }

    #[test]
    fn check_required_needs_type3() {
        assert!(check_required(&[site("a", "A")]).is_ok());
        let push = CatSite {
            key: "p".into(),
            name: "推送".into(),
            site_type: 4,
            api: "/spider/p/4".into(),
        };
        assert!(check_required(&[push]).is_err());
        assert!(check_required(&[]).is_err());
    }
}
