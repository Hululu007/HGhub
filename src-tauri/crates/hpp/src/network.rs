use crate::{
    model::Meta,
    native::Logs,
    network_policy,
    node::{NodeBootstrap, NodeClient},
    store::{atomic_write, now},
    wire::Transfer,
    PluginError, Result,
};
use wreq::{
    header::{HeaderMap, HeaderName, HeaderValue},
    Method, Response,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use url::Url;

mod api;
pub use api::Queue;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SavedCookie {
    name: String,
    value: String,
    domain: String,
    path: String,
    host_only: bool,
    secure: bool,
    http_only: bool,
    expires_at: Option<u64>,
}
#[derive(Clone, Serialize, Deserialize)]
struct WebLoginSession {
    generation: String,
    cookies: Vec<SavedCookie>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct CookieJars {
    ordinary: Vec<SavedCookie>,
    // Network belongs to a single plugin. URL origins include the scheme and port.
    web_logins: HashMap<String, WebLoginSession>,
}

/// Cloudflare 机器人管理 Cookie：挑战页发的 `__cf_bm` 只在当次挑战有效，
/// 存下来下次再发只会让 Cloudflare 一直重新挑战，所以宿主不存、不发。
/// （见 docs/developers/node-engine.md §7）
const BOT_MANAGEMENT_COOKIE: &str = "__cf_bm";

/// WebView clearance 拿到的通行证 Cookie 名：内存存储只认这一个；
/// `__cf_bm` 绝不进这份存储（v0.0.75 的教训）。
const CLEARANCE_COOKIE: &str = "cf_clearance";

/// WebView clearance 拿到的通行证（v0.0.77）：只在内存里保存，不落盘、
/// 不进 Cookie 罐；域名/路径/过期按普通 Cookie 规则匹配，过期即失效。
#[derive(Clone, Debug)]
pub struct ClearanceCookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub secure: bool,
    pub http_only: bool,
    pub expires_at: Option<u64>,
}

/// 响应体是不是 Cloudflare 拦截页：页面 `_getInner` 用的同一组标记
/// （`just a moment` / `cf-browser` / `access denied`），另加拦截页
/// 常见的 `attention required` 与挑战表单标记 `cf_chl`。只做子串匹配，
/// 不解析 HTML——拦截页是 HTTP 200，状态码分不出来。
pub fn looks_like_challenge(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    ["just a moment", "cf-browser", "access denied", "attention required", "cf_chl"]
        .iter()
        .any(|m| lower.contains(m))
}

impl CookieJars {
    fn len(&self) -> usize {
        self.ordinary.len()
            + self
                .web_logins
                .values()
                .map(|s| s.cookies.len())
                .sum::<usize>()
    }

    /// 清掉罐子里残留的机器人管理 Cookie，返回清掉的条数。
    fn purge_bot_cookies(&mut self) -> usize {
        let before = self.len();
        self.ordinary
            .retain(|c| !c.name.eq_ignore_ascii_case(BOT_MANAGEMENT_COOKIE));
        for s in self.web_logins.values_mut() {
            s.cookies
                .retain(|c| !c.name.eq_ignore_ascii_case(BOT_MANAGEMENT_COOKIE));
        }
        before - self.len()
    }
}

pub struct Network {
    pub meta: Meta,
    pub strict_override: Option<bool>,
    jar: Mutex<CookieJars>,
    jar_path: PathBuf,
    gate: Arc<tokio::sync::Semaphore>,
    hosts: Mutex<HashMap<String, Weak<tokio::sync::Semaphore>>>,
    inflight: Mutex<HashMap<String, Arc<api::Flight>>>,
    rate: Mutex<(f64, Instant)>,
    cache: Mutex<HashMap<String, CachedResponse>>,
    clients: tokio::sync::Mutex<HashMap<String, (Instant, wreq::Client)>>,
    pub cooldown: Arc<Mutex<HashMap<String, Instant>>>,
    /// Node 边车引导：iOS 上启动真 Node 引擎，非 iOS 用桩实现。
    /// `PluginManager::with_node_bootstrap` 注入后会覆盖这里的默认
    /// 桩（管理器先建、引导后注，见 `set_node_bootstrap`）。
    node_bootstrap: Mutex<Arc<dyn NodeBootstrap>>,
    /// 已建好的 Node 边车客户端（地址可能换端口，重探后换新）。
    node_client: Mutex<Option<Arc<NodeClient>>>,
    /// WebView clearance 拿到的通行证（v0.0.77）：只在内存里按插件
    /// 保存，不落盘、不进 Cookie 罐；`__cf_bm` 绝不进这里。
    clearance: Mutex<Vec<SavedCookie>>,
    /// 该插件的 ext.log 面板（v0.0.78）：`Installed::installed` 在装配
    /// 时绑定；net.request 的代理决议写进这里。没绑定时不记（测试用
    /// 的 Network 没有日志面板）。
    logs: Mutex<Option<Logs>>,
    /// 仅测试：注入的代理决议（见 `proxy_decision`）。
    #[cfg(test)]
    proxy_override: Mutex<Option<(crate::proxy::Mode, Option<String>)>>,
}
#[derive(Clone)]
struct RedactedSetCookie;

struct CachedResponse {
    value: Transfer,
    expires: Instant,
    bytes: usize,
}
impl Network {
    pub fn new(
        meta: Meta,
        dir: PathBuf,
        strict: Option<bool>,
        cooldown: Arc<Mutex<HashMap<String, Instant>>>,
        node_bootstrap: Arc<dyn NodeBootstrap>,
    ) -> Result<Self> {
        let jar_path = dir.join("cookies.json");
        let mut jar = if jar_path.exists() {
            #[derive(Deserialize)]
            #[serde(untagged)]
            enum DiskJar {
                Ordinary(Vec<SavedCookie>),
                WithWebLogins(CookieJars),
            }
            match serde_json::from_slice(&std::fs::read(&jar_path)?)
                .map_err(|_| PluginError::internal("Cookie 存储损坏"))?
            {
                DiskJar::Ordinary(ordinary) => CookieJars {
                    ordinary,
                    ..Default::default()
                },
                DiskJar::WithWebLogins(jars) => jars,
            }
        } else {
            CookieJars::default()
        };
        // 挑战页发的 __cf_bm 只在当次挑战有效，存下来下次再发只会让
        // Cloudflare 一直重新挑战：加载时把罐子里残留的清掉并回写。
        let purged = jar.purge_bot_cookies();
        let this = Self {
            meta,
            strict_override: strict,
            jar: Mutex::new(jar),
            jar_path,
            gate: Arc::new(tokio::sync::Semaphore::new(6)),
            hosts: Default::default(),
            inflight: Default::default(),
            rate: Mutex::new((15.0, Instant::now())),
            cache: Mutex::new(HashMap::new()),
            clients: Default::default(),
            cooldown,
            node_bootstrap: Mutex::new(node_bootstrap),
            node_client: Default::default(),
            clearance: Default::default(),
            logs: Default::default(),
            #[cfg(test)]
            proxy_override: Default::default(),
        };
        if purged > 0 {
            let jars = this.jar.lock().unwrap().clone();
            this.save_jar(&jars)?;
        }
        Ok(this)
    }
    /// 注入 Node 边车引导（`PluginManager::with_node_bootstrap` 用）：
    /// 换引导即换边车地址，旧客户端作废。
    pub fn set_node_bootstrap(&self, bootstrap: Arc<dyn NodeBootstrap>) {
        *self.node_bootstrap.lock().unwrap() = bootstrap;
        *self.node_client.lock().unwrap() = None;
    }
    /// 绑定该插件的 ext.log 面板（v0.0.78）：manager 在装配 Installed
    /// 时调（见 `set_node_bootstrap` 的先例）。绑定后 net.request 的
    /// 代理决议写进该面板。
    pub(crate) fn set_logs(&self, logs: Logs) {
        *self.logs.lock().unwrap() = Some(logs);
    }

    /// 本跳的代理决议（v0.0.78）：生产走全局设置快照（模式 + 解析后
    /// 的代理 URL）。测试用 `set_proxy_decision` 注入：全局代理状态
    /// 是进程共享的（`proxy::snapshot()` + `HGHUB_PROXY` 环境变量），
    /// 测试里直接改它会互相干扰，并行跑还会竞态。
    fn proxy_decision(&self) -> (crate::proxy::Mode, Option<String>) {
        #[cfg(test)]
        if let Some(d) = self.proxy_override.lock().unwrap().clone() {
            return d;
        }
        let config = crate::proxy::snapshot();
        (config.mode, crate::proxy::effective_proxy_url(&config))
    }

    #[cfg(test)]
    fn set_proxy_decision(&self, mode: crate::proxy::Mode, proxy_url: Option<String>) {
        *self.proxy_override.lock().unwrap() = Some((mode, proxy_url));
    }
    /// 存入 WebView clearance 拿到的通行证（v0.0.77）：只认
    /// `cf_clearance`，其他名字（包括 `__cf_bm`）一律不收；过期
    /// （或已过期）的不要。只在内存里保存，不落盘、不进 Cookie 罐。
    pub fn set_clearance(&self, cookie: ClearanceCookie) {
        if !cookie.name.eq_ignore_ascii_case(CLEARANCE_COOKIE) {
            return;
        }
        let domain = cookie.domain.trim_start_matches('.').to_ascii_lowercase();
        if domain.is_empty() || cookie.value.is_empty() {
            return;
        }
        if cookie.expires_at.is_some_and(|t| t <= now()) {
            return;
        }
        let c = SavedCookie {
            name: cookie.name,
            value: cookie.value,
            domain,
            path: if cookie.path.is_empty() {
                "/".into()
            } else {
                cookie.path
            },
            host_only: false,
            // Secure / HttpOnly 取 WebView 里那只 Cookie 自身的属性，
            // 不硬编码：Secure 的通行证不能发给 http 目标（浏览器同款
            // 规则；真站点的 cf_clearance 本来就是 Secure 的）。
            secure: cookie.secure,
            http_only: cookie.http_only,
            expires_at: cookie.expires_at,
        };
        Self::upsert(&mut self.clearance.lock().unwrap(), c);
    }
    /// 匹配目标 URL 的通行证（`name=value` 串，已按路径长度排过序）：
    /// 请求头里已有同名 Cookie 的不重复带；`__cf_bm` 永不从这里出去。
    fn clearance_cookies(&self, url: &Url, existing_cookie: Option<&str>) -> String {
        let present: Vec<&str> = existing_cookie
            .map(|v| {
                v.split(';')
                    .filter_map(|p| p.trim().split('=').next())
                    .collect()
            })
            .unwrap_or_default();
        let guard = self.clearance.lock().unwrap();
        let mut matched: Vec<_> = guard.iter().filter(|c| Self::matches(c, url)).collect();
        matched.sort_by_key(|c| std::cmp::Reverse(c.path.len()));
        matched
            .into_iter()
            .filter(|c| {
                !c.name.eq_ignore_ascii_case(BOT_MANAGEMENT_COOKIE)
                    && !present.iter().any(|n| n.eq_ignore_ascii_case(&c.name))
            })
            .map(|c| format!("{}={}", c.name, c.value))
            .collect::<Vec<_>>()
            .join("; ")
    }
    /// 插件 net 请求走不走 Node 传输（iOS + 引导可用才走）。
    pub(crate) fn node_transport(&self) -> bool {
        self.node_bootstrap.lock().unwrap().is_available()
    }
    pub fn strict(&self) -> bool {
        self.strict_override
            .unwrap_or_else(network_policy::strict_security)
    }
    pub fn target(&self, raw: &str) -> Result<Url> {
        // WebHome 页面是普通网页，其站点可能是 http：带 webhome 声明的
        // 插件，宿主对其网络请求放行 http；普通插件仍按 allowInsecureHttp 门禁。
        let allow_http = self.meta.allow_insecure_http || self.meta.webhome.is_some();
        crate::model::url(raw, allow_http, false)?;
        network_policy::parse_target_with_mode(raw, self.strict())
            .map_err(|e| PluginError::new("NETWORK", e))
    }
    fn matches(c: &SavedCookie, u: &Url) -> bool {
        let h = u.host_str().unwrap_or("");
        let path = u.path();
        let domain = h == c.domain || !c.host_only && h.ends_with(&format!(".{}", c.domain));
        let p = path == c.path
            || path.starts_with(&c.path)
                && (c.path.ends_with('/') || path.as_bytes().get(c.path.len()) == Some(&b'/'));
        domain
            && p
            && (!c.secure || u.scheme() == "https")
            && c.expires_at.is_none_or(|t| t > now())
    }
    fn save_jar(&self, jars: &CookieJars) -> Result<()> {
        let count = jars.len();
        if count > 3000 {
            return Err(PluginError::invalid("Cookie 数量超限"));
        }
        let encoded = if jars.web_logins.is_empty() {
            serde_json::to_vec(&jars.ordinary)
        } else {
            serde_json::to_vec(jars)
        }
        .map_err(|_| PluginError::internal("无法保存 Cookie"))?;
        atomic_write(&self.jar_path, &encoded)
    }
    fn parse_cookie(u: &Url, v: Value) -> Result<SavedCookie> {
        let host = u
            .host_str()
            .ok_or_else(|| PluginError::invalid("Cookie URL 缺少主机"))?;
        let domain = v["domain"]
            .as_str()
            .unwrap_or(host)
            .trim_start_matches('.')
            .to_ascii_lowercase();
        if domain != host && !host.ends_with(&format!(".{domain}")) {
            return Err(PluginError::invalid("Cookie domain 与 URL 不匹配"));
        }
        let host_only = v["hostOnly"].as_bool().unwrap_or(v.get("domain").is_none());
        if host_only && domain != host {
            return Err(PluginError::invalid("hostOnly Cookie 必须绑定当前主机"));
        }
        let c = SavedCookie {
            name: v["name"]
                .as_str()
                .ok_or_else(|| PluginError::invalid("Cookie 缺少名称"))?
                .into(),
            value: v["value"]
                .as_str()
                .ok_or_else(|| PluginError::invalid("Cookie 缺少值"))?
                .into(),
            domain,
            path: v["path"].as_str().unwrap_or("/").into(),
            host_only,
            secure: v["secure"].as_bool().unwrap_or(false),
            http_only: v["httpOnly"].as_bool().unwrap_or(false),
            expires_at: v["expiresAt"].as_u64(),
        };
        if c.name.is_empty()
            || c.name.len() + c.value.len() > 16384
            || c.name.chars().any(|c| c.is_control() || ";= ".contains(c))
            || c.value.chars().any(|c| c.is_control() || c == ';')
        {
            return Err(PluginError::invalid("Cookie 名称或值无效"));
        }
        Ok(c)
    }
    fn upsert(jar: &mut Vec<SavedCookie>, c: SavedCookie) {
        jar.retain(|x| !(x.name == c.name && x.domain == c.domain && x.path == c.path));
        if c.expires_at.is_none_or(|t| t > now()) {
            jar.push(c);
        }
    }
    // Called only by the native host after a successful, first-party login.
    pub(crate) fn replace_web_login(
        &self,
        initial: &str,
        final_url: &str,
        values: Vec<Value>,
    ) -> Result<()> {
        let initial = self.target(initial)?;
        let final_url = self.target(final_url)?;
        if initial.scheme() != "https" || initial.origin() != final_url.origin() {
            return Err(PluginError::invalid("网页登录必须在起始 HTTPS origin 完成"));
        }
        if values.len() > 3000 {
            return Err(PluginError::invalid("网页登录 Cookie 数量无效"));
        }
        let mut cookies = Vec::new();
        for value in values {
            // The native adapter must supply the original scope, including host-only.
            if !value["hostOnly"].is_boolean()
                || !value["secure"].is_boolean()
                || !value["httpOnly"].is_boolean()
                || !value["domain"].is_string()
                || !value["path"].as_str().is_some_and(|p| p.starts_with('/'))
                || !(value["expiresAt"].is_null() || value["expiresAt"].is_u64())
            {
                return Err(PluginError::invalid("缺少有效的网页登录 Cookie 属性"));
            }
            let c = Self::parse_cookie(&final_url, value)?;
            if !Self::matches(&c, &final_url) {
                return Err(PluginError::invalid("Cookie 与登录完成地址不匹配"));
            }
            Self::upsert(&mut cookies, c);
        }
        let mut guard = self.jar.lock().unwrap();
        let mut next = guard.clone();
        let origin = initial.origin().ascii_serialization();
        if !next.web_logins.contains_key(&origin) && next.web_logins.len() >= 3000 {
            return Err(PluginError::invalid("网页登录站点数量超限"));
        }
        next.web_logins.insert(
            origin,
            WebLoginSession {
                generation: uuid::Uuid::new_v4().to_string(),
                cookies,
            },
        );
        self.save_jar(&next)?;
        *guard = next;
        Ok(())
    }
    pub fn cookies(&self, op: &str, p: Value) -> Result<Value> {
        let u = p["url"].as_str().map(|s| self.target(s)).transpose()?;
        match op {
            "get" => {
                let u = u.ok_or_else(|| PluginError::invalid("缺少 Cookie URL"))?;
                Ok(json!(self
                    .jar
                    .lock()
                    .unwrap()
                    .ordinary
                    .iter()
                    .filter(|c| Self::matches(c, &u))
                    .collect::<Vec<_>>()))
            }
            "set" => {
                let c = Self::parse_cookie(
                    &u.ok_or_else(|| PluginError::invalid("缺少 Cookie URL"))?,
                    p["cookie"].clone(),
                )?;
                let mut guard = self.jar.lock().unwrap();
                let mut next = guard.clone();
                Self::upsert(&mut next.ordinary, c);
                self.save_jar(&next)?;
                *guard = next;
                Ok(Value::Null)
            }
            "clear" => {
                let mut jar = self.jar.lock().unwrap();
                let mut next = jar.clone();
                next.ordinary
                    .retain(|c| u.as_ref().is_some_and(|u| !Self::matches(c, u)));
                if let Some(url) = u {
                    next.web_logins.remove(&url.origin().ascii_serialization());
                } else {
                    next.web_logins.clear();
                }
                self.save_jar(&next)?;
                *jar = next;
                Ok(Value::Null)
            }
            _ => Err(PluginError::invalid("未知 Cookie 操作")),
        }
    }
    fn request_cookies(&self, url: &Url, attach: bool) -> (Option<String>, String) {
        let jars = self.jar.lock().unwrap();
        let session = jars.web_logins.get(&url.origin().ascii_serialization());
        let generation = session.map(|s| s.generation.clone());
        if !attach {
            return (generation, String::new());
        }
        let cookies = session.map_or(&jars.ordinary, |s| &s.cookies);
        let mut matched: Vec<_> = cookies.iter().filter(|c| Self::matches(c, url)).collect();
        matched.sort_by_key(|c| std::cmp::Reverse(c.path.len()));
        (
            generation,
            matched
                .iter()
                .map(|c| format!("{}={}", c.name, c.value))
                .collect::<Vec<_>>()
                .join("; "),
        )
    }
    fn receive_cookies(
        &self,
        u: &Url,
        headers: &mut HeaderMap,
        sent_generation: Option<&str>,
        store: bool,
    ) -> Result<bool> {
        if !headers.contains_key("set-cookie") {
            return Ok(false);
        }
        let mut guard = self.jar.lock().unwrap();
        let origin = u.origin().ascii_serialization();
        let current = guard.web_logins.get(&origin).map(|s| s.generation.as_str());
        let hidden = sent_generation.is_some() || current.is_some();
        // A late response after logout or account replacement cannot revive that session.
        if !store || sent_generation != current {
            if hidden {
                headers.remove("set-cookie");
            }
            return Ok(hidden);
        }
        let mut next = guard.clone();
        let total = next.len();
        let jar = next
            .web_logins
            .get_mut(&origin)
            .map_or(&mut next.ordinary, |s| &mut s.cookies);
        let capacity = 3000 - (total - jar.len());
        for h in headers.get_all("set-cookie") {
            if let Ok(s) = h.to_str() {
                if let Ok(c) = cookie::Cookie::parse(s.to_string()) {
                    // 挑战页的机器人管理 Cookie 不存：它只在当次挑战有效，
                    // 存下来下次再发只会让 Cloudflare 一直重新挑战。
                    if c.name().eq_ignore_ascii_case(BOT_MANAGEMENT_COOKIE) {
                        continue;
                    }
                    let default_path = u
                        .path()
                        .rsplit_once('/')
                        .map(|(s, _)| if s.is_empty() { "/" } else { s })
                        .unwrap_or("/");
                    let mut v = json!({"name":c.name(),"value":c.value(),"path":c.path().unwrap_or(default_path),"secure":c.secure().unwrap_or(false),"httpOnly":c.http_only().unwrap_or(false)});
                    if let Some(d) = c.domain() {
                        v["domain"] = json!(d);
                    }
                    if let Some(age) = c.max_age() {
                        v["expiresAt"] = json!(if age.whole_seconds() <= 0 {
                            0
                        } else {
                            now().saturating_add(age.whole_milliseconds() as u64)
                        });
                    } else if let Some(t) = c.expires_datetime() {
                        v["expiresAt"] = json!((t.unix_timestamp().max(0) as u64) * 1000);
                    }
                    // Ignore individual invalid upstream cookies, as browsers do.
                    if let Ok(c) = Self::parse_cookie(u, v) {
                        Self::upsert(jar, c);
                        if jar.len() > capacity {
                            jar.pop();
                        }
                    }
                }
            }
        }
        if hidden {
            headers.remove("set-cookie");
        }
        self.save_jar(&next)?;
        *guard = next;
        Ok(hidden)
    }
    async fn rate_limit(&self, cancel: &CancellationToken) -> Result<()> {
        loop {
            {
                let mut r = self.rate.lock().unwrap();
                let elapsed = r.1.elapsed().as_secs_f64();
                r.0 = (r.0 + elapsed * 5.0).min(15.0);
                r.1 = Instant::now();
                if r.0 >= 1.0 {
                    r.0 -= 1.0;
                    return Ok(());
                }
            }
            tokio::select! {_=cancel.cancelled()=>return Err(PluginError::cancelled()),_=tokio::time::sleep(Duration::from_millis(50))=>{}}
        }
    }
    // A pool is bound to validated DNS answers and the effective network policy.
    // Revalidate periodically; never reuse a pool across proxy/security changes.
    async fn client(&self, target: &Url, cancel: &CancellationToken) -> Result<wreq::Client> {
        let strict = self.strict();
        let key = format!(
            "{}:{}:{}",
            crate::proxy::generation(),
            strict,
            target.origin().ascii_serialization()
        );
        let mut pools = tokio::select! {
            _ = cancel.cancelled() => return Err(PluginError::cancelled()),
            guard = self.clients.lock() => guard,
        };
        pools.retain(|_, (created, _)| created.elapsed() < Duration::from_secs(60));
        if let Some((_, client)) = pools.get(&key) {
            return Ok(client.clone());
        }
        let addrs = tokio::select! {
            _ = cancel.cancelled() => return Err(PluginError::cancelled()),
            value = network_policy::resolve_target(target, strict) => value.map_err(|e| PluginError::new("NETWORK", e))?,
        };
        // Node 式极简画像（v0.0.69）：插件 net 请求不用浏览器指纹模拟。
        // v0.0.68 的 Chrome 149 模拟 + 页面裸 "Mozilla/5.0" UA 是自相
        // 矛盾的组合，Cloudflare 照拦；Arcane 的 Node 18 https（HTTP/1.1、
        // 无 sec-ch-ua、无浏览器头、裸 UA 原样）同一台手机能过。画像只
        // 作用于插件 net 请求这一路；媒体裸客户端、下载、代理测试、边车
        // 仍用各自的客户端（Chrome 149 模拟保留）。默认 UA 仍是
        // HGhub/HPP-dev；页面自带的 UA 是请求级头，不受这里影响。
        let builder = wreq::Client::builder()
            .http1_only()
            .redirect(wreq::redirect::Policy::none())
            .referer(false)
            .connect_timeout(Duration::from_secs(10))
            .pool_idle_timeout(Duration::from_secs(45))
            .pool_max_idle_per_host(6)
            .user_agent("HGhub/HPP-dev")
            // wreq 的 resolve_to_addrs 只要 SocketAddr 本身（reqwest 接受
            // &Vec<SocketAddr>，wreq 不接受），域名还要 Into<Cow<'static, str>>
            //（reqwest 只要 Into<String>）；addrs 之后不再用，直接移交，
            // host_str 借来的 &str 则 to_owned 成 String 再进 Cow::Owned。
            .resolve_to_addrs(target.host_str().unwrap().to_owned(), addrs);
        let client = crate::proxy::configure(builder, &crate::proxy::snapshot())
            .map_err(|e| PluginError::new("NETWORK", e))?
            .build()
            .map_err(|_| PluginError::new("NETWORK", "无法创建 HTTP 客户端"))?;
        if pools.len() >= 32 {
            if let Some(oldest) = pools
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(key, _)| key.clone())
            {
                pools.remove(&oldest);
            }
        }
        pools.insert(key, (Instant::now(), client.clone()));
        Ok(client)
    }
    pub async fn request(
        &self,
        raw: &str,
        method: Method,
        headers: HeaderMap,
        body: Option<Vec<u8>>,
        cookies: bool,
        follow: bool,
        queue: Queue,
        cancel: &CancellationToken,
    ) -> Result<Response> {
        let mut target = self.target(raw)?;
        let initial = target.origin();
        let mut explicit = headers;
        for hop in 0..=5 {
            if target.origin() != initial {
                explicit.remove("authorization");
                explicit.remove("cookie");
            }
            let outgoing = self.outgoing(
                target.clone(),
                method.clone(),
                explicit.clone(),
                body.clone(),
                cookies,
            )?;
            let response = self.send(outgoing, queue, cancel).await?;
            if let Some(next) =
                self.redirect(&target, response.status(), response.headers(), follow, hop)?
            {
                target = next;
            } else {
                return Ok(response);
            }
        }
        unreachable!("redirect rejects a sixth hop")
    }
    pub async fn fetch(
        self: &Arc<Self>,
        p: Transfer,
        cancel: CancellationToken,
    ) -> Result<Transfer> {
        let v = p.json();
        let effective = headers(&v["headers"])?;
        let method = v["method"].as_str().unwrap_or("GET");
        let retries = v["retry"].as_u64().unwrap_or(0).min(2);
        let eligible = method == "GET"
            && v["credentials"] == "omit"
            && !effective.contains_key("authorization")
            && !effective.contains_key("cookie")
            && v["cache"].is_object();
        let ttl = v["cache"]["ttlSec"]
            .as_f64()
            .unwrap_or(0.0)
            .clamp(0.0, 86400.0);
        let stale = v["cache"]["staleIfErrorSec"]
            .as_f64()
            .unwrap_or(0.0)
            .clamp(0.0, 604800.0);
        let mut key = v.clone();
        if let Some(o) = key.as_object_mut() {
            for k in ["cache", "retry", "timeoutMs"] {
                o.remove(k);
            }
            o.insert(
                "headers".into(),
                json!(effective
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.to_str().unwrap_or("")))
                    .collect::<std::collections::BTreeMap<_, _>>()),
            );
        }
        let key = format!("{}:{}", crate::proxy::generation(), key);
        let mut old = None;
        if eligible {
            if let Some(entry) = self.cache.lock().unwrap().get(&key) {
                if entry.expires > Instant::now() {
                    return Ok(cached(entry.value.clone()));
                }
                if Instant::now().duration_since(entry.expires).as_secs_f64() <= stale {
                    old = Some(entry.value.clone());
                }
            }
        }
        let mut attempt = 0;
        loop {
            let result = self.fetch_once(p.clone(), cancel.clone()).await;
            let failed = match &result {
                Err(e) => {
                    matches!(e.code.as_str(), "NETWORK" | "TIMEOUT")
                        || e.details
                            .as_ref()
                            .and_then(|d| d["status"].as_u64())
                            .is_some_and(|s| s >= 500)
                }
                Ok((v, _)) => v.json()["status"].as_u64().is_some_and(|s| s >= 500),
            };
            if failed && matches!(method, "GET" | "HEAD") && attempt < retries {
                let host = v["url"]
                    .as_str()
                    .and_then(|u| Url::parse(u).ok())
                    .and_then(|u| u.host_str().map(String::from))
                    .unwrap_or_default();
                let cooling = self
                    .cooldown
                    .lock()
                    .unwrap()
                    .get(&host)
                    .is_some_and(|t| *t > Instant::now());
                if !cooling {
                    let delay = if attempt == 0 { 400 } else { 1200 };
                    attempt += 1;
                    tokio::select! {_=cancel.cancelled()=>return Err(PluginError::cancelled()),_=tokio::time::sleep(Duration::from_millis(delay))=>{}};
                    continue;
                }
            }
            if failed {
                if let Some(value) = old {
                    return Ok(cached(value));
                }
            }
            if eligible {
                if let Ok((value, false)) = &result {
                    let response = value.json();
                    let control = response["headers"]["cache-control"]
                        .as_str()
                        .unwrap_or("")
                        .to_ascii_lowercase();
                    if response["ok"] == true
                        && response["setCookies"].as_array().is_some_and(Vec::is_empty)
                        && !response["headers"]["vary"]
                            .as_str()
                            .unwrap_or("")
                            .split(',')
                            .any(|s| s.trim() == "*")
                        && !control.split(',').any(|s| s.trim() == "no-store")
                    {
                        let bytes = serde_json::to_vec(value)
                            .map(|v| v.len())
                            .unwrap_or(usize::MAX);
                        if bytes <= 8 * 1024 * 1024 && ttl > 0.0 {
                            let mut cache = self.cache.lock().unwrap();
                            cache.remove(&key);
                            while !cache.is_empty()
                                && cache.values().map(|e| e.bytes).sum::<usize>() + bytes
                                    > 8 * 1024 * 1024
                            {
                                let oldest = cache
                                    .iter()
                                    .min_by_key(|(_, e)| e.expires)
                                    .map(|(k, _)| k.clone())
                                    .unwrap();
                                cache.remove(&oldest);
                            }
                            cache.insert(
                                key,
                                CachedResponse {
                                    value: value.clone(),
                                    expires: Instant::now() + Duration::from_secs_f64(ttl),
                                    bytes,
                                },
                            );
                        }
                    }
                }
            }
            return result.map(|(value, _)| value);
        }
    }
    async fn fetch_once(
        self: &Arc<Self>,
        p: Transfer,
        cancel: CancellationToken,
    ) -> Result<(Transfer, bool)> {
        let v = p.json();
        let timeout =
            Duration::from_millis(v["timeoutMs"].as_u64().unwrap_or(15000).clamp(1, 30000));
        let work = async {
            let mut u = self.target(
                v["url"]
                    .as_str()
                    .ok_or_else(|| PluginError::invalid("缺少请求 URL"))?,
            )?;
            if let Some(q) = v["query"].as_object() {
                for (k, v) in q {
                    u.query_pairs_mut().append_pair(
                        k,
                        &v.as_str()
                            .map(String::from)
                            .unwrap_or_else(|| v.to_string()),
                    );
                }
            }
            let method = Method::from_bytes(v["method"].as_str().unwrap_or("GET").as_bytes())
                .map_err(|_| PluginError::invalid("请求 method 无效"))?;
            if !matches!(
                method.as_str(),
                "GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE"
            ) {
                return Err(PluginError::invalid("不支持该请求 method"));
            }
            let mut headers = headers(&v["headers"])?;
            let mut body = None;
            if let Some(b) = p.field("body") {
                if method == Method::GET || method == Method::HEAD {
                    return Err(PluginError::invalid("GET/HEAD 不允许 body"));
                }
                let j = b.json();
                body = Some(if let Some(j) = j.get("json") {
                    if !headers.contains_key("content-type") {
                        headers
                            .insert("content-type", HeaderValue::from_static("application/json"));
                    }
                    serde_json::to_vec(j).unwrap()
                } else if let Some(form) = j["form"].as_object() {
                    if !headers.contains_key("content-type") {
                        headers.insert(
                            "content-type",
                            HeaderValue::from_static("application/x-www-form-urlencoded"),
                        );
                    }
                    let mut ser = url::form_urlencoded::Serializer::new(String::new());
                    for (k, v) in form {
                        ser.append_pair(
                            k,
                            v.as_str()
                                .ok_or_else(|| PluginError::invalid("form 值必须为字符串"))?,
                        );
                    }
                    ser.finish().into_bytes()
                } else {
                    b.bytes()?
                });
                if body.as_ref().unwrap().len() > 1024 * 1024 {
                    return Err(PluginError::invalid("请求 body 超过 1 MiB"));
                }
            }
            let follow = body.is_none() && v["redirect"] != "manual";
            let max = v["maxBytes"]
                .as_u64()
                .unwrap_or(2 * 1024 * 1024)
                .clamp(1, 8 * 1024 * 1024) as usize;
            let resp = self
                .buffered(
                    u,
                    method,
                    headers,
                    body,
                    v["credentials"] != "omit",
                    follow,
                    max,
                    timeout,
                    &cancel,
                )
                .await?;
            let mut meta = json!({"status":resp.status.as_u16(),"ok":resp.status.is_success(),"url":resp.url.as_str(),"headers":{},"setCookies":[],"fromCache":false});
            for (k, v) in &resp.headers {
                if let Ok(s) = v.to_str() {
                    meta["headers"][k.as_str()] = json!(s);
                }
            }
            meta["setCookies"] = json!(resp
                .headers
                .get_all("set-cookie")
                .iter()
                .filter_map(|v| v.to_str().ok())
                .collect::<Vec<_>>());
            let charset = v["charset"]
                .as_str()
                .map(String::from)
                .or_else(|| {
                    meta["headers"]["content-type"].as_str().and_then(|s| {
                        s.split(';').find_map(|x| {
                            x.trim()
                                .strip_prefix("charset=")
                                .map(|s| s.trim_matches('"').to_string())
                        })
                    })
                })
                .unwrap_or_else(|| "utf-8".into());
            let data = if v["responseType"] == "bytes" {
                Transfer::Bytes(resp.bytes.clone())
            } else {
                let enc = encoding_rs::Encoding::for_label(charset.as_bytes())
                    .ok_or_else(|| PluginError::invalid("不支持该字符集"))?;
                let (text, _, _) = enc.decode(&resp.bytes);
                if v["responseType"] == "json" {
                    Transfer::from_json(serde_json::from_str(&text).map_err(|_| {
                        let mut error = PluginError::new("UPSTREAM_CHANGED", "响应不是合法 JSON");
                        error.details = Some(json!({"status":meta["status"]}));
                        error
                    })?)
                } else {
                    Transfer::String(text.into_owned())
                }
            };
            let mut result = Transfer::from_json(meta);
            if let Transfer::Object(ref mut m) = result {
                m.insert("data".into(), data);
            }
            result.validate(8 * 1024 * 1024)?;
            Ok((result, resp.redacted))
        };
        tokio::select! {_=cancel.cancelled()=>Err(PluginError::cancelled()),v=tokio::time::timeout(timeout,work)=>v.unwrap_or_else(|_|Err(PluginError::timeout()))}
    }
}
fn cached(mut value: Transfer) -> Transfer {
    if let Transfer::Object(ref mut o) = value {
        o.insert("fromCache".into(), Transfer::Bool(true));
    }
    value
}
pub fn headers(v: &Value) -> Result<HeaderMap> {
    let mut out = HeaderMap::new();
    let mut size = 0;
    if !v.is_null() && !v.is_object() {
        return Err(PluginError::invalid("headers 必须为对象"));
    }
    if let Some(m) = v.as_object() {
        for (k, v) in m {
            let k = HeaderName::from_bytes(k.as_bytes())
                .map_err(|_| PluginError::invalid("请求头名称无效"))?;
            if matches!(
                k.as_str(),
                "host" | "content-length" | "connection" | "transfer-encoding" | "accept-encoding"
            ) {
                continue;
            }
            let s = v
                .as_str()
                .ok_or_else(|| PluginError::invalid("请求头值必须为字符串"))?;
            size += k.as_str().len() + s.len();
            if size > 16384 {
                return Err(PluginError::invalid("请求头超过 16 KiB"));
            }
            out.insert(
                k,
                HeaderValue::from_str(s).map_err(|_| PluginError::invalid("请求头值无效"))?,
            );
        }
    }
    Ok(out)
}

#[cfg(test)]
mod node_tests;
#[cfg(test)]
mod scheduling_tests;
#[cfg(test)]
mod web_login_tests;
