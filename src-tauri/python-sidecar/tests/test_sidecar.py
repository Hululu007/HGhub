# HGhub Python 边车端到端测试（本机 CPython 3.12 实测，非 iOS）。
#
# 用法：python3 tests/test_sidecar.py
# 起真实的 main.py（测试端口），再用 requests 调 /load /call /proxy，
# 并用一个假 Rust 回环桥（/pyfetch + /pylog）验证宿主取数桥与日志桥。
# v1-only 模式：HG_PY_TEST_V1_ONLY=1 python3 tests/test_sidecar.py
# ——边车子进程跑在模拟的 iOS v1 环境里（vendored 纯 Python 包 +
# lxml/pyquery/Crypto 被拦截），验证缺包时的行为。
import json
import os
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import requests

HERE = os.path.dirname(os.path.abspath(__file__))
SIDECAR_DIR = os.path.dirname(HERE)
DEMO = os.path.abspath(os.path.join(HERE, "..", "..", "..", "examples", "demo-spider.py"))
VENV_PY = os.path.abspath(
    os.path.join(HERE, "..", "..", "..", "..", "tmp", "pysidecar-test", "venv", "bin", "python")
)

API_PORT = 55101
PROXY_PORT = 9998
API = f"http://127.0.0.1:{API_PORT}"
FAKE_BRIDGE_PORT = 55102
FAKE_BRIDGE = f"http://127.0.0.1:{FAKE_BRIDGE_PORT}"

# v1-only 模式：HG_PY_TEST_V1_ONLY=1 时，边车子进程跑在模拟的 iOS v1
# 环境里——PYTHONPATH 只给 vendored site-packages（纯 Python），并用
# tests/v1only/sitecustomize.py 拦截 lxml/pyquery/Crypto 的 import。
# 该模式下二进制包相关的检查改为“缺包时报明错”。
V1_ONLY = os.environ.get("HG_PY_TEST_V1_ONLY") == "1"
V1ONLY_DIR = os.path.join(HERE, "v1only")
VENDORED_SITE_PACKAGES = os.path.join(SIDECAR_DIR, "site-packages")

FAILURES = []


def check(name, cond, detail=""):
    print(("PASS " if cond else "FAIL ") + name + (f" — {detail}" if detail and not cond else ""))
    if not cond:
        FAILURES.append(name)


# ---------- 假 Rust 回环桥：/pyfetch + /pylog ----------
class FakeBridgeHandler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) if length else b""
        try:
            payload = json.loads(raw or b"{}")
        except Exception:
            payload = {}
        if self.path == "/pyfetch":
            import base64

            body = base64.b64encode(
                json.dumps({"via": "host-bridge", "url": payload.get("url")}).encode("utf-8")
            ).decode("ascii")
            out = {
                "status": 200,
                "headers": {"Content-Type": "application/json"},
                "bodyBase64": body,
                "url": payload.get("url"),
            }
            self._send(200, out)
        elif self.path == "/pylog":
            self._send(200, {"ok": True})
        else:
            self._send(404, {"error": "not found"})

    def _send(self, status, obj):
        data = json.dumps(obj).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


def start_fake_bridge():
    server = ThreadingHTTPServer(("127.0.0.1", FAKE_BRIDGE_PORT), FakeBridgeHandler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


# ---------- 假上游：供 requests 直连兜底测试 ----------
class FakeUpstreamHandler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_GET(self):
        out = json.dumps({"via": "direct", "path": self.path}).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(out)))
        self.end_headers()
        self.wfile.write(out)


def start_fake_upstream():
    server = ThreadingHTTPServer(("127.0.0.1", 0), FakeUpstreamHandler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


def wait_health(timeout=20):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            r = requests.get(API + "/health", timeout=2)
            if r.status_code == 200:
                return r.json()
        except Exception:
            pass
        time.sleep(0.3)
    raise RuntimeError("sidecar did not become healthy")


def call(key, method, args):
    r = requests.post(API + "/call", json={"key": key, "method": method, "args": args}, timeout=15)
    return r.status_code, r.json()


def main():
    start_fake_bridge()
    upstream = start_fake_upstream()
    upstream_base = f"http://127.0.0.1:{upstream.server_address[1]}"

    env = dict(os.environ)
    env["HG_PY_API_PORT"] = str(API_PORT)
    env["HG_PY_PROXY_PORT"] = str(PROXY_PORT)
    if V1_ONLY:
        # 拦截器 + vendored 纯 Python 包走在最前面，venv 里装的
        # lxml/pyquery/Crypto 对子进程不可见。
        env["PYTHONPATH"] = os.pathsep.join(
            [V1ONLY_DIR, VENDORED_SITE_PACKAGES, env.get("PYTHONPATH", "")]
        )
    proc = subprocess.Popen(
        [VENV_PY, "main.py"],
        cwd=SIDECAR_DIR,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    try:
        health = wait_health()
        check("health ok", health.get("status") == "ok")
        check(
            "health proxyBase",
            health.get("proxyBase") == f"http://127.0.0.1:{PROXY_PORT}",
            str(health),
        )

        with open(DEMO, "r", encoding="utf-8") as f:
            demo_code = f.read()
        r = requests.post(
            API + "/load",
            json={"key": "demo", "code": demo_code, "mediaBase": FAKE_BRIDGE},
            timeout=15,
        )
        check("load demo", r.status_code == 200 and r.json().get("success") is True, r.text[:300])
        check(
            "load demo methods",
            r.status_code == 200
            and "homeContent" in r.json().get("methods", [])
            and "detailContent" in r.json().get("methods", [])
            and "playerContent" in r.json().get("methods", [])
            and r.json().get("style") == "class",
            r.text[:300],
        )

        status, home = call("demo", "homeContent", [])
        check("homeContent class", status == 200 and len(home.get("class", [])) == 3, str(home)[:200])
        check("homeContent list", status == 200 and len(home.get("list", [])) == 3)

        status, hv = call("demo", "homeVideoContent", [])
        check("homeVideoContent", status == 200 and len(hv.get("list", [])) == 3)

        status, cat = call("demo", "categoryContent", ["movie", "1", {}])
        check("categoryContent p1", status == 200 and len(cat.get("list", [])) == 3 and cat.get("page") == 1)
        status, cat2 = call("demo", "categoryContent", ["movie", "2", {}])
        check("categoryContent p2 empty", status == 200 and cat2.get("list") == [])

        status, det = call("demo", "detailContent", ["demo-bbb"])
        check("detailContent", status == 200 and len(det.get("list", [])) == 1)
        vod = det["list"][0] if status == 200 and det.get("list") else {}
        check("detail play url 2 eps", vod.get("vod_play_url", "").count("#") == 1, vod.get("vod_play_url", ""))

        status, sea = call("demo", "searchContent", ["Bunny", "1"])
        check("searchContent", status == 200 and len(sea.get("list", [])) == 1)

        mux = "https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8"
        status, play = call("demo", "playerContent", ["演示源", mux, []])
        check("playerContent", status == 200 and play.get("parse") == 0 and play.get("url") == mux, str(play)[:200])

        # 短名映射：home/category/detail/search/play。
        status, cat3 = call("demo", "category", ["movie", "1", {}])
        check("short name category", status == 200 and len(cat3.get("list", [])) == 3)
        status, play2 = call("demo", "play", ["演示源", mux])
        check("short name play", status == 200 and play2.get("url") == mux)

        # init 无参数默认值。
        status, init = call("demo", "init", [])
        check("init", status == 200)

        # 运行时缓存清理（v0.0.84）：clearCache 只清本蜘蛛命名空间，
        # 空缓存再清也成功。
        status, _ = call("demo", "setCache", ["ck1", "v1"])
        check("setCache", status == 200)
        status, got = call("demo", "getCache", ["ck1"])
        check("getCache hit", status == 200 and got == "v1", str(got)[:200])
        status, cleared = call("demo", "clearCache", [])
        check("clearCache", status == 200 and cleared == "succeed", str(cleared)[:200])
        status, got = call("demo", "getCache", ["ck1"])
        # 未命中时 getCache 返回 None，runner 按“返回值是 dict”归一成 {}。
        check("getCache miss after clearCache", status == 200 and got == {}, str(got)[:200])
        status, cleared = call("demo", "clearCache", [])
        check("clearCache empty ok", status == 200 and cleared == "succeed", str(cleared)[:200])

        # 未知方法 / 未加载的 key 报明错。
        status, err = call("demo", "noSuchMethod", [])
        check("unknown method errors", status == 500 and "error" in err, str(err)[:200])
        status, err = call("ghost", "homeContent", [])
        check("unknown key errors", status == 500 and "Spider not loaded" in str(err.get("error")), str(err)[:200])

        # localProxy：cover 出图，未知 act 回 404。
        r = requests.get(API + f"/proxy?do=py&siteKey=demo&act=cover", timeout=10)
        check("proxy cover 200 png", r.status_code == 200 and r.headers.get("Content-Type", "").startswith("image/png") and r.content[:8] == b"\x89PNG\r\n\x1a\n", f"{r.status_code} {r.headers.get('Content-Type')}")
        r = requests.get(API + "/proxy?do=py&siteKey=demo&act=nope", timeout=10)
        check("proxy unknown act 404", r.status_code == 404, str(r.status_code))
        # 单蜘蛛时不带 siteKey 也能解析归属。
        r = requests.get(API + "/proxy?do=py&act=cover", timeout=10)
        check("proxy no siteKey single spider", r.status_code == 200, str(r.status_code))
        # 根路径 ?do=py 同义。
        r = requests.get(API + "/?do=py&siteKey=demo&act=cover", timeout=10)
        check("root do=py proxy", r.status_code == 200, str(r.status_code))

        # getProxyUrl 绑定：蜘蛛返回的地址带上自己的 siteKey。
        echo_code = '''
try:
    from base.spider import Spider as BaseSpider
except ImportError:
    class BaseSpider: pass
import json
class Spider(BaseSpider):
    def getName(self):
        return "echo"
    def homeContent(self, filter):
        return {"proxy": self.getProxyUrl(), "class": [], "list": []}
    def localProxy(self, param):
        return [200, "application/json", json.dumps(param, ensure_ascii=False), {}]
'''
        r = requests.post(API + "/load", json={"key": "echo", "code": echo_code, "mediaBase": FAKE_BRIDGE}, timeout=15)
        check("load echo spider", r.status_code == 200 and r.json().get("success") is True, r.text[:200])
        status, got = call("echo", "homeContent", [])
        check(
            "getProxyUrl siteKey",
            status == 200 and got.get("proxy", "").startswith(f"http://127.0.0.1:{PROXY_PORT}/proxy?do=py&siteType=3&siteKey=echo"),
            str(got)[:200],
        )
        # Base64 里的 + 经查询串还原（Flask 把 + 解成空格，路由再换回 +）。
        r = requests.get(API + "/proxy?do=py&siteKey=echo&data=QUJD+REVG", timeout=10)
        check(
            "proxy + restore",
            r.status_code == 200 and json.loads(r.text).get("data") == "QUJD+REVG",
            r.text[:200],
        )
        # play 返回的代理地址自动补 siteKey。
        play_proxy_code = '''
try:
    from base.spider import Spider as BaseSpider
except ImportError:
    class BaseSpider: pass
class Spider(BaseSpider):
    def homeContent(self, filter):
        return {"class": [], "list": []}
    def detailContent(self, ids):
        return {"list": [{"vod_id": "x", "vod_name": "x", "vod_play_from": "s", "vod_play_url": "e1$http://127.0.0.1:%d/proxy?do=py&type=media"}]}
    def playerContent(self, flag, id, vipFlags):
        return {"parse": 0, "url": id, "header": {}}
''' % PROXY_PORT
        r = requests.post(API + "/load", json={"key": "pp", "code": play_proxy_code, "mediaBase": FAKE_BRIDGE}, timeout=15)
        check("load play-proxy spider", r.status_code == 200 and r.json().get("success") is True, r.text[:200])
        status, got = call("pp", "playerContent", ["s", f"http://127.0.0.1:{PROXY_PORT}/proxy?do=py&type=media", []])
        check(
            "play url gets siteKey",
            status == 200 and "siteKey=pp" in got.get("url", ""),
            str(got)[:300],
        )

        # 宿主取数桥：self.fetch 优先走 /pyfetch，返回桥的标记。
        fetch_code = '''
try:
    from base.spider import Spider as BaseSpider
except ImportError:
    class BaseSpider: pass
class Spider(BaseSpider):
    def homeContent(self, filter):
        rsp = self.fetch("https://example.invalid/api")
        return {"class": [], "list": [], "via": rsp.json().get("via"), "status": rsp.status_code}
    def detailContent(self, ids):
        return {"list": []}
    def playerContent(self, flag, id, vipFlags):
        return {"parse": 0, "url": id, "header": {}}
'''
        r = requests.post(API + "/load", json={"key": "fb", "code": fetch_code, "mediaBase": FAKE_BRIDGE}, timeout=15)
        check("load fetch-bridge spider", r.status_code == 200 and r.json().get("success") is True, r.text[:200])
        status, got = call("fb", "homeContent", [])
        check("fetch via host bridge", status == 200 and got.get("via") == "host-bridge", str(got)[:300])

        # 无 mediaBase 时 self.fetch 退回 requests 直连。
        direct_code = fetch_code.replace("https://example.invalid/api", upstream_base + "/hello")
        r = requests.post(API + "/load", json={"key": "fd", "code": direct_code}, timeout=15)
        check("load direct-fetch spider", r.status_code == 200 and r.json().get("success") is True, r.text[:200])
        status, got = call("fd", "homeContent", [])
        check("fetch direct fallback", status == 200 and got.get("via") == "direct", str(got)[:300])

        # Cryptodome 别名 + pyquery 大文档补丁。
        compat_code = '''
try:
    from base.spider import Spider as BaseSpider
except ImportError:
    class BaseSpider: pass
from Cryptodome.Hash import MD5
from pyquery import PyQuery as pq
class Spider(BaseSpider):
    def homeContent(self, filter):
        h = MD5.new(b"abc").hexdigest()
        doc = pq("<html><body>" + "<p>x</p>" * 5000 + "</body></html>")
        return {"class": [], "list": [], "md5": h, "p": len(doc("p"))}
    def detailContent(self, ids):
        return {"list": []}
    def playerContent(self, flag, id, vipFlags):
        return {"parse": 0, "url": id, "header": {}}
'''
        r = requests.post(API + "/load", json={"key": "cc", "code": compat_code, "mediaBase": FAKE_BRIDGE}, timeout=15)
        if V1_ONLY:
            # v1 包里没有 pycryptodome/pyquery：蜘蛛顶层 import 失败，
            # 加载即报明错，而不是在调用时才炸。
            check("v1: compat spider load fails clearly", r.status_code == 500 and r.json().get("success") is False, r.text[:300])
        else:
            check("load compat spider", r.status_code == 200 and r.json().get("success") is True, r.text[:300])
            status, got = call("cc", "homeContent", [])
            check(
                "cryptodome + pyquery",
                status == 200 and got.get("md5") == "900150983cd24fb0d6963f7d28e17f72" and got.get("p") == 5000,
                str(got)[:300],
            )

        # 函数式蜘蛛。
        func_code = '''
def homeContent(filter):
    return {"class": [{"type_id": "a", "type_name": "A"}], "list": []}
def detailContent(ids):
    return {"list": []}
def playerContent(flag, id, vipFlags):
    return {"parse": 0, "url": id, "header": {}}
'''
        r = requests.post(API + "/load", json={"key": "fn", "code": func_code, "mediaBase": FAKE_BRIDGE}, timeout=15)
        check("load function spider", r.status_code == 200 and r.json().get("success") is True, r.text[:200])
        status, got = call("fn", "homeContent", [])
        check("function spider call", status == 200 and len(got.get("class", [])) == 1, str(got)[:200])
        status, err = call("fn", "searchContent", ["x", True, "1"])
        check("function spider missing method", status == 500, str(err)[:200])

        # 语法错误的蜘蛛加载失败，报明错。
        r = requests.post(API + "/load", json={"key": "bad", "code": "def broken(:\n", "mediaBase": FAKE_BRIDGE}, timeout=15)
        check("syntax error load fails", r.status_code == 500 and r.json().get("success") is False, r.text[:200])

        # /list 与 /unload。
        r = requests.get(API + "/list", timeout=10)
        loaded = r.json().get("spiders", [])
        check("list spiders", "demo" in loaded and "echo" in loaded, str(loaded))
        r = requests.post(API + "/unload", json={"key": "fn"}, timeout=10)
        check("unload", r.status_code == 200 and r.json().get("success") is True, r.text[:200])
        status, err = call("fn", "homeContent", [])
        check("unloaded spider gone", status == 500, str(err)[:200])

        # 日志桥：self.log 走 /pylog（假桥返回 200 即不抛错）。
        log_code = '''
try:
    from base.spider import Spider as BaseSpider
except ImportError:
    class BaseSpider: pass
class Spider(BaseSpider):
    def homeContent(self, filter):
        self.log("hello from spider")
        self.log({"k": 1})
        return {"class": [], "list": []}
    def detailContent(self, ids):
        return {"list": []}
    def playerContent(self, flag, id, vipFlags):
        return {"parse": 0, "url": id, "header": {}}
'''
        r = requests.post(API + "/load", json={"key": "lg", "code": log_code, "mediaBase": FAKE_BRIDGE}, timeout=15)
        check("load log spider", r.status_code == 200 and r.json().get("success") is True, r.text[:200])
        status, got = call("lg", "homeContent", [])
        check("log via bridge", status == 200, str(got)[:200])

        if V1_ONLY:
            # v1 探测蜘蛛：纯 Python 包可用（且确实从 vendored 目录加载），
            # 二进制包缺失报 ImportError，base.html() 报明错，page() 正常。
            probe_code = '''
try:
    from base.spider import Spider as BaseSpider
except ImportError:
    class BaseSpider: pass
import flask
import requests
import bs4
def _try_import(name):
    try:
        __import__(name)
        return "ok"
    except ImportError:
        return "missing"
class Spider(BaseSpider):
    def action(self, action):
        try:
            self.html("<p>hi</p>")
            html_result = "unexpected-ok"
        except ImportError as e:
            html_result = str(e)
        return {
            "flask_file": flask.__file__,
            "requests_file": requests.__file__,
            "bs4_ok": True,
            "lxml": _try_import("lxml"),
            "pyquery": _try_import("pyquery"),
            "crypto": _try_import("Crypto"),
            "html_error": html_result,
            "page": self.page(list(range(25)), 2, 10),
        }
    def detailContent(self, ids):
        return {"list": []}
    def playerContent(self, flag, id, vipFlags):
        return {"parse": 0, "url": id, "header": {}}
'''
            r = requests.post(API + "/load", json={"key": "v1p", "code": probe_code, "mediaBase": FAKE_BRIDGE}, timeout=15)
            check("v1: load probe spider", r.status_code == 200 and r.json().get("success") is True, r.text[:200])
            status, got = call("v1p", "action", ["probe"])
            check("v1: flask from vendored dir", status == 200 and got.get("flask_file", "").startswith(VENDORED_SITE_PACKAGES), str(got)[:200])
            check("v1: requests from vendored dir", status == 200 and got.get("requests_file", "").startswith(VENDORED_SITE_PACKAGES), str(got)[:200])
            check("v1: bs4 importable", status == 200 and got.get("bs4_ok") is True, str(got)[:200])
            check("v1: lxml missing", status == 200 and got.get("lxml") == "missing", str(got)[:200])
            check("v1: pyquery missing", status == 200 and got.get("pyquery") == "missing", str(got)[:200])
            check("v1: Crypto missing", status == 200 and got.get("crypto") == "missing", str(got)[:200])
            check("v1: html() clear error", status == 200 and "lxml" in str(got.get("html_error", "")), str(got)[:200])
            page = got.get("page", {}) if status == 200 else {}
            check("v1: page() slices", page.get("list") == list(range(10, 20)) and page.get("pagecount") == 3, str(page)[:200])
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
        upstream.shutdown()

    print()
    if FAILURES:
        print(f"{len(FAILURES)} FAILURES: {FAILURES}")
        sys.exit(1)
    print("ALL SIDECAR TESTS PASSED")


if __name__ == "__main__":
    main()
