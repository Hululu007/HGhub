# HGhub Python 边车入口——解释器启动点（iOS 上由宿主引导调用）。
#
# 内嵌的 Python 运行时没有系统 CA 根证书库，蜘蛛脚本里直接用
# urllib.request.urlopen 的 HTTPS 请求会报 CERTIFICATE_VERIFY_FAILED
# （requests 自带 certifi 不受影响）。这里全局关闭默认 HTTPS 上下文的
# 证书校验，让这类脚本能正常抓取。注意：这会让所有走默认上下文的
# Python 请求都不再校验证书。
import json
import os
import ssl
import sys
import time
import traceback

print(f"Python version: {sys.version}", flush=True)
print(f"Current directory: {os.getcwd()}", flush=True)


def _disable_ssl_verification():
    try:
        ssl._create_default_https_context = ssl._create_unverified_context
        print("SSL verification disabled (embedded runtime has no system CA roots)", flush=True)
    except AttributeError as e:
        print(f"WARNING: cannot disable SSL verification: {e}", flush=True)


_disable_ssl_verification()


def _write_info_file():
    """把边车实际地址写进宿主给的 info 文件（HG_PY_INFO_PATH）。

    宿主（Rust）引导时不知道临时端口，只能靠这个文件发现地址。
    未设置该环境变量时（本机测试）跳过。写临时文件再改名，避免
    宿主读到一半的内容。
    """
    path = os.environ.get("HG_PY_INFO_PATH")
    if not path:
        return
    try:
        from spider_server import api_base_url, proxy_base_url

        info = {"apiBase": api_base_url(), "proxyBase": proxy_base_url()}
        tmp = path + ".tmp"
        with open(tmp, "w", encoding="utf-8") as f:
            f.write(json.dumps(info))
        os.replace(tmp, path)
        print(f"Sidecar info written to {path}: {info}", flush=True)
    except Exception as e:
        print(f"WARNING: cannot write sidecar info file: {e}", flush=True)


def _serve_forever_loop(
    host="127.0.0.1",
    port=55001,
    max_bind_retries=30,
    bind_retry_delay=1.0,
    restart_delay=1.0,
):
    """自愈式 serving 循环。

    iOS 长时间挂起后 werkzeug 的 accept 循环可能抛异常使 serve_forever
    返回，那会让解释器线程退出并触发 Py_Finalize——CPython 不支持同进程
    二次初始化，宿主只能提示重启应用。这里保证本函数永不返回：
    serve_forever 出错就关掉旧 server、重建、继续监听，同时已加载的
    爬虫状态也得以保留。

    端口被占（例如别的 Python 壳也在跑）时先占请求端口，占不到改用
    临时端口；实际端口每次绑定成功后都写进 info 文件并更新 /health。
    """
    from spider_server import app, bind_server, set_api_port_actual

    bind_attempts = 0
    while True:
        server = None
        # 每轮先试请求端口，再试临时端口；两轮都失败才计一次重试。
        # 注意：直接调 make_server 试绑是行不通的——werkzeug 在端口被占时
        # 会 sys.exit(1) 而不是抛异常；bind_server 先用原生 socket 试绑，
        # 绑上后再把 fd 交给 werkzeug，失败时抛 OSError。
        try:
            server = bind_server(host, (port, 0), app)
        except OSError:
            bind_attempts += 1
            if bind_attempts >= max_bind_retries:
                # 初始绑定阶段彻底失败（端口被占/环境异常），交外层处理。
                raise OSError(f"cannot bind API port {port} or ephemeral port")
            print(
                f"Port {host}:{port} bind failed "
                f"(attempt {bind_attempts}/{max_bind_retries}), "
                f"retry in {bind_retry_delay}s",
                flush=True,
            )
            time.sleep(bind_retry_delay)
            continue
        bind_attempts = 0
        set_api_port_actual(server.server_address[1])
        _write_info_file()

        try:
            print(f"Flask listening on http://{host}:{server.server_address[1]}", flush=True)
            server.serve_forever()
            # 正常情况下只有 shutdown() 会让它返回；这里没人调用，
            # 返回即视为异常。
            print("serve_forever returned unexpectedly, rebuilding server", flush=True)
        except Exception as e:
            print(f"Flask serve loop error (rebuilding listener): {e}", flush=True)
            traceback.print_exc()
        finally:
            try:
                server.server_close()  # 必须先关闭旧 socket，再重新 bind
            except Exception:
                pass

        time.sleep(restart_delay)


# 启动 Flask 服务
try:
    from spider_server import (
        API_PORT,
        start_proxy_server,
    )

    print("Flask app imported successfully", flush=True)
    try:
        start_proxy_server()
        from spider_server import proxy_base_url

        print(f"Python proxy started on {proxy_base_url()}", flush=True)
    except OSError as e:
        print(f"Proxy port is host-managed: {e}", flush=True)
    print(f"Starting Flask server on http://127.0.0.1:{API_PORT}", flush=True)

    # 自愈式阻塞 serving —— 解释器线程永不退出。
    _serve_forever_loop(host="127.0.0.1", port=API_PORT)

except Exception as e:
    print(f"Error starting Flask: {e}", flush=True)
    traceback.print_exc()
    raise
