# HGhub Python 边车：TVBox 蜘蛛基类。
#
# 由宿主在解释器启动时注入 sys.modules（不随插件分发、不写进插件目录），
# 因此 `from base.spider import Spider` / `BaseSpider` 在任何蜘蛛里都命中
# 这一份实现，不会落到蜘蛛自带的 _StdBase 兜底上。
#
# 行为接口与 TVBox / FongMi 的 BaseSpider 惯例一致：单例、按站点隔离的
# 进程级缓存、getProxyUrl 返回宿主回环代理地址。fetch 优先走宿主网络桥
# （Rust 侧的 /pyfetch，经插件自己的 Network：Cookie 罐、目标校验、限速
# 都生效），桥不可用时才用 requests 直连兜底——宿主取数优先、urllib 兜底。
import base64
import json
import re
import time
import threading

import requests
from urllib.parse import quote

# lxml 是二进制包，iOS v1 打不进去（需交叉编译，见 requirements.txt）。
# html() 在没有 lxml 时报明错，不静默返回错的对象。
try:
    from lxml import etree as _etree
except ImportError:  # pragma: no cover - iOS v1 打包路径
    _etree = None

# 按站点隔离的进程级缓存。不同蜘蛛的数据不得互见，否则代理路由按
# getCache() 判断媒体归属时会串站。
_RUNTIME_CACHE = {}
_RUNTIME_CACHE_LOCK = threading.RLock()


def _cache_namespace(instance):
    return str(getattr(instance, "_proxy_site_key", "") or "")


class Spider:
    """TVBox Python 爬虫基类。"""

    _instance = None

    def __init__(self):
        self.extend = ""
        self._proxy_site_key = ""
        self._proxy_base_url = "http://127.0.0.1:9978"
        # 宿主桥上下文：由 SpiderRunner 在 load() 时绑定。
        self._host_media_base = ""
        self._host_plugin_key = ""

    def __new__(cls, *args, **kwargs):
        if cls._instance:
            return cls._instance
        cls._instance = super().__new__(cls)
        return cls._instance

    def init(self, extend=""):
        """初始化爬虫。"""
        pass

    def homeContent(self, filter):
        """获取首页分类和筛选。"""
        pass

    def homeVideoContent(self):
        """获取首页推荐视频。"""
        pass

    def categoryContent(self, tid, pg, filter, extend):
        """获取分类内容。"""
        pass

    def detailContent(self, ids):
        """获取详情。"""
        pass

    def searchContent(self, key, quick, pg="1"):
        """搜索。"""
        pass

    def playerContent(self, flag, id, vipFlags):
        """获取播放地址。"""
        pass

    def liveContent(self, url):
        """获取直播内容。"""
        pass

    def localProxy(self, param):
        """本地代理。"""
        pass

    def isVideoFormat(self, url):
        """检查视频格式。"""
        pass

    def manualVideoCheck(self):
        """手动检查。"""
        pass

    def action(self, action):
        """执行爬虫扩展动作。"""
        pass

    def destroy(self):
        """销毁。"""
        pass

    def getName(self):
        """获取名称。"""
        return self.__class__.__name__

    def getDependence(self):
        return []

    def _set_proxy_context(self, site_key, base_url):
        """由运行时绑定当前爬虫的本地代理上下文。"""
        self._proxy_site_key = str(site_key or "")
        self._proxy_base_url = str(base_url or "").rstrip("/")

    def _set_host_context(self, plugin_key, media_base):
        """绑定宿主桥上下文：取数桥与日志桥的目标地址。"""
        self._host_plugin_key = str(plugin_key or "")
        self._host_media_base = str(media_base or "").rstrip("/")

    def getProxyUrl(self, local=True):
        """返回 TVBox Python 爬虫兼容的本地代理地址。"""
        site_key = quote(self._proxy_site_key, safe="")
        return (
            f"{self._proxy_base_url}/proxy?do=py&siteType=3"
            f"&siteKey={site_key}"
        )

    def setCache(self, key, value):
        """保存供 playerContent / localProxy 共享的运行时数据。"""
        if value is None:
            return "failed"
        if isinstance(value, (int, float)):
            value = str(value)
        elif isinstance(value, (dict, list)):
            value = json.dumps(value, ensure_ascii=False)
        elif not isinstance(value, str):
            value = str(value)
        if not value:
            return "failed"
        with _RUNTIME_CACHE_LOCK:
            namespace = _cache_namespace(self)
            _RUNTIME_CACHE.setdefault(namespace, {})[str(key)] = value
        return "succeed"

    def getCache(self, key):
        """读取缓存，按 FongMi 约定处理 JSON 与 expiresAt。"""
        with _RUNTIME_CACHE_LOCK:
            namespace = _cache_namespace(self)
            value = _RUNTIME_CACHE.get(namespace, {}).get(str(key))
        if value is None or value == "":
            return None
        if isinstance(value, str) and (
            (value.startswith("{") and value.endswith("}"))
            or (value.startswith("[") and value.endswith("]"))
        ):
            try:
                value = json.loads(value)
            except json.JSONDecodeError:
                return value
        if isinstance(value, dict) and "expiresAt" in value:
            try:
                if float(value["expiresAt"]) < int(time.time()):
                    self.delCache(key)
                    return None
            except (TypeError, ValueError):
                pass
        return value

    def delCache(self, key):
        """删除缓存项。"""
        with _RUNTIME_CACHE_LOCK:
            namespace = _cache_namespace(self)
            cache = _RUNTIME_CACHE.get(namespace)
            if cache is not None:
                cache.pop(str(key), None)
                if not cache:
                    _RUNTIME_CACHE.pop(namespace, None)
        return "succeed"

    # ============ 工具方法 ============

    def regStr(self, reg, src, group=1):
        """正则提取。"""
        m = re.search(reg, src)
        if m:
            return m.group(group)
        return ""

    def removeHtmlTags(self, src):
        """移除 HTML 标签。"""
        clean = re.compile("<.*?>")
        return re.sub(clean, "", src)

    def cleanText(self, src):
        """清理文本。"""
        clean = re.sub(
            "[\U0001F600-\U0001F64F\U0001F300-\U0001F5FF\U0001F680-\U0001F6FF\U0001F1E0-\U0001F1FF]",
            "",
            src,
        )
        return clean

    def _host_fetch(self, url, method="GET", headers=None, body=None, timeout=15):
        """走宿主网络桥取数。返回桥的响应 dict，桥不可用时返回 None。

        桥是 Rust 侧回环服务上的 /pyfetch：用插件自己的 Network 取数，
        Cookie 罐、目标校验、限速与重试都与 JS 插件的 hg.http 一致。
        请求体与响应体都按 Base64 传，避免二进制在 JSON 里损坏。
        """
        base = self._host_media_base
        key = self._host_plugin_key
        if not base or not key:
            return None
        try:
            payload = {
                "pluginId": key,
                "url": url,
                "method": method,
                "headers": headers or {},
                "timeoutMs": int(timeout * 1000),
                "maxBytes": 8 * 1024 * 1024,
            }
            if body is not None:
                if isinstance(body, (bytes, bytearray)):
                    payload["bodyBase64"] = base64.b64encode(bytes(body)).decode("ascii")
                else:
                    payload["bodyBase64"] = base64.b64encode(
                        str(body).encode("utf-8")
                    ).decode("ascii")
            rsp = requests.post(base + "/pyfetch", json=payload, timeout=timeout + 5)
            if rsp.status_code != 200:
                return None
            data = rsp.json()
            if not isinstance(data, dict) or "status" not in data:
                return None
            return data
        except Exception:
            return None

    def fetch(self, url, params=None, cookies=None, headers=None, timeout=10, verify=True, stream=False,
              allow_redirects=True):
        """GET 请求：优先走宿主网络桥，桥不可用时 requests 直连兜底。"""
        bridged = self._host_fetch(url, "GET", headers, None, timeout)
        if bridged is not None:
            return _BridgedResponse(bridged)
        rsp = requests.get(url, params=params, cookies=cookies, headers=headers, timeout=timeout, verify=verify,
                           stream=stream, allow_redirects=allow_redirects)
        rsp.encoding = "utf-8"
        return rsp

    def post(self, url, params=None, data=None, json_data=None, cookies=None, headers=None, timeout=10, verify=True,
             stream=False, allow_redirects=True, **kwargs):
        """POST 请求：优先走宿主网络桥，桥不可用时 requests 直连兜底。"""
        payload = kwargs.get("json", json_data)
        body = None
        if payload is not None:
            body = json.dumps(payload, ensure_ascii=False)
            headers = dict(headers or {})
            headers.setdefault("Content-Type", "application/json")
        elif data is not None:
            body = data if isinstance(data, (bytes, bytearray)) else str(data)
        bridged = self._host_fetch(url, "POST", headers, body, timeout)
        if bridged is not None:
            return _BridgedResponse(bridged)
        rsp = requests.post(url, params=params, data=data, json=payload, cookies=cookies, headers=headers,
                            timeout=timeout, verify=verify, stream=stream, allow_redirects=allow_redirects)
        rsp.encoding = "utf-8"
        return rsp

    def html(self, content):
        """解析 HTML。

        有 lxml 时走 etree.HTML（xpath 可用）；iOS v1 没有 lxml 时报明错——
        不要用 beautifulsoup4 假装返回 lxml 元素，xpath 调用会在更晚的
        地方以难懂的 AttributeError 失败。需要解析 HTML 的蜘蛛在 v1 里
        请直接用 beautifulsoup4（已打进包）。
        """
        if _etree is None:
            raise ImportError(
                "lxml 未包含在 Python 运行时 v1 中（需 iOS 交叉编译）；"
                "解析 HTML 请用 beautifulsoup4"
            )
        return _etree.HTML(content)

    def page(self, items, pg, limit, total=None):
        """分页工具：给 total 时不切片（站点已按页取数），否则按页切片。

        pagecount = ceil(total / limit)。返回 TVBox 约定的分页 dict。
        """
        pg = int(pg or 1)
        limit = int(limit or 20)
        if total is None:
            total = len(items)
            items = items[(pg - 1) * limit: pg * limit]
        pagecount = (int(total) + limit - 1) // limit
        return {
            "list": items,
            "page": pg,
            "pagecount": pagecount,
            "limit": limit,
            "total": int(total),
        }

    def str2json(self, s):
        """字符串转 JSON。"""
        return json.loads(s)

    def json2str(self, obj):
        """JSON 转字符串。"""
        return json.dumps(obj, ensure_ascii=False)

    def log(self, msg):
        """日志输出：能走宿主日志桥就写进插件日志面板，否则打到标准输出。"""
        text = msg if isinstance(msg, str) else json.dumps(msg, ensure_ascii=False)
        base = self._host_media_base
        key = self._host_plugin_key
        if base and key:
            try:
                requests.post(
                    base + "/pylog",
                    json={"pluginId": key, "level": "info", "text": text},
                    timeout=5,
                )
                return
            except Exception:
                pass
        print(text, flush=True)


class _BridgedResponse:
    """宿主桥取数结果的 requests.Response 兼容外观。

    蜘蛛常写的 `.text` / `.content` / `.json()` / `.status_code` /
    `.headers` 在这里都有；桥不做流式读取，`stream` 语义不保留。
    """

    def __init__(self, data):
        self.status_code = int(data.get("status") or 0)
        self.headers = dict(data.get("headers") or {})
        raw = data.get("bodyBase64") or ""
        try:
            self.content = base64.b64decode(raw) if raw else b""
        except Exception:
            self.content = b""
        self.url = data.get("url") or ""
        self.encoding = "utf-8"
        self.ok = 200 <= self.status_code < 300

    @property
    def text(self):
        try:
            return self.content.decode(self.encoding or "utf-8", errors="replace")
        except Exception:
            return ""

    def json(self):
        return json.loads(self.text)
