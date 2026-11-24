// HGhub WebHome 页面桥（04 §13）：注入 WebHome 窗口的 window.fm。
// 只做方法转发：数据方法经 Tauri invoke 走宿主分发，导航与视口在页内处理。
// 不依赖页面自身的任何库；页面可通过 window.fm / window.fongmi / window.webhome 调用。
(function () {
  "use strict";
  // Tauri 2 默认不注入 window.__TAURI__（withGlobalTauri 缺省为 false），
  // 但每个 WebView 一定有 window.__TAURI_INTERNALS__（官方 @tauri-apps/api
  // 的 invoke 就是调它）。优先用它，__TAURI__.core 只作后备。
  function getInvoke() {
    var internals = window.__TAURI_INTERNALS__;
    if (internals && typeof internals.invoke === "function") {
      return function (cmd, args) { return internals.invoke(cmd, args); };
    }
    var core = window.__TAURI__ && window.__TAURI__.core;
    if (core && typeof core.invoke === "function") {
      return function (cmd, args) { return core.invoke(cmd, args); };
    }
    return null;
  }
  function invoke(method, payload) {
    var fn = getInvoke();
    if (!fn) {
      return Promise.reject(new Error("WebHome 桥未就绪"));
    }
    // 宿主命令失败时以裸字符串拒绝（Result<Value, String>）；页面按
    // e.message 取报错原因，字符串没有 message，原因会被吞掉，只剩页面
    // 自己的兜底文案。包成 Error 后，页面才能看到真正的失败原因。
    return fn("webhome_call", { method: method, payload: payload || {} }).catch(function (e) {
      if (e instanceof Error) throw e;
      throw new Error(typeof e === "string" ? e : (e && e.message) || "请求失败");
    });
  }
  function closeWindow() {
    var fn = getInvoke();
    if (!fn) return Promise.resolve({ code: 0 });
    return fn("webhome_close", {});
  }
  function post(method, payload) {
    // 导航与视口不经过宿主，在页内直接处理。
    if (method === "navigation.back") {
      if (window.history.length > 1) {
        window.history.back();
        return Promise.resolve({ code: 0 });
      }
      return closeWindow();
    }
    if (method === "navigation.reload") {
      window.location.reload();
      return Promise.resolve({ code: 0 });
    }
    if (method === "ui.getViewport") {
      return Promise.resolve({
        width: window.innerWidth,
        height: window.innerHeight,
        devicePixelRatio: window.devicePixelRatio || 1,
      });
    }
    if (method === "net.request") {
      // v0.0.88：HPP 的 net.request 返回体把响应正文放在 `data`，
      // 页面按 webhtv 约定读 `body`。SDK 是兼容层，在此补上 body
      // 引用——JS 赋值只是引用，不复制字符串，不增加 IPC 体积；
      // `data` 保留不动。bytes 通路不在本次范围，不映射。
      var mapBody = !payload || payload.responseType !== "bytes";
      return invoke(method, payload).then(function (r) {
        if (mapBody && r && typeof r === "object" && r.body === undefined && r.data !== undefined) {
          r.body = r.data;
        }
        return r;
      });
    }
    return invoke(method, payload);
  }
  function fmInvoke(method, payload) {
    return post(method, payload);
  }

  var player = {
    playUrl: function (url, title, options) {
      return post("player.playUrl", Object.assign({}, options || {}, { url: url, title: title }));
    },
    playVod: function (siteKey, vodId, title, pic, options) {
      if (siteKey && typeof siteKey === "object") return post("player.playVod", siteKey);
      return post("player.playVod", Object.assign({}, options || {}, { siteKey: siteKey, vodId: vodId, title: title, pic: pic }));
    },
    playVodInline: function (opt) { return post("player.playVodInline", opt || {}); },
    preloadArtwork: function (pic, wallPic) { return post("player.preloadArtwork", { pic: pic, wallPic: wallPic }); },
    control: function (action, seconds) { return post("player.control", { action: action, seconds: seconds }); },
    status: function () { return post("player.status", {}); },
    play: function (url, title, options) {
      return post("player.playUrl", Object.assign({}, options || {}, { url: url, title: title }));
    },
  };
  var app = {
    search: function (query) { return post("app.search", { query: query }); },
    openVod: function (opt) { return post("app.openVod", opt || {}); },
    openLive: function () { return post("app.openLive", {}); },
    openKeep: function () { return post("app.openKeep", {}); },
    openSetting: function () { return post("app.openSetting", {}); },
    history: function () { return post("app.history", {}); },
    webhomeSources: function () { return post("app.webhomeSources", {}); },
    openWebhome: function (pluginId) { return post("app.openWebhome", { pluginId: pluginId }); },
  };
  var nav = {
    back: function () { return post("navigation.back", {}); },
    reload: function () { return post("navigation.reload", {}); },
  };
  var cache = {
    set: function (k, v, rule) { return post("cache.set", { k: k, v: v, rule: rule || "" }); },
    get: function (k, rule) { return post("cache.get", { k: k, rule: rule || "" }); },
    del: function (k, rule) { return post("cache.del", { k: k, rule: rule || "" }); },
    clear: function (rule) { return post("cache.clear", { rule: rule || "" }); },
  };
  var net = {
    request: function (url, opt) {
      if (url && typeof url === "object") return post("net.request", url);
      return post("net.request", Object.assign({}, opt || {}, { url: url }));
    },
    // v1 不经代理取图：直接返回原地址。
    resourceUrl: function (url) { return url; },
  };
  function fmReq(url, opt) {
    return post("net.request", Object.assign({}, opt || {}, { url: url }));
  }
  function fmConfig() { return post("config.info", {}); }
  fmConfig.info = fmConfig;
  function fmSearch(keyword) { return post("app.search", { query: keyword }); }
  function fmBack() { return post("navigation.back", {}); }
  function fmReload() { return post("navigation.reload", {}); }
  var fmPan = { play: function (payload) { return post("pan.play", payload || {}); } };
  fmPan.check = function (items) { return post("pan.check", { items: items || [] }); };
  var fmUi = {
    setChrome: function (options) { return post("ui.setChrome", options || {}); },
    setToolbar: function (visible) { return post("ui.setToolbar", { visible: visible !== false }); },
    restoreChrome: function () { return post("ui.restoreChrome", {}); },
    getViewport: function () { return post("ui.getViewport", {}); },
  };
  var fmExt = {
    info: function () { return post("ext.info", {}); },
    log: function (message, data) { return post("ext.log", { message: message, data: data }); },
    toast: function (message) { return post("ext.toast", { message: message }); },
  };
  var config = { info: fmConfig };
  var site = { info: function () { return post("site.info", {}); } };
  var device = { info: function () { return post("device.info", {}); } };
  var history = { list: function () { return post("app.history", {}); } };
  function fmHistory() { return post("app.history", {}); }
  fmHistory.list = fmHistory;

  window.fm = {
    invoke: fmInvoke,
    player: player,
    net: net,
    app: app,
    navigation: nav,
    cache: cache,
    config: config,
    site: site,
    device: device,
    history: history,
    req: fmReq,
    res: net.resourceUrl,
    play: player.playUrl,
    open: function (url) {
      if (url && typeof url === "object") return post("app.openExternal", url);
      return post("app.openExternal", { url: url });
    },
    vod: player.playVod,
    vodInline: player.playVodInline,
    search: fmSearch,
    config_info: fmConfig,
    pan: fmPan,
    ui: fmUi,
    ext: fmExt,
    back: fmBack,
    reload: fmReload,
    history_list: fmHistory,
  };
  window.fm.preloadArtwork = player.preloadArtwork;
  window.fm.ctrl = player.control;
  window.fm.stat = player.status;
  window.fm.openVod = app.openVod;
  window.fm.openLive = app.openLive;
  window.fm.openKeep = app.openKeep;
  window.fm.openSetting = app.openSetting;
  window.fm.config = fmConfig;
  window.fm.site = site.info;
  window.fm.device = device.info;
  window.fm.history = fmHistory;
  window.fm.cache = cache;
  window.fongmi = window.fm;
  window.webhome = window.fm;
  window.fongmiClient = { mode: "webhome", isLeanback: false };

  // 小房子：WebHome 窗口的源切换与关闭控件（04 §14、08 §10）。
  // 右侧中部一个常显的圆形房子按钮（白房子图标、半透明深色底）；
  // 点它直接弹出源面板（列已装的 WebHome 源，点一个切过去，下面一行
  // “关闭”回主界面）；几秒不用面板自己收起，按钮一直留着。窗口常驻
  // 不销毁，× 由此控件代替。
  var houseBtn = document.createElement("button");
  houseBtn.id = "hg-webhome-btn";
  houseBtn.type = "button";
  houseBtn.setAttribute("aria-label", "源切换");
  houseBtn.innerHTML = '<svg viewBox="0 0 24 24" width="22" height="22" fill="#fff" aria-hidden="true"><path d="M12 3 2 12h3v8h6v-6h2v6h6v-8h3L12 3z"/></svg>';
  houseBtn.style.cssText = "position:fixed;right:8px;top:50%;transform:translateY(-50%);z-index:2147483647;width:44px;height:44px;border:none;border-radius:50%;background:rgba(0,0,0,.55);display:flex;align-items:center;justify-content:center;cursor:pointer;";
  var housePanel = document.createElement("div");
  housePanel.id = "hg-webhome-panel";
  housePanel.style.cssText = "position:fixed;right:60px;top:50%;transform:translateY(-50%);z-index:2147483647;width:230px;max-height:60vh;overflow-y:auto;border-radius:12px;background:rgba(20,20,24,.92);color:#fff;box-shadow:0 4px 24px rgba(0,0,0,.4);padding:6px;display:none;";
  var houseErr = document.createElement("div");
  houseErr.style.cssText = "display:none;padding:6px 10px;color:#ff8a80;font-size:12px;line-height:1.4;";
  var houseTimer = null;
  var houseRoot = document.body || document.documentElement;
  houseRoot.appendChild(houseBtn);
  houseRoot.appendChild(housePanel);
  function houseCollapse() {
    houseTimer = null;
    housePanel.style.display = "none";
  }
  function houseArm() {
    if (houseTimer) clearTimeout(houseTimer);
    houseTimer = setTimeout(houseCollapse, 4000);
  }
  function houseShowError(msg) {
    houseErr.textContent = msg;
    houseErr.style.display = "";
  }
  function houseRow(label, opts) {
    var row = document.createElement("button");
    row.type = "button";
    row.textContent = label;
    row.style.cssText = "display:block;width:100%;text-align:left;border:none;background:transparent;color:#fff;font-size:14px;line-height:1.3;padding:10px;border-bottom:1px solid rgba(255,255,255,.08);cursor:pointer;";
    if (opts && opts.disabled) {
      row.disabled = true;
      row.style.opacity = ".45";
      row.style.cursor = "default";
    }
    return row;
  }
  function houseOpenPanel() {
    housePanel.style.display = "block";
    housePanel.innerHTML = "";
    houseErr.style.display = "none";
    houseErr.textContent = "";
    var loading = houseRow("加载中…", { disabled: true });
    housePanel.appendChild(loading);
    housePanel.appendChild(houseErr);
    houseArm();
    post("app.webhomeSources", {}).then(function (sources) {
      housePanel.innerHTML = "";
      (sources || []).forEach(function (s) {
        var row = houseRow(s.name + (s.current ? " · 当前" : ""), { disabled: !s.ready });
        if (!s.current) {
          row.addEventListener("click", function () {
            houseArm();
            post("app.openWebhome", { pluginId: s.id }).catch(function (e) {
              houseShowError(e && e.message ? e.message : "切换失败");
            });
          });
        }
        housePanel.appendChild(row);
      });
      var closeRow = houseRow("关闭");
      closeRow.style.borderBottom = "none";
      closeRow.addEventListener("click", function () { closeWindow(); });
      housePanel.appendChild(closeRow);
      housePanel.appendChild(houseErr);
    }).catch(function (e) {
      houseShowError(e && e.message ? e.message : "加载失败");
    });
  }
  houseBtn.addEventListener("click", function () {
    if (housePanel.style.display === "block") {
      housePanel.style.display = "none";
      houseArm();
    } else {
      houseOpenPanel();
    }
  });
  // 点面板外面只收起面板，小房子留着，4 秒后面板自己收起。
  document.addEventListener("click", function (e) {
    if (housePanel.style.display !== "block") return;
    var t = e.target;
    if (housePanel.contains(t) || houseBtn.contains(t)) return;
    housePanel.style.display = "none";
    houseArm();
  }, true);
  window.__hgWebHome = {
    setToolbar: function (visible) {
      if (visible === false) {
        if (houseTimer) { clearTimeout(houseTimer); houseTimer = null; }
        houseBtn.style.display = "none";
        housePanel.style.display = "none";
      } else {
        houseBtn.style.display = "flex";
        houseCollapse();
      }
    },
  };

  try {
    if (window.document && window.document.documentElement) window.document.documentElement.classList.add("fm-native");
    if (window.CustomEvent) window.dispatchEvent(new CustomEvent("fmsdk"));
  } catch (e) {}
})();
