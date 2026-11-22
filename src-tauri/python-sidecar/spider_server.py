# HGhub Python 边车：Flask 服务入口。
#
# 宿主（Rust）经回环 HTTP 调边车：
#   GET  /health          健康检查，附带代理入口地址
#   POST /load            {key, code, mediaBase?} 加载蜘蛛
#   POST /call            {key, method, args} 调用蜘蛛方法，返回结果 JSON
#   GET/POST /proxy       ?do=py&siteKey=... 转 localProxy
#   POST /unload          {key} 卸载蜘蛛
#   GET  /list            已加载的蜘蛛
#   GET/POST /            ?do=py 时与 /proxy 同义（兼容写死该地址的蜘蛛）
#
# /call 成功时直接返回结果 JSON（不是 {success, result} 包裹——
# Rust 侧按 HTTP 状态区分成功失败）；失败时返回 {error} + 500。
import json
import os
import socket
import threading

from flask import Flask, Response, jsonify, request
from werkzeug.serving import make_server

from proxy_response import normalize_proxy_result
from spider_runner import SpiderRunner

app = Flask(__name__)
runner = SpiderRunner()

CURRENT_DIR = os.path.dirname(os.path.abspath(__file__))

# 代理入口端口：先占 9978（FongMi 兼容地址），占不到就换临时端口，
# 实际地址以 /health 返回的 proxyBase 为准。
PROXY_PORT = int(os.environ.get("HG_PY_PROXY_PORT", "9978"))
API_PORT = int(os.environ.get("HG_PY_API_PORT", "55001"))

_proxy_server = None
_proxy_port_actual = None
_proxy_lock = threading.Lock()
_api_port_actual = None


def bind_server(host, ports, app):
    """按顺序试绑端口，返回 werkzeug 服务。

    werkzeug 3.1 的 make_server 在端口被占时直接 sys.exit(1)，不抛异常，
    所以不能靠 try/except OSError 做端口回退；这里先用原生 socket 试绑，
    占不到就换下一个端口（0 表示临时端口），绑上后再把 fd 交给 werkzeug
    （fd 模式下它不再自己 bind，也就不会 exit）。
    """
    last_error = None
    for port in ports:
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        try:
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            sock.bind((host, port))
            actual = sock.getsockname()[1]
            server = make_server(host, actual, app, threaded=True, fd=sock.fileno())
            # fd 模式下 werkzeug 只接管 socket，不调 listen；这里补上，
            # 否则 serve_forever 收不到连接。
            server.server_activate()
            return server
        except OSError as e:
            last_error = e
        finally:
            try:
                sock.close()
            except Exception:
                pass
    raise OSError(f"cannot bind {host} on ports {list(ports)}: {last_error}")


def api_base_url():
    port = _api_port_actual if _api_port_actual else API_PORT
    return f"http://127.0.0.1:{port}"


def set_api_port_actual(port):
    """main.py 每次成功绑定 API 端口后调用（临时端口时与 API_PORT 不同）。"""
    global _api_port_actual
    _api_port_actual = int(port)


def proxy_base_url():
    with _proxy_lock:
        port = _proxy_port_actual if _proxy_port_actual else PROXY_PORT
    return f"http://127.0.0.1:{port}"


def start_proxy_server():
    """启动 FongMi 兼容代理入口；9978 被占时改用临时端口。"""
    global _proxy_server, _proxy_port_actual
    with _proxy_lock:
        if _proxy_server is not None:
            return _proxy_server
        server = bind_server("127.0.0.1", (PROXY_PORT, 0), app)
        _proxy_port_actual = server.server_address[1]
        runner.set_proxy_base_url(f"http://127.0.0.1:{_proxy_port_actual}")
        thread = threading.Thread(
            target=server.serve_forever,
            name="hg-python-proxy",
            daemon=True,
        )
        thread.start()
        _proxy_server = server
        return server


@app.route("/health", methods=["GET"])
def health():
    """健康检查。"""
    return jsonify(
        {
            "status": "ok",
            "proxyBase": proxy_base_url(),
            "apiBase": api_base_url(),
        }
    )


@app.route("/", methods=["GET", "POST", "PUT"])
def root():
    """兼容写死为 http://127.0.0.1:9978?do=py 的爬虫。"""
    if request.args.get("do") == "py":
        return proxy_spider()
    return jsonify({"status": "ok"})


@app.route("/load", methods=["POST"])
def load_spider():
    """加载爬虫脚本。

    Request JSON:
        {
            "key": "spider_key",
            "code": "python code string",
            "mediaBase": "http://127.0.0.1:<port>" (可选，宿主取数桥目标)
        }

    Response JSON:
        {"success": true, "error": null}
    """
    try:
        data = request.get_json()
        key = data.get("key")
        code = data.get("code")
        media_base = data.get("mediaBase")

        if not key or not code:
            return jsonify({"success": False, "error": "Missing key or code"}), 400

        result = runner.load(key, code, media_base=media_base)

        if result["success"]:
            return jsonify(result)
        else:
            return jsonify(result), 500
    except Exception as e:
        return jsonify({"success": False, "error": str(e)}), 500


@app.route("/call", methods=["POST"])
def call_spider():
    """调用爬虫方法。

    Request JSON:
        {"key": "spider_key", "method": "home", "args": []}

    成功时直接返回结果 JSON；失败时返回 {"error": ...} + 500。
    """
    try:
        data = request.get_json()
        key = data.get("key")
        method = data.get("method")
        args = data.get("args", [])

        if not key or not method:
            return jsonify({"success": False, "error": "Missing key or method"}), 400

        result = runner.call(key, method, args)

        if result["success"]:
            return jsonify(result["result"])
        else:
            return jsonify({"error": result["error"]}), 500
    except Exception as e:
        return jsonify({"error": str(e)}), 500


@app.route("/proxy", methods=["GET", "POST", "PUT"])
def proxy_spider():
    """将播放器/图片组件的本地请求转交给爬虫 localProxy。"""
    params = request.args.to_dict(flat=True)
    params.pop("siteKey", None)
    # 很多 TVBox 爬虫把标准 Base64 直接拼到查询串，其中的 +
    # 会被 Flask 当作空格解码，这里按宿主兼容行为还原。
    params = {
        name: value.replace(" ", "+") if isinstance(value, str) else value
        for name, value in params.items()
    }
    for name, value in request.headers.items():
        if value:
            params.setdefault(name.lower(), value)
    for name, value in request.form.items():
        params[name] = value
    site_key = runner.resolve_proxy_key(
        request.args.get("siteKey", ""),
        params,
    )
    if not site_key:
        return jsonify({"error": "No active Python spider for proxy request"}), 404
    result = runner.call(site_key, "localProxy", [params])
    if not result["success"]:
        status = 404 if result["error"].startswith("Spider not loaded:") else 502
        return jsonify({"error": result["error"]}), status

    try:
        status, content_type, body, headers = normalize_proxy_result(result["result"])
        headers.setdefault("Access-Control-Allow-Origin", "*")
        return Response(
            body,
            status=status,
            content_type=content_type,
            headers=headers,
        )
    except Exception as e:
        return jsonify({"error": str(e)}), 502


@app.route("/unload", methods=["POST"])
def unload_spider():
    """卸载爬虫脚本。

    Request JSON:
        {"key": "spider_key"}
    """
    try:
        data = request.get_json()
        key = data.get("key")

        if not key:
            return jsonify({"success": False, "error": "Missing key or code"}), 400

        result = runner.unload(key)
        return jsonify(result)
    except Exception as e:
        return jsonify({"success": False, "error": str(e)}), 500


@app.route("/list", methods=["GET"])
def list_spiders():
    """列出已加载的爬虫。"""
    return jsonify({"spiders": runner.list_loaded()})


if __name__ == "__main__":
    app.run(host="127.0.0.1", port=API_PORT)
