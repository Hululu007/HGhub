# Python 演示蜘蛛：最小可跑的 TVBox 蜘蛛，供红苹果影视 Python 运行时 v1 演示与测试。
#
# 导入方式：插件中心 → 导入与订阅 → 选择插件文件（挑本文件）或粘贴代码。
# 内容全部是写死的演示数据，播放地址是公开测试流，不联网取分类也能跑；
# searchContent 会按关键字过滤这三条演示数据。
#
# 兼容说明：继承 base.spider.Spider（宿主在解释器启动时注入）；不依赖
# 任何第三方包（只用标准库），所以即使 site-packages 不全也能加载。
import base64

try:
    from base.spider import Spider as BaseSpider
except ImportError:
    # 在宿主之外（例如本地直接跑）没有 base 包时，用空基类兜底，
    # 让文件仍可被 import 与静态扫描。
    class BaseSpider:  # noqa: D101
        pass

_MUX_STREAM = "https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8"
_BIPBOP_STREAM = "https://devstreaming-cdn.apple.com/videos/streaming/examples/img_bipbop_adv_example_hevc/master.m3u8"

_ITEMS = [
    {
        "vod_id": "demo-bbb",
        "vod_name": "Big Buck Bunny（演示）",
        "vod_pic": "https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8",
        "vod_remarks": "演示片",
        "vod_year": "2008",
        "vod_actor": "Blender Foundation",
        "vod_director": "Sacha Goedegebure",
        "vod_area": "荷兰",
        "vod_play_from": "演示源",
        "vod_play_url": "第 1 集$" + _MUX_STREAM + "#第 2 集$" + _MUX_STREAM,
    },
    {
        "vod_id": "demo-bipbop",
        "vod_name": "BipBop（演示）",
        "vod_pic": "",
        "vod_remarks": "演示片",
        "vod_year": "2010",
        "vod_play_from": "演示源",
        "vod_play_url": "第 1 集$" + _BIPBOP_STREAM,
    },
    {
        "vod_id": "demo-sintel",
        "vod_name": "Sintel（演示）",
        "vod_pic": "",
        "vod_remarks": "演示片",
        "vod_year": "2010",
        "vod_play_from": "演示源",
        "vod_play_url": "正片$" + _MUX_STREAM,
    },
]

# 1x1 透明 PNG，供 localProxy 的 act=cover 演示用。
_PNG_1X1 = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8Xw8AAwMCAO+ip1sAAAAASUVORK5CYII="
)


class Spider(BaseSpider):
    def getName(self):
        return "Python 演示源"

    def getDependence(self):
        return []

    def init(self, extend=""):
        pass

    def homeContent(self, filter):
        return {
            "class": [
                {"type_id": "movie", "type_name": "电影"},
                {"type_id": "series", "type_name": "电视剧"},
                {"type_id": "variety", "type_name": "综艺"},
            ],
            "filters": {},
            "list": _ITEMS,
        }

    def homeVideoContent(self):
        return {"list": _ITEMS}

    def categoryContent(self, tid, pg, filter, extend):
        page = int(pg)
        if page > 1:
            return {"list": [], "page": page, "pagecount": 1, "limit": 20, "total": len(_ITEMS)}
        return {
            "list": _ITEMS,
            "page": page,
            "pagecount": 1,
            "limit": 20,
            "total": len(_ITEMS),
        }

    def detailContent(self, ids):
        wanted = ids[0] if ids else ""
        for item in _ITEMS:
            if item["vod_id"] == wanted:
                return {"list": [item]}
        return {"list": []}

    def searchContent(self, key, quick, pg="1"):
        found = [it for it in _ITEMS if key in it["vod_name"]]
        return {
            "list": found,
            "page": int(pg),
            "pagecount": 1,
            "limit": 20,
            "total": len(found),
        }

    def playerContent(self, flag, id, vipFlags):
        # id 是分集地址（detailContent 里 vod_play_url 的 $ 后半段）。
        return {"parse": 0, "url": id, "header": {}}

    def localProxy(self, param):
        if isinstance(param, dict) and param.get("act") == "cover":
            return [200, "image/png", _PNG_1X1, {}]
        return [404, "text/plain", "", {}]

    def isVideoFormat(self, url):
        return url.endswith(".m3u8") or url.endswith(".mp4")

    def manualVideoCheck(self):
        return False

    def destroy(self):
        pass
