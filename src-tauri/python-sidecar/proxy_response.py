# HGhub Python 边车：localProxy 返回归一化。
#
# TVBox 蜘蛛的 localProxy 按约定返回 [status, content_type, body, headers]
# 四元组，但各家写法有出入（body 可能是 str / bytes / 生成器，headers
# 可能缺失）；这里统一成 (status, content_type, body_bytes, headers)，
# 让 /proxy 路由只处理一种形状。

def normalize_proxy_result(result):
    """把 localProxy 的返回值归一化为 (status, content_type, body, headers)。"""
    if not isinstance(result, (list, tuple)):
        raise ValueError("localProxy 必须返回 [status, content_type, body, headers]")
    if len(result) < 3:
        raise ValueError("localProxy 返回值缺字段")
    status = int(result[0])
    content_type = str(result[1] or "application/octet-stream")
    body = result[2]
    headers = result[3] if len(result) > 3 and isinstance(result[3], dict) else {}
    headers = {str(k): str(v) for k, v in headers.items()}
    if body is None:
        body = b""
    elif isinstance(body, str):
        body = body.encode("utf-8")
    elif isinstance(body, (bytes, bytearray)):
        body = bytes(body)
    else:
        # 生成器或其它可迭代体：读成字节串（localProxy 的媒体流走
        # 边车的流式路径，不经过这里；这里只接有限长度的 body）。
        body = b"".join(
            chunk if isinstance(chunk, (bytes, bytearray)) else str(chunk).encode("utf-8")
            for chunk in body
        )
    return status, content_type, body, headers
