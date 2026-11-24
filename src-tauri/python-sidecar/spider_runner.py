# HGhub Python 边车：爬虫执行器。
#
# 支持两种蜘蛛写法：
# 1. 类式：定义 class Spider（优先继承 base.spider.Spider 的子类，
#    其次名叫 Spider 的类，即使不继承基类）；
# 2. 函数式：模块级定义 homeContent / categoryContent / detailContent /
#    searchContent / playerContent 等函数。
#
# 加载时把宿主的 base 包与 Spider / getProxyUrl 注入模块 globals；
# 类检测只认类自身 __dict__ 里实现的方法，不把基类的空方法误判为有效。
import re
import sys
import threading
import types
from urllib.parse import quote, urlparse

from base.spider import Spider as BaseSpider

# Crypto / Cryptodome 兼容层：pycryptodome 安装为 Crypto 命名空间，
# 部分蜘蛛写 `from Cryptodome.Hash import MD5`，这里建个别名指过去。
# 子模块（如 Crypto.Hash.MD5）是按需加载的，dir() 里看不到，
# 所以垫片用模块级 __getattr__（PEP 562）按需转发，并把最常用的
# 子模块预先注册进 sys.modules，让 `import Cryptodome.Hash.MD5` 也能走通。
try:
    import importlib

    import Crypto as _RealCrypto

    _CRYPTODOME_CHILDREN = {
        "Hash": [
            "MD5", "SHA1", "SHA224", "SHA256", "SHA384", "SHA512",
            "SHA3_224", "SHA3_256", "SHA3_384", "SHA3_512",
            "BLAKE2b", "BLAKE2s", "HMAC", "CMAC", "SHAKE128", "SHAKE256",
            "Poly1305",
        ],
        "Cipher": [
            "AES", "DES", "DES3", "ARC2", "ARC4", "Blowfish", "CAST",
            "ChaCha20", "ChaCha20_Poly1305", "Salsa20",
        ],
        "Util": ["Padding", "Counter", "strxor", "number"],
        "PublicKey": ["RSA", "DSA", "ECC", "ElGamal"],
        "Signature": ["pkcs1_15", "pss", "DSS"],
        "Protocol": ["KDF"],
        "Random": [],
    }

    def _make_cryptodome_shim(name, real_name):
        shim = types.ModuleType(name)
        shim.__package__ = "Cryptodome"
        shim.__path__ = []

        def __getattr__(attr, _real=real_name):
            real = importlib.import_module(_real)
            try:
                return getattr(real, attr)
            except AttributeError:
                pass
            try:
                return importlib.import_module(f"{_real}.{attr}")
            except ImportError:
                pass
            raise AttributeError(f"module {name!r} has no attribute {attr!r}")

        shim.__getattr__ = __getattr__
        return shim

    if "Cryptodome" not in sys.modules:
        cryptodome_package = types.ModuleType("Cryptodome")
        cryptodome_package.__path__ = getattr(_RealCrypto, "__path__", [])
        cryptodome_package.__package__ = "Cryptodome"
        sys.modules["Cryptodome"] = cryptodome_package

        for submodule in ["Cipher", "Hash", "Protocol", "PublicKey", "Signature", "Util", "IO", "Random"]:
            real_name = f"Crypto.{submodule}"
            try:
                importlib.import_module(real_name)
            except ImportError:
                continue
            shim_module = _make_cryptodome_shim(f"Cryptodome.{submodule}", real_name)
            sys.modules[f"Cryptodome.{submodule}"] = shim_module
            for child in _CRYPTODOME_CHILDREN.get(submodule, []):
                try:
                    sys.modules[f"Cryptodome.{submodule}.{child}"] = importlib.import_module(
                        f"{real_name}.{child}"
                    )
                except ImportError:
                    pass

        print("Crypto compat layer enabled (Cryptodome -> Crypto)", flush=True)
    else:
        print("Crypto compat layer enabled", flush=True)
except ImportError:
    print("WARNING: pycryptodome not installed, crypto functions may not work", flush=True)


# pyquery 解析路径修正：pyquery.fromstring 在 parser=None 时先试
# etree.fromstring（XML 解析器），只在抛 XMLSyntaxError 时才回退到
# lxml.html；且 lxml.html 默认的 html_parser 没开 huge_tree。大文档
# 在不同平台会解析出残缺 DOM。这里对默认的字符串输入强制走
# lxml.html + recover + huge_tree，远程蜘蛛的 pq(...) 调用透明生效。
try:
    import pyquery.pyquery as _pq_mod
    from lxml import html as _lhtml

    if not getattr(_pq_mod, "_hg_huge_tree_patched", False):
        _orig_pq_fromstring = _pq_mod.fromstring

        def _hg_pq_fromstring(context, parser=None, custom_parser=None):
            # 只干预默认的字符串解析；显式指定 parser 或 custom_parser
            # 的保持原行为，避免破坏特定解析需求。
            if (
                custom_parser is None
                and parser is None
                and isinstance(context, (str, bytes))
            ):
                # str->bytes + encoding='utf-8'：lxml.html.fromstring
                # 对 str 与 bytes 走不同内部路径，str 路径对大文档仍会截断。
                data = context.encode("utf-8") if isinstance(context, str) else context
                _parser = _lhtml.HTMLParser(recover=True, huge_tree=True, encoding="utf-8")
                try:
                    result = _lhtml.fromstring(data, parser=_parser)
                except Exception as _e:
                    print(f"[hg-patch] fromstring exc: {_e}", flush=True)
                    return _orig_pq_fromstring(context, parser, custom_parser)
                if isinstance(result, list):
                    return result
                if result is not None:
                    return [result]
                return []
            return _orig_pq_fromstring(context, parser, custom_parser)

        _pq_mod.fromstring = _hg_pq_fromstring
        _pq_mod._hg_huge_tree_patched = True
        print("pyquery.fromstring patched (lxml.html + recover + huge_tree)", flush=True)
except Exception as _e:
    print(f"WARNING: patch pyquery.fromstring failed: {_e}", flush=True)


class SpiderRunner:
    # 无界媒体 Range 的默认上限：localProxy 没给结束位置时，只读前 512 KiB，
    # 避免第三方代理无界读取整个媒体文件。
    _FALLBACK_OPEN_RANGE_SIZE = 512 * 1024

    # 宿主探测用的蜘蛛方法名表（与 Rust 侧 SPIDER_METHODS 对齐）。
    _SPIDER_METHODS = [
        "init",
        "homeContent",
        "homeVideoContent",
        "categoryContent",
        "detailContent",
        "searchContent",
        "playerContent",
        "localProxy",
        "liveContent",
        "action",
        "destroy",
        "isVideoFormat",
        "manualVideoCheck",
        "getName",
        "getDependence",
    ]

    @classmethod
    def _detected_methods(cls, spider_data):
        """执行期方法面：只认蜘蛛自身实现的方法，不认基类的空方法。"""
        if spider_data["type"] == "class":
            instance = spider_data["instance"]
            found = set()
            for klass in type(instance).__mro__:
                if klass is BaseSpider or klass is object:
                    continue
                for name in klass.__dict__:
                    if name in cls._SPIDER_METHODS and callable(
                        getattr(instance, name, None)
                    ):
                        found.add(name)
            return [m for m in cls._SPIDER_METHODS if m in found]
        module = spider_data["module"]
        return [m for m in cls._SPIDER_METHODS if callable(module.get(m))]

    def __init__(self, proxy_base_url="http://127.0.0.1:9978"):
        self._lock = threading.RLock()
        self._spiders = {}  # key -> spider instance or module globals
        self._proxy_base_url = str(proxy_base_url).rstrip("/")
        self._active_key = None
        # key -> 宿主回环地址（取数桥 / 日志桥的目标），由 /load 传入。
        self._host_media_base = {}

    def set_proxy_base_url(self, base_url):
        with self._lock:
            self._proxy_base_url = str(base_url or "").rstrip("/") or self._proxy_base_url

    def _get_proxy_url(self, key):
        site_key = quote(str(key), safe="")
        return (
            f"{self._proxy_base_url}/proxy?do=py&siteType=3"
            f"&siteKey={site_key}"
        )

    def _bind_proxy_site_key(self, value, key):
        """为蜘蛛返回的本地 Python 代理地址补充明确的站点归属。"""
        if isinstance(value, list):
            return [self._bind_proxy_site_key(item, key) for item in value]
        if isinstance(value, tuple):
            return tuple(self._bind_proxy_site_key(item, key) for item in value)
        if not isinstance(value, str) or not value:
            return value

        try:
            parsed = urlparse(value)
            proxy_base = urlparse(self._proxy_base_url)
            if parsed.scheme not in ("http", "https"):
                return value
            if parsed.hostname not in ("127.0.0.1", "localhost", "::1"):
                return value
            if parsed.port != proxy_base.port:
                return value
            if parsed.path not in ("", "/", "/proxy") and not parsed.path.startswith("/proxy/"):
                return value
        except ValueError:
            return value

        query = parsed.query
        if not re.search(r"(^|&)do=py(&|$)", query, re.IGNORECASE):
            return value
        if re.search(r"(^|&)siteKey=", query, re.IGNORECASE):
            return value

        site_key = quote(str(key), safe="")
        query = f"{query}&siteKey={site_key}" if query else f"siteKey={site_key}"
        return parsed._replace(query=query).geturl()

    def resolve_proxy_key(self, key=None, params=None):
        """解析代理归属：显式 siteKey 优先，其次当前活动蜘蛛，单蜘蛛时直接取。"""
        with self._lock:
            spiders = dict(self._spiders)
            active_key = self._active_key
        if key and key in spiders:
            return key
        if active_key in spiders:
            return active_key
        if len(spiders) == 1:
            return next(iter(spiders))
        return None

    def load(self, key: str, code: str, media_base: str | None = None) -> dict:
        """动态加载 Python 脚本。

        Returns:
            dict: {"success": bool, "error": str|None}
        """
        try:
            import os

            # 创建 base 模块结构，支持 from base.spider import Spider。
            # 基类在项目中命名为 Spider，但部分 TVBox 脚本以别名 BaseSpider
            # 导入（from base.spider import BaseSpider），两个名字都要暴露。
            base_package = types.ModuleType("base")
            base_package.__path__ = []  # 标记为包
            base_package.__package__ = "base"

            base_spider_module = types.ModuleType("base.spider")
            base_spider_module.Spider = BaseSpider
            base_spider_module.BaseSpider = BaseSpider
            base_spider_module.__package__ = "base"

            # 注册到 sys.modules（每次 load 都重建，保证拿到当前这一份）。
            sys.modules["base"] = base_package
            sys.modules["base.spider"] = base_spider_module

            # 创建模块命名空间。
            module_globals = {
                "__name__": f"spider_{key}",
                "__file__": f"<spider_{key}>",
                "Spider": BaseSpider,  # 注入 Spider 基类（支持直接 class Spider(Spider)）
                # 兼容函数式爬虫直接调用 getProxyUrl()。
                "getProxyUrl": lambda: self._get_proxy_url(key),
            }

            # 执行脚本代码。
            exec(code, module_globals)

            # 检测脚本格式。
            spider_instance = None

            # 优先查找 BaseSpider 子类，再兼容未继承基类的 TVBox class Spider。
            # 很多第三方脚本会直接声明 class Spider，而不是导入项目基类。
            script_classes = [
                (name, obj)
                for name, obj in module_globals.items()
                if isinstance(obj, type)
                and obj is not BaseSpider
                and obj.__module__ == module_globals["__name__"]
            ]
            class_candidates = [
                (name, obj)
                for name, obj in script_classes
                if issubclass(obj, BaseSpider)
            ]
            class_candidates.extend(
                (name, obj)
                for name, obj in script_classes
                if name == "Spider" and not issubclass(obj, BaseSpider)
            )

            spider_methods = {
                "homeContent",
                "categoryContent",
                "detailContent",
                "searchContent",
                "playerContent",
                "action",
            }
            for name, obj in class_candidates:
                # 只检查类自身实现，避免把基类中的空方法误判为有效实现。
                if any(
                    method_name in obj.__dict__
                    and callable(obj.__dict__[method_name])
                    for method_name in spider_methods
                ):
                    spider_instance = obj()
                    print(f"Detected class spider: {name}", flush=True)
                    break

            # 存储爬虫实例或模块。
            if spider_instance:
                if hasattr(spider_instance, "_set_proxy_context"):
                    spider_instance._set_proxy_context(key, self._proxy_base_url)
                else:
                    # 未继承 BaseSpider 的 TVBox class Spider 也需要缓存命名空间。
                    spider_instance._proxy_site_key = str(key)
                    spider_instance._proxy_base_url = self._proxy_base_url
                    if not callable(getattr(spider_instance, "getProxyUrl", None)):
                        proxy_url = self._get_proxy_url(key)
                        spider_instance.getProxyUrl = types.MethodType(
                            lambda _instance: proxy_url,
                            spider_instance,
                        )
                if hasattr(spider_instance, "_set_host_context"):
                    spider_instance._set_host_context(key, media_base or "")
                for cache_method in (
                    "setCache",
                    "getCache",
                    "delCache",
                    "clearCache",
                ):
                    if not callable(getattr(spider_instance, cache_method, None)):
                        setattr(
                            spider_instance,
                            cache_method,
                            types.MethodType(
                                getattr(BaseSpider, cache_method),
                                spider_instance,
                            ),
                        )
                self._spiders[key] = {
                    "type": "class",
                    "instance": spider_instance,
                }
            else:
                # 函数式爬虫。
                self._spiders[key] = {
                    "type": "function",
                    "module": module_globals,
                }
                print("Detected function spider", flush=True)

            if media_base:
                with self._lock:
                    self._host_media_base[key] = str(media_base).rstrip("/")

            style = self._spiders[key]["type"]
            return {
                "success": True,
                "error": None,
                "methods": self._detected_methods(self._spiders[key]),
                "style": style,
            }
        except Exception as e:
            import traceback

            traceback.print_exc()
            return {"success": False, "error": str(e)}

    def call(self, key: str, method: str, args: list) -> dict:
        """调用爬虫方法。

        Args:
            key: 爬虫唯一标识
            method: 方法名（init/home/category/detail/play/search 等短名或全名）
            args: 参数列表

        Returns:
            dict: {"success": bool, "result": any, "error": str|None}
        """
        if key not in self._spiders:
            return {"success": False, "result": None, "error": f"Spider not loaded: {key}"}

        spider_data = self._spiders[key]
        spider_type = spider_data["type"]
        if method not in ("localProxy", "proxy"):
            self._active_key = key

        try:
            orig_args = list(args)
            # 参数适配只做一次（类式与函数式同一套签名规则），
            # 再给 localProxy 的媒体 Range 加上限。
            adapted = self._adapt_args(method, orig_args)
            bounded = self._bound_media_range_args(method, adapted)
            if spider_type == "class":
                # 类式爬虫。
                instance = spider_data["instance"]
                result = self._call_class_method(instance, method, bounded)
            else:
                # 函数式爬虫。
                result = self._call_function(spider_data["module"], method, bounded)

            # 方法不存在时分发层返回 {"error": ...}，这里转成调用失败，
            # 不能当成功结果返回给宿主。
            if isinstance(result, dict) and set(result.keys()) == {"error"}:
                return {"success": False, "result": None, "error": str(result["error"])}

            # 媒体代理的脚本级 500 只重试一次（上游偶发连接重置）。
            if self._should_retry_local_proxy(method, bounded, result):
                print(f"Retrying media proxy once: key={key}", flush=True)
                if spider_type == "class":
                    result = self._call_class_method(
                        spider_data["instance"], method, bounded
                    )
                else:
                    result = self._call_function(spider_data["module"], method, bounded)

            if (
                method in ("play", "playerContent")
                and isinstance(result, dict)
                and "url" in result
            ):
                result = result.copy()
                result["url"] = self._bind_proxy_site_key(result["url"], key)

            # 确保返回值是 dict。
            if result is None:
                result = {}
            return {"success": True, "result": result, "error": None}

        except Exception as e:
            import traceback

            traceback.print_exc()
            return {"success": False, "result": None, "error": str(e)}

    @staticmethod
    def _should_retry_local_proxy(method, args, result):
        """只对媒体代理的脚本级 500 做一次重试。"""
        if method not in ("localProxy", "proxy") or not isinstance(result, (list, tuple)):
            return False
        if len(result) < 2 or result[0] != 500:
            return False
        params = args[0] if args and isinstance(args[0], dict) else {}
        return params.get("type") in ("media", "single")

    @classmethod
    def _bound_media_range_args(cls, method, args):
        """没有流式缓存信息时，避免第三方代理无界读取整个媒体文件。"""
        if method not in ("localProxy", "proxy") or not args:
            return args
        params = args[0] if isinstance(args[0], dict) else None
        if not params or params.get("type") not in ("media", "single"):
            return args

        raw_range = params.get("range") or params.get("Range")
        value = str(raw_range or "").strip()
        match = re.fullmatch(r"bytes=(\d+)-", value, re.IGNORECASE)
        if value and not match:
            return args
        start = int(match.group(1)) if match else 0
        bounded = f"bytes={start}-{start + cls._FALLBACK_OPEN_RANGE_SIZE - 1}"
        bounded_params = dict(params)
        bounded_params["range"] = bounded
        bounded_params.pop("Range", None)
        print(f"Bounded open media range: {bounded}", flush=True)
        return [bounded_params, *args[1:]]

    def _call_class_method(self, instance, method: str, args: list):
        """调用类式爬虫方法。"""
        # 方法名映射：TVBox 短名 -> 类方法名。
        method_map = {
            "init": "init",
            "home": "homeContent",
            "homeContent": "homeContent",
            "homeVideo": "homeVideoContent",
            "homeVideoContent": "homeVideoContent",
            "category": "categoryContent",
            "categoryContent": "categoryContent",
            "detail": "detailContent",
            "detailContent": "detailContent",
            "search": "searchContent",
            "searchContent": "searchContent",
            "play": "playerContent",
            "playerContent": "playerContent",
            "action": "action",
        }

        actual_method = method_map.get(method, method)

        if not hasattr(instance, actual_method):
            return {"error": f"Method not found: {actual_method}"}

        func = getattr(instance, actual_method)

        # 参数已在 call() 里适配过一次，这里直接调用。
        return func(*args)

    def _call_function(self, module, method: str, args: list):
        """调用函数式爬虫方法。"""
        func = module.get(method)

        if not func or not callable(func):
            return {"error": f"Function not found: {method}"}

        return func(*args)

    def _adapt_args(self, method: str, args: list) -> list:
        """参数适配（类式与函数式同一套规则；宿主只传宿主侧参数）。

        类式/函数式爬虫方法签名：
        - init(extend="")
        - homeContent(filter)
        - homeVideoContent()
        - categoryContent(tid, pg, filter, extend)
        - detailContent(ids)
        - searchContent(key, quick, pg="1")
        - playerContent(flag, id, vipFlags)
        """
        if method == "init":
            # init(ext) -> init(extend)。
            if args:
                import json

                ext = args[0]
                if ext is None:
                    return [""]
                elif isinstance(ext, (dict, list)):
                    # 将 dict/list 转为 JSON 字符串。
                    return [json.dumps(ext, ensure_ascii=False)]
                else:
                    return [str(ext)]
            return [""]

        elif method in ["home", "homeContent"]:
            # home() -> homeContent(filter)。
            # TVBox/FongMi 原生传 1，脚本惯用 `if filter:` 决定是否返回 filters；
            # 空字典是 falsy，会让筛选条件静默丢失。
            return [1]  # filter 参数

        elif method == "homeVideo":
            # homeVideo()
            return []

        elif method in ["category", "categoryContent"]:
            # category(tid, pg, ext) -> categoryContent(tid, pg, filter, extend)。
            # TVBox 约定 pg 为字符串，很多脚本会调用 pg.isdigit()，
            # 若传入 int 会抛 'int' object has no attribute 'isdigit'。
            tid = args[0] if len(args) > 0 else ""
            pg = str(args[1]) if len(args) > 1 else "1"
            ext = args[2] if len(args) > 2 else {}
            return [tid, pg, {}, ext]  # filter 为空字典

        elif method in ["detail", "detailContent"]:
            # detail(id) -> detailContent([id])
            if args:
                return [[args[0]]]  # 转为列表
            return [[]]

        elif method in ["search", "searchContent"]:
            # search(wd, pg) -> searchContent(key, quick, pg)
            key = args[0] if len(args) > 0 else ""
            pg = str(args[1]) if len(args) > 1 else "1"
            return [key, True, pg]  # quick 默认 True

        elif method in ["play", "playerContent"]:
            # play(flag, id) -> playerContent(flag, id, vipFlags)
            flag = args[0] if len(args) > 0 else ""
            id = args[1] if len(args) > 1 else ""
            return [flag, id, []]  # vipFlags 为空列表

        return args

    def unload(self, key: str) -> dict:
        """卸载爬虫脚本。"""
        if key in self._spiders:
            # 调用 destroy 方法（如果存在）。
            spider_data = self._spiders[key]
            if spider_data["type"] == "class":
                instance = spider_data["instance"]
                if hasattr(instance, "destroy"):
                    try:
                        instance.destroy()
                    except Exception:
                        pass
            del self._spiders[key]
            with self._lock:
                self._host_media_base.pop(key, None)
            if self._active_key == key:
                self._active_key = None
        return {"success": True}

    def list_loaded(self) -> list:
        """列出已加载的爬虫。"""
        return list(self._spiders.keys())
