// WebHome SDK 回归检查（v0.0.88）：net.request 的 data→body 映射。
// HPP 的 net.request 返回体把响应正文放在 `data`；页面按 webhtv 约定
// 读 `body`。SDK 是兼容层，在 post() 的 net.request 分支里补上 body
// 引用（不复制、不改 Rust 返回体、不改协议定义）。
// 用法：node scripts/test-webhome-sdk.mjs
// 仓库没有 webhome_sdk.js 的现成 harness，所以用 Node vm 直接加载
// SDK，mock window.__TAURI_INTERNALS__.invoke，断言映射行为。
import assert from "node:assert";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import vm from "node:vm";

const SDK_PATH = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../src-tauri/src/webhome_sdk.js",
);

function fakeElement() {
  return {
    style: {},
    textContent: "",
    innerHTML: "",
    setAttribute() {},
    addEventListener() {},
    appendChild() {},
    contains() {
      return false;
    },
  };
}

function loadSdk(mockInvoke) {
  // body.appendChild 记下来，供用例检查注入的房子控件。
  const appended = [];
  const bodyEl = fakeElement();
  bodyEl.appendChild = (el) => { appended.push(el); };
  const sandbox = {
    console,
    setTimeout,
    clearTimeout,
    CustomEvent: class {
      constructor(type) {
        this.type = type;
      }
    },
    document: {
      createElement: () => fakeElement(),
      body: bodyEl,
      documentElement: { classList: { add() {} } },
      addEventListener() {},
      // 页面没有 #aggBtn 时返回 null（setupAggBtnGuard 会直接返回）。
      getElementById: () => null,
    },
    dispatchEvent() {},
    innerWidth: 800,
    innerHeight: 600,
    devicePixelRatio: 1,
    history: { length: 1 },
    location: { reload() {} },
  };
  sandbox.window = sandbox;
  sandbox.__TAURI_INTERNALS__ = { invoke: mockInvoke };
  vm.createContext(sandbox);
  vm.runInContext(fs.readFileSync(SDK_PATH, "utf8"), sandbox, {
    filename: "webhome_sdk.js",
  });
  loadSdk.appended = appended;
  return sandbox.fm;
}

const LIST_HTML = '<div class="module-item">尘缘</div>';
const BASE = {
  ok: true,
  status: 200,
  url: "https://woggpan.888484.xyz/vodshow/1--------1---.html",
  headers: {},
  setCookies: [],
  fromCache: false,
};

// 1. text：页面 req() 的场景——body 等于 data，且 data 还在。
{
  const fm = loadSdk(() =>
    Promise.resolve({ ...BASE, data: LIST_HTML }),
  );
  const r = await fm.req("https://woggpan.888484.xyz/vodshow/1--------1---.html", {
    responseType: "text",
  });
  assert.strictEqual(r.body, LIST_HTML);
  assert.strictEqual(r.data, LIST_HTML);
  // 页面实际的判定：r.body||'' 非空且含 module-item，race 不会报拦截。
  assert.ok((r.body || "").includes("module-item"));
  console.log("ok 1: text data→body");
}

// 2. json：data 是解析后的对象，body 是同一引用（页面 615 行接得住对象）。
{
  const data = { list: [{ title: "尘缘" }] };
  const fm = loadSdk(() =>
    Promise.resolve({ ...BASE, data }),
  );
  const r = await fm.req("https://example.invalid/api", { responseType: "json" });
  assert.strictEqual(r.body, data);
  assert.strictEqual(r.data, data);
  console.log("ok 2: json data→body (same reference)");
}

// 3. data 缺失时不崩，body 保持 undefined。
{
  const fm = loadSdk(() => Promise.resolve({ ok: true, status: 200 }));
  const r = await fm.req("https://example.invalid/empty", { responseType: "text" });
  assert.strictEqual(r.body, undefined);
  console.log("ok 3: missing data does not crash");
}

// 4. 已有 body 时不覆盖。
{
  const fm = loadSdk(() =>
    Promise.resolve({ ...BASE, data: "d", body: "b" }),
  );
  const r = await fm.req("https://example.invalid/x", { responseType: "text" });
  assert.strictEqual(r.body, "b");
  console.log("ok 4: existing body is kept");
}

// 5. bytes 通路不在本次范围：不映射 body。
{
  const fm = loadSdk(() =>
    Promise.resolve({ ...BASE, data: [60, 100, 105, 118, 62] }),
  );
  const r = await fm.req("https://example.invalid/x", { responseType: "bytes" });
  assert.strictEqual(r.body, undefined);
  assert.deepStrictEqual(r.data, [60, 100, 105, 118, 62]);
  console.log("ok 5: bytes is not mapped");
}

// 6. 另一入口 fm.net.request 同样映射（与 fm.req 共用 post 分支）。
{
  const fm = loadSdk(() =>
    Promise.resolve({ ...BASE, data: LIST_HTML }),
  );
  const r = await fm.net.request("https://woggpan.888484.xyz/vodshow/1--------1---.html", {
    responseType: "text",
  });
  assert.strictEqual(r.body, LIST_HTML);
  console.log("ok 6: fm.net.request maps too");
}

// 7. invoke 拒绝时原样透传，不吞错。
{
  const fm = loadSdk(() => Promise.reject(new Error("boom")));
  await assert.rejects(
    fm.req("https://example.invalid/x", { responseType: "text" }),
    /boom/,
  );
  console.log("ok 7: invoke rejection propagates");
}

// 8. 房子按钮：几何尺寸与图标尺寸都走内联 !important，不怕页面 svg 规则
// 覆盖（Tab S9 新版页上房子缩成小点点：按钮本身 40px 正常，SVG 被页面
// 规则压到只剩几点像素——width/height 属性是展示属性，扛不住页面 CSS）。
{
  loadSdk(() => Promise.resolve({ ...BASE, data: LIST_HTML }));
  const btn = loadSdk.appended.find((el) => el.id === "hg-webhome-btn");
  assert.ok(btn, "house button appended to body");
  assert.ok(
    String(btn.style.cssText).includes("width:40px!important"),
    btn.style.cssText,
  );
  assert.ok(
    String(btn.innerHTML).includes("width:22px!important"),
    btn.innerHTML,
  );
  console.log("ok 8: house button uses !important sizing");
}

console.log("webhome_sdk data→body: all 8 passed");
