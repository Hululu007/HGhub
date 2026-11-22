// HGhub WebHome 页面桥（04 §13）：注入 WebHome 窗口的 window.fm。
// 只做方法转发：数据方法经 Tauri invoke 走宿主分发，导航与视口在页内处理。
// 不依赖页面自身的任何库；页面可通过 window.fm / window.fongmi / window.webhome 调用。
(function () {
  "use strict";
  function invoke(method, payload) {
    var core = window.__TAURI__ && window.__TAURI__.core;
    if (!core || typeof core.invoke !== "function") {
      return Promise.reject(new Error("WebHome 桥未就绪"));
    }
    return core.invoke("webhome_call", { method: method, payload: payload || {} });
  }
  function closeWindow() {
    var core = window.__TAURI__ && window.__TAURI__.core;
    if (!core || typeof core.invoke !== "function") return Promise.resolve({ code: 0 });
    return core.invoke("webhome_close", {});
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

  // 退出按钮：WebHome 窗口是独立 WebView，页面自己的返回关不掉它。
  var btn = document.createElement("button");
  btn.id = "hg-webhome-close";
  btn.type = "button";
  btn.setAttribute("aria-label", "关闭");
  btn.textContent = "×";
  btn.style.cssText = "position:fixed;top:max(12px, env(safe-area-inset-top));right:12px;z-index:2147483647;width:32px;height:32px;border-radius:50%;border:none;background:rgba(0,0,0,.55);color:#fff;font-size:20px;line-height:1;display:flex;align-items:center;justify-content:center;cursor:pointer;";
  btn.addEventListener("click", function () { closeWindow(); });
  (document.body || document.documentElement).appendChild(btn);
  window.__hgWebHome = {
    setToolbar: function (visible) {
      btn.style.display = visible ? "" : "none";
    },
  };

  try {
    if (window.document && window.document.documentElement) window.document.documentElement.classList.add("fm-native");
    if (window.CustomEvent) window.dispatchEvent(new CustomEvent("fmsdk"));
  } catch (e) {}
})();
