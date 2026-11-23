# Python 蜘蛛（TVBox .py 源）

红苹果影视 v0.0.53 起可以在 **iOS** 上导入并运行 TVBox Python 爬虫源（`.py` 文件）。
本页是新手指南：先跑通演示源，再写自己的蜘蛛。协议定义见
[02 §11](../plugin-protocol/02-runtime.md)（Python 运行时）、
[04 §15](../plugin-protocol/04-host-api.md)（`base.spider`）、
[06 §19](../plugin-protocol/06-capabilities.md)（方法映射）。

## 先跑通演示源

仓库里有一份最小可跑的演示蜘蛛 `examples/demo-spider.py`，内容是写死的演示数据，
播放地址是公开测试流（Mux、BipBop），不联网取分类也能跑：

1. 插件中心 → “导入与订阅” → 选择插件文件，挑 `examples/demo-spider.py`
   （或粘贴它的代码导入）。
2. 导入后它在插件中心显示为 `Python 演示源 · 内容源 · Python`，默认启用。
3. 首页、分类、搜索、详情、播放应该全通；搜索会按关键字过滤三条演示数据。

导入的 `.py` 源名字优先取蜘蛛 `getName()` 声明的名字（演示源是 `Python 演示源`），
没有声明时取文件名；id 取自文件名词干（这里是 `py.demo-spider`），版本恒为 `1.0.0`。
更新靠手动重导：重复导入同一文件覆盖同一个插件。

## 写一个最小蜘蛛

类式写法（推荐，与大多数 TVBox 蜘蛛一致）：

```python
try:
    from base.spider import Spider as BaseSpider
except ImportError:
    # 在宿主之外（例如本地直接跑）没有 base 包时，用空基类兜底，
    # 让文件仍可被 import 与静态扫描。
    class BaseSpider:
        pass


class Spider(BaseSpider):
    def getName(self):
        return "我的演示源"

    def homeContent(self, filter):
        return {
            "class": [{"type_id": "movie", "type_name": "电影"}],
            "list": [],
        }

    def categoryContent(self, tid, pg, filter, extend):
        return {"list": [], "page": int(pg), "pagecount": 1, "total": 0}

    def detailContent(self, ids):
        return {"list": [{
            "vod_id": ids[0],
            "vod_name": "示例片",
            "vod_play_from": "线路A",
            "vod_play_url": "第1集$https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8",
        }]}

    def searchContent(self, key, quick, pg="1"):
        return {"list": [], "page": 1, "pagecount": 1, "total": 0}

    def playerContent(self, flag, id, vipFlags):
        return {"parse": 0, "url": id, "header": {}}
```

函数式写法也可以：模块级直接定义同名函数（至少两个内容函数），不用写类。
宿主优先找 `BaseSpider` 子类，其次找名叫 `Spider` 的类，最后按函数式处理。

### 必须实现的方法

| 方法 | 不实现会怎样 |
| --- | --- |
| `detailContent(ids)` + `playerContent(flag, id, vipFlags)` | 缺一不可，否则初始化失败 |
| `homeContent` / `homeVideoContent` / `categoryContent` / `searchContent` 其中至少一个 | 没有内容入口，初始化失败 |

`init(extend="")` 可以不写：类式继承基类的空实现，函数式直接跳过；
写了就会在加载时被调一次，参数恒为 `""`（v1 没有 `meta.settings`）。

### 返回值形状

- **列表**（`categoryContent` / `searchContent` / `homeVideoContent`）：`{"list": [vod, …], "page": 1, "pagecount": 1, "total": 0}`。
  每个 `vod` 至少要有 `vod_id` 与 `vod_name`；缺一的条目会被丢弃，不会让整页失败。
  常用字段：`vod_pic`（封面）、`vod_remarks`（短备注作徽标）、`vod_year`、
  `vod_area` / `vod_lang` / `vod_class`（进标签）、`vod_content`（简介）。
- **首页分类**（`homeContent`）：`{"class": [{"type_id": "movie", "type_name": "电影"}]}`，
  `class` 决定分类页的分类；`list` 有内容时也会在首页显示为一个分区。
- **详情**（`detailContent`）：`{"list": [vod]}`，`vod` 里加
  `vod_play_from`（多线路以 `$$$` 分隔，如 `"线路A$$$线路B"`）与
  `vod_play_url`（分集以 `#` 分隔，每集 `标题$地址`，如
  `"第1集$https://…/a.m3u8#第2集$https://…/b.m3u8"`）。
  播放列表 id 取线路名，`playerContent` 的 `flag` 就是它。
- **播放**（`playerContent`）：`{"parse": 0, "url": "…", "header": {}}`。
  `parse: 0` 是直链；`parse: 1` 只有 url 本身是媒体地址时才按直链处理，
  否则报 `UNSUPPORTED`（TVBox 的 `parse: 1` 是开 WebView 嗅探，宿主没有这个能力）。
  空 url 报“没有返回播放地址”。
- **失败**：返回带非 0 `code` 且没有 `list` 的 dict（如 `{"code": -1, "message": "token 失效"}`），
  宿主按上游失败处理；直接把失败吞成空列表时宿主无法区分，按正常空页处理。

### 参数约定

宿主传参有固定形状，写方法签名时照抄即可：`homeContent` 的 `filter` 恒为 `1`；
`categoryContent` 的 `pg` 恒为字符串；`detailContent` 的 `ids` 恒为列表；
`searchContent` 的 `quick` 恒为 `True`；`playerContent` 的 `vipFlags` 恒为空列表。
`homeContent` 可能在同一次初始化里被调多次（分类与首页各取一次），必须幂等。

## 取数、代理与解析

- **取数**：用基类的 `self.fetch(url)` / `self.post(url, …)`。它优先走宿主网络
  （用该插件的 Cookie 罐、网络规则与限速），宿主桥不可用时回落到 `requests` 直连。
  返回对象有 `.text` / `.content` / `.json()` / `.status_code` / `.headers`，
  与 `requests.Response` 常用面一致。
- **直接用 `requests` / `urllib`** 也可以（`requests` 已打进包），但不走宿主的
  Cookie 罐与网络规则；需要登录态的站优先用 `self.fetch`。
- **图片与播放地址走代理**：`self.getProxyUrl()` 返回边车代理入口地址，
  拼在播放地址或图片地址前面用；边车按 `siteKey` 找到你的蜘蛛，再调你的
  `localProxy(param)`，返回 `[status, contentType, body, headers]` 四元组。
  演示源里 `localProxy` 用 `act=cover` 返回一张 1×1 透明 PNG，可照抄结构。
- **解析 HTML**：v1 没有 `lxml`，`self.html()` 会报明错；直接用 `beautifulsoup4`
  （已打进包）。`pyquery` / `pycryptodome` v1 也不带，需要它们的蜘蛛加载即报明错。
- **日志**：`self.log("…")` 写进该插件的日志面板；`self.setCache` / `getCache` /
  `delCache` 是进程级缓存（按站点隔离），应用重启即失。

## 能用什么包

v1 打进包的纯 Python 包：`requests`（+ `urllib3` / `certifi` / `idna` /
`charset-normalizer`）、`flask` / `werkzeug`、`beautifulsoup4` / `soupsieve`、
`httpx`。标准库全量可用。`lxml` / `pyquery` / `pycryptodome` 需要 iOS 交叉编译，
v1 不带；`pyexecjs` 不在包内（它需要本机装 Node）。

## 限制（v1）

- 只在 iOS 上可用；其他平台导入后初始化报 `UNSUPPORTED`，插件保留文件、可重试。
- 没有 `meta.settings`（`init` 只传空串）、没有持久存储桥、没有 `endpoints` /
  `auth` / `ui` 能力；`liveContent` / `suggest` / `trending` /
  `transformResource` 不映射。
- 版本恒为 `1.0.0`，更新检查不触发，改了代码就重导一次。
- 刷新播放只是重新跑一次 `playerContent`，不要返回缓存的旧地址。

## 本地验证

- 边车测试：`python3 src-tauri/python-sidecar/tests/test_sidecar.py`
  （全模式）；`HG_PY_TEST_V1_ONLY=1 python3 src-tauri/python-sidecar/tests/test_sidecar.py`
  （v1-only 模式，模拟 iOS 缺二进制包的环境）。跑的是本机 CPython 3.12，
  不是 iOS。
- Rust 集成测试：`cargo test -p hghub-hpp --test python_spider`
  （需在 `src-tauri/crates/hpp` 里跑；该 crate 是 path 依赖，不是 workspace 成员）。
- iOS 构建与真机播放只能等 CI / 真机验收。
