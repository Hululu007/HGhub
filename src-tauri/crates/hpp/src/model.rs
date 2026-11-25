use crate::{PluginError, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Meta {
    pub id: String,
    pub name: String,
    pub version: String,
    pub protocol: u32,
    #[serde(default = "source_kind")]
    pub kind: String,
    /// 运行时：`"js"`（默认，QuickJS 插件）、`"python"`（TVBox Python 蜘蛛，
    /// 走 CPython 边车）或 `"cat"`（Node 猫源，走 Node 边车）。旧索引无此字段的
    /// 一律按 `"js"` 读。
    #[serde(default = "js_runtime")]
    pub runtime: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub update_url: Option<String>,
    /// WebHome 首页声明（06 §18）：有它表示该源提供一个 WebHome 页面，
    /// 宿主在独立 WebView 里打开并注入 window.fm 桥。
    #[serde(default)]
    pub webhome: Option<WebHomeMeta>,
    #[serde(default)]
    pub matches: Vec<String>,
    #[serde(default)]
    pub link_domains: Vec<String>,
    #[serde(default)]
    pub default_headers: BTreeMap<String, String>,
    #[serde(default)]
    pub timeouts: BTreeMap<String, f64>,
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub requires: Vec<Requirement>,
    #[serde(default)]
    pub allow_insecure_http: bool,
    #[serde(default)]
    pub mature: bool,
    #[serde(default)]
    pub settings: Vec<Value>,
}
fn source_kind() -> String {
    "source".into()
}
fn js_runtime() -> String {
    "js".into()
}
/// WebHome 宿主声明：首页地址与本地页面二选一。
/// - `homePage`：远端页面地址，页面留在远端，相对资源照常工作。
/// - `localPage`：直接导入的本地 HTML 文件名（裸文件名，不含路径），
///   文件存在插件目录里，打开时宿主用回环地址提供。
/// 旧版本只有 homePage（必填字符串），反序列化时仍按 Some 处理。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebHomeMeta {
    #[serde(default)]
    pub home_page: Option<String>,
    #[serde(default)]
    pub local_page: Option<String>,
}

/// 裸文件名：不含路径分隔符，不为 `.` / `..`，不与插件自身文件重名。
fn is_bare_file_name(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= 255
        && !raw.contains(['/', '\\', '\0'])
        && raw.chars().all(|c| !c.is_control())
        && raw != "."
        && raw != ".."
        && raw != "plugin.js"
        && raw != "plugin.previous.json"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Requirement {
    pub r#type: String,
    pub name: String,
}

pub fn version(raw: &str) -> Result<[u64; 3]> {
    let parts = raw.split('.').collect::<Vec<_>>();
    if parts.len() != 3 {
        return Err(PluginError::invalid("版本号必须为三段非负整数"));
    }
    let mut out = [0; 3];
    for (i, s) in parts.iter().enumerate() {
        if s.is_empty()
            || (s.len() > 1 && s.starts_with('0'))
            || !s.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(PluginError::invalid("版本号格式无效"));
        }
        out[i] = s
            .parse::<u64>()
            .ok()
            .filter(|v| *v <= 9_007_199_254_740_991)
            .ok_or_else(|| PluginError::invalid("版本号数值过大"))?;
    }
    Ok(out)
}
impl Meta {
    pub fn parse(v: Value) -> Result<Self> {
        let m: Self = serde_json::from_value(v)
            .map_err(|e| PluginError::invalid(format!("插件元信息无效：{e}")))?;
        let b = m.id.as_bytes();
        if b.is_empty()
            || b.len() > 64
            || !b[0].is_ascii_lowercase() && !b[0].is_ascii_digit()
            || !b
                .iter()
                .all(|x| x.is_ascii_lowercase() || x.is_ascii_digit() || b"._-".contains(x))
        {
            return Err(PluginError::invalid("插件 ID 格式无效"));
        }
        // Windows device names cannot be used as directory names.
        let stem = m.id.split('.').next().unwrap_or("").to_ascii_uppercase();
        if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || (stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && stem.as_bytes()[3].is_ascii_digit())
        {
            return Err(PluginError::invalid("插件 ID 使用了 Windows 保留名称"));
        }
        if m.id.ends_with('.')
            || m.name.trim().is_empty()
            || m.name.chars().count() > 60
            || m.description.chars().count() > 500
        {
            return Err(PluginError::invalid("插件名称或简介长度无效"));
        }
        version(&m.version)?;
        if let Some(webhome) = &m.webhome {
            match (&webhome.home_page, &webhome.local_page) {
                (Some(home), None) => url(home, true, false).map_err(|_| {
                    PluginError::invalid("WebHome 首页地址必须为 http(s) 绝对地址")
                })?,
                (None, Some(local)) => {
                    if !is_bare_file_name(local) {
                        return Err(PluginError::invalid("WebHome 本地页面文件名无效"));
                    }
                }
                _ => {
                    return Err(PluginError::invalid(
                        "WebHome 首页须在 homePage 与 localPage 中二选一",
                    ));
                }
            }
        }
        if m.protocol != 1 {
            return Err(PluginError::new("UNSUPPORTED", "不支持该协议主版本"));
        }
        if !matches!(m.kind.as_str(), "source" | "resolver")
            || m.kind == "resolver" && m.matches.is_empty()
        {
            return Err(PluginError::invalid("插件类型或解析器 matches 无效"));
        }
        // Python 运行时 v1：蜘蛛恒为内容源，不声明 WebHome（WebHome 包装是 JS 形态）。
        if !matches!(m.runtime.as_str(), "js" | "python" | "cat") {
            return Err(PluginError::invalid("插件运行时无效"));
        }
        if m.runtime == "python" {
            if m.kind != "source" {
                return Err(PluginError::invalid("Python 蜘蛛只能是内容源"));
            }
            if m.webhome.is_some() {
                return Err(PluginError::invalid("Python 蜘蛛不能声明 WebHome 首页"));
            }
        }
        // 猫源 v1：与 Python 蜘蛛同款约束，恒为内容源，不声明 WebHome。
        if m.runtime == "cat" {
            if m.kind != "source" {
                return Err(PluginError::invalid("猫源只能是内容源"));
            }
            if m.webhome.is_some() {
                return Err(PluginError::invalid("猫源不能声明 WebHome 首页"));
            }
        }
        let mut keys = std::collections::HashSet::new();
        for s in &m.settings {
            let k = s["key"]
                .as_str()
                .filter(|x| !x.is_empty())
                .ok_or_else(|| PluginError::invalid("设置项缺少 key"))?;
            if !keys.insert(k)
                || !s["label"].is_string()
                || !matches!(
                    s["type"].as_str(),
                    Some("text" | "password" | "switch" | "number" | "select")
                )
            {
                return Err(PluginError::invalid("设置项无效或 key 重复"));
            }
            if let Some(v) = s.get("default") {
                validate_setting(s, v)?;
            }
        }
        for v in m.timeouts.values() {
            if !v.is_finite() {
                return Err(PluginError::invalid("调用预算必须为有限数值"));
            }
        }
        Ok(m)
    }
    pub fn budget(&self, method: &str) -> std::time::Duration {
        let (kind, default, max) = if method == "transformResource" {
            ("transform", 5.0, 10.0)
        } else if matches!(
            method,
            "media.resolvePlayback" | "resolver.resolve" | "live.resolve"
        ) {
            ("playback", 20.0, 60.0)
        } else if method == "auth.submit" {
            ("login", 30.0, 60.0)
        } else {
            ("read", 15.0, 30.0)
        };
        std::time::Duration::from_secs_f64(
            self.timeouts
                .get(kind)
                .copied()
                .unwrap_or(default)
                .clamp(5.0, max),
        )
    }
}
pub fn validate_setting(s: &Value, v: &Value) -> Result<()> {
    let valid = match s["type"].as_str() {
        Some("text" | "password") => v.is_string(),
        Some("switch") => v.is_boolean(),
        Some("number") => v.as_f64().is_some_and(|v| {
            v.is_finite()
                && s["min"].as_f64().is_none_or(|min| v >= min)
                && s["max"].as_f64().is_none_or(|max| v <= max)
        }),
        Some("select") => {
            v.is_string()
                && s["options"]
                    .as_array()
                    .is_some_and(|opts| opts.iter().any(|x| x["value"] == *v))
        }
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(PluginError::invalid("设置值类型、范围或选项无效"))
    }
}
pub fn url(raw: &str, allow_http: bool, external: bool) -> Result<()> {
    if raw.len() > 8192 || raw.chars().any(char::is_control) {
        return Err(PluginError::invalid("资源地址无效"));
    }
    // eD2k uses a pipe-delimited link body, not a URL authority/host.
    if external
        && raw.split_once("://").is_some_and(|(scheme, body)| {
            scheme.eq_ignore_ascii_case("ed2k") && body.starts_with('|') && body.ends_with("|/")
        })
    {
        return Ok(());
    }
    let u = url::Url::parse(raw).map_err(|_| PluginError::invalid("需要绝对资源地址"))?;
    let valid = if external {
        matches!(
            u.scheme(),
            "http" | "https" | "magnet" | "ed2k" | "rtsp" | "rtmp"
        )
    } else {
        u.scheme() == "https" || allow_http && u.scheme() == "http"
    };
    if !valid || !u.username().is_empty() || u.password().is_some() {
        return Err(PluginError::invalid("资源地址协议或认证信息不合法"));
    }
    Ok(())
}
pub(crate) fn object(v: Value) -> Result<Map<String, Value>> {
    v.as_object()
        .cloned()
        .ok_or_else(|| PluginError::internal("插件返回值必须为对象"))
}
pub(crate) fn required(o: &Map<String, Value>, key: &str) -> Result<String> {
    o.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .ok_or_else(|| PluginError::internal(format!("返回值缺少 {key}")))
}
/// 分集 id 的长度上限（字节）。网盘分集 id 本身就是分享链接，实测最长
/// 565 字节（王二小·玩偶哥哥，2026-11-25），256 不够用；其它 id
/// （作品、线路等）仍走 256。
pub(crate) const EPISODE_ID_MAX: usize = 2048;

pub(crate) fn id_max(o: &Map<String, Value>, key: &str, max: usize) -> Result<()> {
    if required(o, key)?.len() > max {
        Err(PluginError::internal(format!("{key} 超长")))
    } else {
        Ok(())
    }
}

pub(crate) fn id(o: &Map<String, Value>, key: &str) -> Result<()> {
    id_max(o, key, 256)
}
pub(crate) fn keep(o: &mut Map<String, Value>, keys: &[&str]) {
    o.retain(|k, _| keys.contains(&k.as_str()));
}
pub(crate) fn text(o: &mut Map<String, Value>, key: &str, max: usize) -> Result<()> {
    if let Some(v) = o.get_mut(key) {
        let s = v
            .as_str()
            .ok_or_else(|| PluginError::internal(format!("{key} 必须为文本")))?;
        *v = Value::String(s.chars().take(max).collect());
    }
    Ok(())
}
pub fn media_item(v: Value, http: bool) -> Result<Value> {
    let mut o = object(v)?;
    id(&o, "id")?;
    required(&o, "title")?;
    keep(
        &mut o,
        &[
            "id",
            "title",
            "type",
            "cover",
            "backdrop",
            "description",
            "tags",
            "score",
            "year",
            "status",
            "episodeCount",
            "totalEpisodes",
            "durationSec",
            "stats",
            "author",
            "publishedAt",
            "updatedAt",
            "badge",
            "entryEpisodeId",
        ],
    );
    for k in ["title", "badge", "description"] {
        text(
            &mut o,
            k,
            if k == "badge" {
                8
            } else if k == "description" {
                5000
            } else {
                200
            },
        )?;
    }
    if let Some(v) = o.remove("tags") {
        o.insert(
            "tags".into(),
            list(v, 100, |v| {
                v.as_str()
                    .map(|s| json!(s.chars().take(60).collect::<String>()))
                    .ok_or_else(|| PluginError::internal("tags 必须为文本"))
            })?,
        );
    }
    for k in ["score", "durationSec"] {
        if o.get(k)
            .is_some_and(|v| !v.as_f64().is_some_and(|n| n.is_finite() && n >= 0.0))
        {
            return Err(PluginError::internal(format!("{k} 必须为非负数")));
        }
    }
    if let Some(v) = o.remove("author") {
        let mut a = object(v)?;
        id(&a, "id")?;
        required(&a, "name")?;
        text(&mut a, "name", 200)?;
        text(&mut a, "description", 5000)?;
        if let Some(v) = a.remove("avatar") {
            a.insert("avatar".into(), resource(v, http)?);
        }
        if a.get("followers").is_some_and(|v| !v.as_u64().is_some()) {
            return Err(PluginError::internal("followers 必须为非负整数"));
        }
        keep(
            &mut a,
            &["id", "name", "description", "avatar", "followers"],
        );
        o.insert("author".into(), Value::Object(a));
    }
    if let Some(v) = o.remove("stats") {
        let mut stats = object(v)?;
        keep(
            &mut stats,
            &["plays", "hot", "likes", "favorites", "comments"],
        );
        if stats.values().any(|v| {
            !v.as_f64()
                .is_some_and(|n| n.is_finite() && n >= 0.0 && n.fract() == 0.0)
        }) {
            return Err(PluginError::internal("stats 必须为非负整数"));
        }
        o.insert("stats".into(), Value::Object(stats));
    }
    if o.contains_key("entryEpisodeId") {
        // 入口分集 id 也是分集 id，走同一上限。
        id_max(&o, "entryEpisodeId", EPISODE_ID_MAX)?;
    }
    if o.get("status")
        .is_some_and(|v| !matches!(v.as_str(), Some("ongoing" | "finished" | "upcoming")))
    {
        return Err(PluginError::internal("status 无效"));
    }
    for k in ["cover", "backdrop"] {
        if let Some(v) = o.remove(k) {
            o.insert(k.into(), resource(v, http)?);
        }
    }
    for k in [
        "year",
        "episodeCount",
        "totalEpisodes",
        "publishedAt",
        "updatedAt",
    ] {
        if let Some(v) = o.get(k) {
            if !v.as_f64().is_some_and(|x| x >= 0.0 && x.fract() == 0.0) {
                return Err(PluginError::internal(format!("{k} 必须为非负整数")));
            }
        }
    }
    if let Some(v) = o.get("type") {
        if !matches!(
            v.as_str(),
            Some(
                "movie"
                    | "series"
                    | "anime"
                    | "variety"
                    | "documentary"
                    | "short"
                    | "live"
                    | "other"
            )
        ) {
            o.insert("type".into(), json!("other"));
        }
    }
    Ok(Value::Object(o))
}
pub fn resource(v: Value, http: bool) -> Result<Value> {
    if let Some(s) = v.as_str() {
        url(s, http, false)?;
        return Ok(v);
    }
    let mut o = object(v)?;
    url(&required(&o, "url")?, http, false)?;
    keep(&mut o, &["url", "headers", "transform"]);
    Ok(Value::Object(o))
}
fn episode(v: Value, http: bool) -> Result<Value> {
    let mut o = object(v)?;
    // 分集 id 走 EPISODE_ID_MAX：网盘分集 id 是分享链接（见上）。
    id_max(&o, "id", EPISODE_ID_MAX)?;
    if !o
        .get("index")
        .and_then(Value::as_f64)
        .is_some_and(|x| x >= 1.0 && x.fract() == 0.0)
    {
        return Err(PluginError::internal("分集 index 必须为正整数"));
    }
    keep(
        &mut o,
        &[
            "id",
            "index",
            "season",
            "episodeNumber",
            "label",
            "title",
            "durationSec",
            "airDate",
            "thumbnail",
            "locked",
            "lockReason",
        ],
    );
    if let Some(v) = o.remove("thumbnail") {
        o.insert("thumbnail".into(), resource(v, http)?);
    }
    for k in ["season", "airDate"] {
        if o.get(k).is_some_and(|v| {
            !v.as_f64()
                .is_some_and(|n| n.is_finite() && n >= 0.0 && n.fract() == 0.0)
        }) {
            return Err(PluginError::internal(format!("{k} 必须为非负整数")));
        }
    }
    for k in ["episodeNumber", "durationSec"] {
        if o.get(k)
            .is_some_and(|v| !v.as_f64().is_some_and(|n| n.is_finite() && n >= 0.0))
        {
            return Err(PluginError::internal(format!("{k} 必须为非负数")));
        }
    }
    for k in ["label", "title", "lockReason"] {
        text(&mut o, k, 200)?;
    }
    if o.get("locked").is_some_and(|v| !v.is_boolean()) {
        return Err(PluginError::internal("locked 必须为布尔值"));
    }
    Ok(Value::Object(o))
}
fn list<F: Fn(Value) -> Result<Value>>(v: Value, max: usize, f: F) -> Result<Value> {
    let a = v
        .as_array()
        .ok_or_else(|| PluginError::internal("返回值应为数组"))?;
    Ok(Value::Array(
        a.iter()
            .take(max)
            .cloned()
            .filter_map(|v| f(v).ok())
            .collect(),
    ))
}
pub fn playback(v: Value, http: bool, resolver: bool) -> Result<Value> {
    let mut o = object(v)?;
    let streams = list(o.remove("streams").unwrap_or(Value::Null), 16, |v| {
        let mut s = object(v)?;
        let kind = s.get("kind").and_then(Value::as_str).unwrap_or("media");
        if !matches!(kind, "media" | "page" | "external") || resolver && kind == "page" {
            return Err(PluginError::internal("无效流类型或递归解析"));
        }
        url(&required(&s, "url")?, http, kind == "external")?;
        if kind == "media"
            && !matches!(
                s.get("format").and_then(Value::as_str),
                Some("hls" | "progressive" | "dash")
            )
        {
            return Err(PluginError::internal("直接流缺少 format"));
        }
        for key in ["label", "serverId", "resolverId"] {
            text(&mut s, key, if key == "label" { 200 } else { 256 })?;
        }
        if let Some(external) = s.remove("external") {
            let mut hint = object(external)?;
            text(&mut hint, "app", 200)?;
            text(&mut hint, "mime", 200)?;
            keep(&mut hint, &["app", "mime"]);
            s.insert("external".into(), Value::Object(hint));
        }
        for key in ["height", "bitrate"] {
            if s.get(key)
                .is_some_and(|v| !v.as_u64().is_some_and(|n| n > 0))
            {
                return Err(PluginError::internal(format!("{key} 必须为正整数")));
            }
        }
        if s.get("codec")
            .is_some_and(|v| !matches!(v.as_str(), Some("h264" | "hevc" | "av1")))
        {
            return Err(PluginError::internal("无效视频编码"));
        }
        keep(
            &mut s,
            &[
                "url",
                "kind",
                "format",
                "codec",
                "label",
                "height",
                "bitrate",
                "serverId",
                "live",
                "headers",
                "resolverId",
                "transforms",
                "external",
            ],
        );
        Ok(Value::Object(s))
    })?;
    if streams.as_array().is_none_or(Vec::is_empty) {
        return Err(PluginError::internal("没有合法可播放流"));
    }
    o.insert("streams".into(), streams);
    keep(
        &mut o,
        &[
            "streams",
            "servers",
            "subtitles",
            "expiresAt",
            "durationSec",
            "resumeSec",
            "skip",
        ],
    );
    if let Some(v) = o.remove("servers") {
        let servers = list(v, 20, |v| {
            let mut server = object(v)?;
            id(&server, "id")?;
            text(&mut server, "label", 200)?;
            keep(&mut server, &["id", "label"]);
            Ok(Value::Object(server))
        })?;
        let mut seen = std::collections::HashSet::new();
        o.insert(
            "servers".into(),
            json!(servers
                .as_array()
                .unwrap()
                .iter()
                .filter(|s| seen.insert(s["id"].to_string()))
                .collect::<Vec<_>>()),
        );
    }
    if let Some(v) = o.remove("subtitles") {
        o.insert(
            "subtitles".into(),
            list(v, 32, |v| {
                let mut s = object(v)?;
                url(&required(&s, "url")?, http, false)?;
                keep(
                    &mut s,
                    &["url", "format", "language", "label", "default", "headers"],
                );
                Ok(Value::Object(s))
            })?,
        );
    }
    Ok(Value::Object(o))
}
pub fn validate_result(method: &str, v: Value, http: bool) -> Result<Value> {
    validate_result_with_warnings(method, v, http, &mut Vec::new())
}
pub(crate) fn validate_result_with_warnings(
    method: &str,
    v: Value,
    http: bool,
    warnings: &mut Vec<String>,
) -> Result<Value> {
    if crate::forms::supports(method) {
        return crate::forms::validate(method, v, http, warnings);
    }
    if crate::content::supports(method) {
        return crate::content::validate(method, v, http, warnings);
    }
    match method {
        "media.getDetail" => {
            let mut o = object(v.clone())?;
            let playlists = list(o.remove("playlists").unwrap_or(Value::Null), 20, |v| {
                let mut p = object(v)?;
                id(&p, "id")?;
                let eps = list(p.remove("episodes").unwrap_or(Value::Null), 5000, |v| {
                    episode(v, http)
                })?;
                p.insert("episodes".into(), eps);
                keep(&mut p, &["id", "label", "kind", "episodes", "next"]);
                Ok(Value::Object(p))
            })?;
            if playlists.as_array().is_none_or(Vec::is_empty) {
                return Err(PluginError::internal("详情缺少合法播放列表"));
            }
            let mut item = object(media_item(v, http)?)?;
            item.insert("playlists".into(), playlists);
            if let Some(v) = o.remove("seasons") {
                item.insert(
                    "seasons".into(),
                    list(v, 100, |v| {
                        let mut s = object(v)?;
                        if !s["number"].as_u64().is_some() {
                            return Err(PluginError::internal("season.number 无效"));
                        }
                        required(&s, "label")?;
                        text(&mut s, "label", 200)?;
                        keep(&mut s, &["number", "label"]);
                        Ok(Value::Object(s))
                    })?,
                );
            }
            if let Some(v) = o.remove("related") {
                item.insert("related".into(), list(v, 100, |v| media_item(v, http))?);
            }
            if let Some(v) = o.remove("interaction") {
                item.insert("interaction".into(), crate::content::interaction(v)?);
            }
            for k in ["people", "trailers"] {
                if let Some(v) = o.remove(k) {
                    item.insert(k.into(), v);
                }
            }
            Ok(Value::Object(item))
        }
        "search.search" | "feed.list" | "home.listSection" | "catalog.list"
        | "media.listEpisodes" => {
            let mut o = object(v)?;
            let items = list(o.remove("items").unwrap_or(Value::Null), 100, |v| {
                if method == "media.listEpisodes" {
                    episode(v, http)
                } else {
                    media_item(v, http)
                }
            })?;
            keep(&mut o, &["next", "total"]);
            if o.get("next")
                .is_some_and(|v| !v.is_null() && !v.is_string())
            {
                return Err(PluginError::internal("next 必须为字符串或 null"));
            }
            o.insert("items".into(), items);
            Ok(Value::Object(o))
        }
        "media.resolvePlayback" | "resolver.resolve" | "live.resolve" => {
            playback(v, http, method == "resolver.resolve")
        }
        "endpoints.list" => {
            let v = list(v, 100, |v| {
                let mut o = object(v)?;
                id(&o, "id")?;
                let raw = required(&o, "url")?;
                url(&raw, http, false)?;
                let u = url::Url::parse(&raw).unwrap();
                if u.path() != "/" || u.query().is_some() || u.fragment().is_some() {
                    return Err(PluginError::internal("线路必须为纯 origin"));
                }
                o.insert("url".into(), json!(u.origin().ascii_serialization()));
                keep(&mut o, &["id", "label", "url"]);
                Ok(Value::Object(o))
            })?;
            let mut ids = std::collections::HashSet::new();
            let mut urls = std::collections::HashSet::new();
            if v.as_array().unwrap().iter().any(|v| {
                !ids.insert(v["id"].clone().to_string())
                    || !urls.insert(v["url"].clone().to_string())
            }) {
                return Err(PluginError::internal("线路 ID 或 origin 重复"));
            }
            Ok(v)
        }
        "media.matchUrl" => {
            if v.is_null() {
                Ok(v)
            } else {
                let mut o = object(v)?;
                id(&o, "mediaId")?;
                keep(&mut o, &["mediaId", "episodeId"]);
                Ok(Value::Object(o))
            }
        }
        "search.suggest" | "search.trending" => list(v, 100, |v| {
            v.as_str()
                .map(|s| json!(s.chars().take(200).collect::<String>()))
                .ok_or_else(|| PluginError::internal("联想应为字符串"))
        }),
        _ => Err(PluginError::new(
            "UNSUPPORTED",
            format!("尚未实现该返回类型校验：{method}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn episode_allows_long_pan_id() {
        // 网盘分集 id 是分享链接，实测 565 字节；2048 内应通过。
        let id = "p".repeat(600);
        let v = json!({"id": id, "index": 1});
        assert!(episode(v, false).is_ok());
    }

    #[test]
    fn episode_rejects_overlong_id() {
        let id = "p".repeat(3000);
        let v = json!({"id": id, "index": 1});
        assert!(episode(v, false).is_err());
    }

    #[test]
    fn media_item_keeps_256_limit() {
        // 作品 id 仍走 256。
        let id = "m".repeat(300);
        let v = json!({"id": id, "title": "t"});
        assert!(media_item(v, false).is_err());
    }

    #[test]
    fn media_item_entry_episode_id_uses_episode_limit() {
        // 入口分集 id 也是分集 id，走同一上限。
        let id = "p".repeat(600);
        let v = json!({"id": "vid1", "title": "t", "entryEpisodeId": id});
        assert!(media_item(v, false).is_ok());
    }

    #[test]
    fn get_detail_keeps_long_pan_episodes() {
        // 回归：长网盘分集 id 不应再被校验丢掉（2026-11-25 实测 565 字节全丢）。
        let id = "p".repeat(600);
        let v = json!({
            "id": "vid1",
            "title": "t",
            "playlists": [{"id": "p1", "episodes": [{"id": id, "index": 1}]}],
        });
        let out = validate_result("media.getDetail", v, false).unwrap();
        let eps = out["playlists"][0]["episodes"].as_array().unwrap();
        assert_eq!(eps.len(), 1);
        assert_eq!(eps[0]["id"].as_str(), Some(id.as_str()));
    }
}
