// Node 边车 host.js 的集成测试：用本机 Node（v24）跑 host.js，
// 对本地 HTTP/HTTPS 服务器与 mock 代理发请求。
//
// 注意：本机 Node 是 v24，目标引擎是 Node 18.20.4——所用核心 API
// （http/https/net/tls、自定义 agent、lookup）在 18→24 间稳定，
// 仍要在真机上以 Node 18 复核（见 node-engine.md §10.1）。
// 但有一个地方 18 与 24 默认行为不同：autoSelectFamily 在 18 上默认关、
// 24 上默认开——lookup 的 all:false 路径端到端测不到（见下面
// 'lookup 按 Node 契约回地址' 用例，只能把 makeLookup 导出后单独调）。
// v0.0.76 起 host.js 不再显式设 autoSelectFamily（与 Arcane 一致，
// Node 18 只试首个地址）；这个“不设”本身也要用 requestOptions 的
// 入参断言钉住，端到端测不到。
// v0.0.79：直连 TLS 被断换地址重试回来了（不用 pinning）：doFetch
// 自己拿系统顺序地址列表逐个试。轮换的端到端测试用 localhost——
// 本机解析回 2 个地址（::1 与 127.0.0.1，顺序以系统为准，测试里
// 先解析再按顺序布坏/好端点，不硬编码顺序）；证书 SAN 因此同时
// 含 myhost.test 与 localhost。IP 字面量 URL 只回 1 个地址，
// 测“仅 1 个地址，无处可换”的诊断。
//
// 跑：node --test src-tauri/node-sidecar/tests/test_host.mjs
import { test, before, after } from 'node:test';
import assert from 'node:assert/strict';
import { spawn, execFileSync } from 'node:child_process';
import http from 'node:http';
import https from 'node:https';
import net from 'node:net';
import dns from 'node:dns';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const SIDECAR = path.resolve(import.meta.dirname, '..');
const HOST = path.join(SIDECAR, 'host.js');
const TMP = fs.mkdtempSync(path.join(os.tmpdir(), 'hg-node-test-'));

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function startHost(extraEnv = {}) {
  const infoPath = path.join(TMP, `node-sidecar-${Date.now()}-${Math.random()}.json`);
  const child = spawn(process.execPath, [HOST], {
    cwd: SIDECAR,
    env: { ...process.env, HG_NODE_INFO_PATH: infoPath, ...extraEnv },
    stdio: 'ignore',
  });
  const deadline = Date.now() + 10000;
  let base = null;
  while (Date.now() < deadline) {
    try {
      const info = JSON.parse(fs.readFileSync(infoPath, 'utf8'));
      const r = await fetch(`${info.apiBase}/health`);
      if (r.ok) {
        base = info.apiBase;
        break;
      }
    } catch {
      // 未就绪：继续等。
    }
    await sleep(100);
  }
  assert.ok(base, 'host.js 未在 10 秒内就绪');
  return { child, base, infoPath };
}

function stopHost(child) {
  child.kill('SIGTERM');
}

async function postRequest(base, body) {
  const r = await fetch(`${base}/request`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body),
  });
  return r.json();
}

// 从 socket 上精确读 n 字节的缓冲读取器（mock 代理握手用）。
function makeReader(socket) {
  let buf = Buffer.alloc(0);
  const waiters = [];
  socket.on('data', (chunk) => {
    buf = Buffer.concat([buf, chunk]);
    while (waiters.length > 0 && buf.length >= waiters[0].n) {
      const w = waiters.shift();
      const out = buf.subarray(0, w.n);
      buf = buf.subarray(w.n);
      w.resolve(out);
    }
  });
  return {
    read(n) {
      if (buf.length >= n) {
        const out = buf.subarray(0, n);
        buf = buf.subarray(n);
        return Promise.resolve(out);
      }
      return new Promise((resolve) => waiters.push({ n, resolve }));
    },
  };
}

// localhost 的系统解析结果（顺序以系统为准）：本机回 [::1, 127.0.0.1]。
// 轮换测试先解析再按顺序布坏/好端点，不硬编码顺序。
function resolveLocalhost() {
  return new Promise((resolve, reject) => {
    dns.lookup('localhost', { all: true, verbatim: true }, (err, addrs) => {
      if (err) return reject(err);
      resolve(addrs);
    });
  });
}

function waitFor(predicate, timeoutMs = 5000) {
  const deadline = Date.now() + timeoutMs;
  return new Promise((resolve, reject) => {
    const tick = () => {
      const v = predicate();
      if (v !== undefined) return resolve(v);
      if (Date.now() > deadline) return reject(new Error('等待超时'));
      setTimeout(tick, 10);
    };
    tick();
  });
}

// 模拟“TCP 接通后 TLS 被断”的坏端点：收到 ClientHello 后延迟 destroy——
// TCP 已建立，Node 不会因此回落到下一个 pinned 地址（v0.0.74 真机报错
// 的复现：Client network socket disconnected before secure TLS
// connection was established）。延迟 150ms 是为了让 TCP 先算“已建立”
// （立即 destroy 时 Node 会当成 TCP 失败而回落，测不到这条路）。
function startBadTlsServer(host, port, delayMs = 150) {
  const hits = { connections: 0, clientHellos: 0 };
  const server = net.createServer((socket) => {
    hits.connections += 1;
    socket.once('data', () => {
      hits.clientHellos += 1;
      setTimeout(() => socket.destroy(), delayMs);
    });
  });
  return new Promise((resolve) => server.listen(port, host, () => resolve({ server, hits })));
}

// ---------------------------------------------------------------------------
// HTTPS 测试用的 CA 与证书（openssl 现生成，只用于本机测试）。
// ---------------------------------------------------------------------------
const certs = {};
before(() => {
  const dir = path.join(TMP, 'certs');
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(
    path.join(dir, 'ca.cnf'),
    '[req]\ndistinguished_name=dn\nx509_extensions=v3_ca\n[dn]\n[v3_ca]\nbasicConstraints=critical,CA:TRUE\nkeyUsage=critical,keyCertSign,cRLSign\n',
  );
  fs.writeFileSync(
    path.join(dir, 'server.cnf'),
    '[req]\ndistinguished_name=dn\n[dn]\n[v3]\nsubjectAltName=DNS:myhost.test,DNS:localhost\n',
  );
  const run = (args) => execFileSync('openssl', args, { cwd: dir, stdio: 'pipe' });
  run(['req', '-x509', '-newkey', 'rsa:2048', '-keyout', 'ca-key.pem', '-out', 'ca.pem', '-days', '2', '-nodes', '-subj', '/CN=TestCA', '-config', 'ca.cnf']);
  run(['req', '-newkey', 'rsa:2048', '-keyout', 'key.pem', '-out', 'csr.pem', '-nodes', '-subj', '/CN=myhost.test', '-config', 'server.cnf']);
  run(['x509', '-req', '-in', 'csr.pem', '-CA', 'ca.pem', '-CAkey', 'ca-key.pem', '-CAcreateserial', '-out', 'cert.pem', '-days', '2', '-extensions', 'v3', '-extfile', 'server.cnf']);
  certs.ca = path.join(dir, 'ca.pem');
  certs.key = fs.readFileSync(path.join(dir, 'key.pem'));
  certs.cert = fs.readFileSync(path.join(dir, 'cert.pem'));
});

after(() => {
  fs.rmSync(TMP, { recursive: true, force: true });
});

test('health 返回 ok 与 node 版本', { timeout: 20000 }, async () => {
  const { child, base } = await startHost();
  try {
    const r = await fetch(`${base}/health`);
    assert.equal(r.status, 200);
    const v = await r.json();
    assert.equal(v.ok, true);
    assert.match(v.node, /^v\d+\./);
  } finally {
    stopHost(child);
  }
});

test('默认端口被占时落临时端口', { timeout: 20000 }, async () => {
  const occupier = net.createServer();
  await new Promise((resolve) => occupier.listen(55101, '127.0.0.1', resolve));
  const { child, base } = await startHost();
  try {
    const port = Number(new URL(base).port);
    assert.notEqual(port, 55101);
    const r = await fetch(`${base}/health`);
    assert.equal(r.status, 200);
  } finally {
    stopHost(child);
    occupier.close();
  }
});

test('只发页面给的头、无 sec-ch-ua、HTTP/1.1', { timeout: 20000 }, async () => {
  const seen = {};
  const server = http.createServer((req, res) => {
    let body = '';
    req.on('data', (c) => {
      body += c;
    });
    req.on('end', () => {
      seen.version = req.httpVersion;
      seen.headers = req.headers;
      seen.body = body;
      res.writeHead(200, { 'content-type': 'text/plain' });
      res.end(`echo:${body}`);
    });
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const port = server.address().port;
  const { child, base } = await startHost();
  try {
    // GET：无 body，不应有 Content-Length。
    const get = await postRequest(base, {
      url: `http://127.0.0.1:${port}/x`,
      method: 'GET',
      headers: { 'User-Agent': 'Mozilla/5.0', Referer: 'https://example.com/' },
      timeoutMs: 10000,
      proxyUrl: null,
      pinnedAddrs: [`127.0.0.1:${port}`],
    });
    assert.equal(get.status, 200);
    assert.equal(Buffer.from(get.bodyBase64, 'base64').toString(), 'echo:');
    assert.equal(seen.version, '1.1');
    assert.equal(seen.headers['user-agent'], 'Mozilla/5.0');
    assert.equal(seen.headers.referer, 'https://example.com/');
    assert.equal(seen.headers['sec-ch-ua'], undefined);
    assert.equal(seen.headers.accept, undefined);
    assert.equal(seen.headers['accept-language'], undefined);
    assert.equal(seen.headers['content-length'], undefined);

    // POST：有 body 时补 Content-Length。
    const post = await postRequest(base, {
      url: `http://127.0.0.1:${port}/x`,
      method: 'POST',
      headers: { 'User-Agent': 'Mozilla/5.0', 'Content-Type': 'application/json' },
      bodyBase64: Buffer.from('{"a":1}').toString('base64'),
      timeoutMs: 10000,
      proxyUrl: null,
      pinnedAddrs: [`127.0.0.1:${port}`],
    });
    assert.equal(post.status, 200);
    assert.equal(Buffer.from(post.bodyBase64, 'base64').toString(), 'echo:{"a":1}');
    assert.equal(seen.headers['content-length'], '7');
    assert.equal(seen.headers['content-type'], 'application/json');
  } finally {
    stopHost(child);
    server.close();
  }
});

test('lookup 固定 IP 时 SNI 与 Host 仍是 hostname', { timeout: 20000 }, async () => {
  const seen = {};
  const server = https.createServer({ key: certs.key, cert: certs.cert }, (req, res) => {
    seen.servername = req.socket.servername;
    seen.host = req.headers.host;
    res.writeHead(200, { 'content-type': 'text/plain' });
    res.end('secure-ok');
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const port = server.address().port;
  const { child, base } = await startHost({ NODE_EXTRA_CA_CERTS: certs.ca });
  try {
    const result = await postRequest(base, {
      url: `https://myhost.test:${port}/secure`,
      method: 'GET',
      headers: { 'User-Agent': 'Mozilla/5.0' },
      timeoutMs: 10000,
      proxyUrl: null,
      pinnedAddrs: [`127.0.0.1:${port}`],
    });
    assert.equal(result.status, 200);
    assert.equal(Buffer.from(result.bodyBase64, 'base64').toString(), 'secure-ok');
    assert.equal(seen.servername, 'myhost.test');
    assert.equal(seen.host, `myhost.test:${port}`);
  } finally {
    stopHost(child);
    server.close();
  }
});

test('lookup 按 Node 契约回地址：all:true 回数组、all:false 回单个地址', { timeout: 20000 }, async () => {
  // 回归测试（v0.0.73）：真机上出现过的 pinned 列表形态——玩偶站域名
  // 解析出 24 个 IPv4，蜡笔/木偶/臻选各 1 个。旧代码无条件回数组，
  // Node 18（autoSelectFamily 默认关，lookup 以 all:false 调用）把它
  // 当成单个 IP，抛 Invalid IP address: [ [Object], ... ]（个数即
  // pinned 个数）；本机 Node 24 默认 all:true，端到端测不到这条路，
  // 只能把 makeLookup 导出后单独调。
  const script = `
    const { makeLookup } = require(${JSON.stringify(HOST)});
    const many = Array.from({ length: 24 }, (_, i) => '93.184.216.' + (i + 1) + ':443');
    const lk = makeLookup(many);
    lk('example.com', { all: true }, (err, addrs) => {
      console.log('ALL_TRUE ' + JSON.stringify(addrs));
    });
    lk('example.com', {}, (err, addr, family) => {
      console.log('ALL_FALSE ' + JSON.stringify(addr) + ' ' + family);
    });
    const one = makeLookup(['93.184.216.34:443']);
    one('example.com', { all: false }, (err, addr, family) => {
      console.log('ONE_FALSE ' + JSON.stringify(addr) + ' ' + family);
    });
    const v6 = makeLookup(['[2606:4700:4700::1111]:443']);
    v6('example.com', {}, (err, addr, family) => {
      console.log('V6_FALSE ' + JSON.stringify(addr) + ' ' + family);
    });
    process.exit(0);
  `;
  const out = execFileSync(process.execPath, ['-e', script], { encoding: 'utf8' });
  const lines = out.trim().split('\n');
  const line = (prefix) => lines.find((l) => l.startsWith(prefix)).slice(prefix.length);
  const allTrue = JSON.parse(line('ALL_TRUE '));
  assert.equal(allTrue.length, 24);
  assert.equal(allTrue[0].address, '93.184.216.1');
  assert.equal(allTrue[0].family, 4);
  assert.equal(allTrue[23].address, '93.184.216.24');
  assert.equal(allTrue[23].family, 4);
  // all:false 时必须回字符串 + family，绝不能回数组（旧代码在这里
  // 回数组，Node 18 直接抛 Invalid IP address）。
  assert.equal(line('ALL_FALSE '), '"93.184.216.1" 4');
  assert.equal(line('ONE_FALSE '), '"93.184.216.34" 4');
  assert.equal(line('V6_FALSE '), '"2606:4700:4700::1111" 6');
});

test('requestOptions：无 pinned 时不用自定义 lookup、不设 autoSelectFamily', { timeout: 20000 }, async () => {
  // 契约测试（v0.0.76）：与 Arcane 完全一致——Node 用默认 lookup，
  // 不传自定义 lookup、不显式设 autoSelectFamily。Node 18.20.4 的
  // autoSelectFamily 会在首个地址 250ms 未接通时并行试下一个地址，
  // 请求可能落到直连边缘而不是首个地址；不设它，Node 18 只试首个
  // 地址（本机 Node 24 默认开，端到端测不到，只能直接断言入参）。
  const script = `
    const { requestOptions } = require(${JSON.stringify(HOST)});
    const empty = requestOptions({ url: 'https://example.com/x', method: 'GET', headers: {}, pinnedAddrs: [], proxyUrl: null }, 10000);
    console.log('EMPTY_LOOKUP ' + ('lookup' in empty));
    console.log('EMPTY_AUTO ' + ('autoSelectFamily' in empty));
    const pinned = requestOptions({ url: 'https://example.com/x', method: 'GET', headers: {}, pinnedAddrs: ['93.184.216.34:443'], proxyUrl: null }, 10000);
    console.log('PINNED_LOOKUP ' + (typeof pinned.lookup));
    console.log('PINNED_AUTO ' + ('autoSelectFamily' in pinned));
    // v0.0.79：doFetch 轮换时传入本次地址（单地址 lookup，all:false
    // 回字符串 + family，all:true 回 1 元素数组——v0.0.73 的契约），
    // 仍不设 autoSelectFamily。
    const addrs = requestOptions({ url: 'https://example.com:8443/x', method: 'GET', headers: {}, pinnedAddrs: [], proxyUrl: null }, 10000, [{ address: '93.184.216.34', family: 4 }]);
    console.log('ADDRS_LOOKUP ' + (typeof addrs.lookup));
    console.log('ADDRS_AUTO ' + ('autoSelectFamily' in addrs));
    addrs.lookup('example.com', { all: false }, (err, addr, family) => {
      console.log('ADDRS_ONE ' + JSON.stringify({ err: err && err.code, addr, family }));
    });
    addrs.lookup('example.com', { all: true }, (err, list) => {
      console.log('ADDRS_ALL ' + JSON.stringify({ err: err && err.code, list }));
    });
    process.exit(0);
  `;
  const out = execFileSync(process.execPath, ['-e', script], { encoding: 'utf8' });
  const lines = out.trim().split('\n');
  const line = (prefix) => lines.find((l) => l.startsWith(prefix)).slice(prefix.length);
  assert.equal(line('EMPTY_LOOKUP '), 'false');
  assert.equal(line('EMPTY_AUTO '), 'false');
  assert.equal(line('PINNED_LOOKUP '), 'function');
  assert.equal(line('PINNED_AUTO '), 'false');
  assert.equal(line('ADDRS_LOOKUP '), 'function');
  assert.equal(line('ADDRS_AUTO '), 'false');
  assert.deepEqual(JSON.parse(line('ADDRS_ONE ')), { err: null, addr: '93.184.216.34', family: 4 });
  assert.deepEqual(JSON.parse(line('ADDRS_ALL ')), { err: null, list: [{ address: '93.184.216.34', family: 4 }] });
});

test('无 pinned 地址时走 Node 默认 lookup（localhost 能通）', { timeout: 20000 }, async () => {
  // v0.0.76：Rust 不再传 pinnedAddrs，Node 用默认 lookup 解析。
  // v0.0.79：localhost 解析出 [::1, 127.0.0.1]，首个地址 TCP 被拒
  // 后轮换到第二个——本测试同时覆盖 http 的 TCP 失败轮换。
  const server = http.createServer((req, res) => {
    res.writeHead(200, { 'content-type': 'text/plain' });
    res.end('default-lookup-ok');
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const port = server.address().port;
  const { child, base } = await startHost();
  try {
    const result = await postRequest(base, {
      url: `http://localhost:${port}/x`,
      method: 'GET',
      headers: { 'User-Agent': 'Mozilla/5.0' },
      timeoutMs: 10000,
      proxyUrl: null,
      pinnedAddrs: [],
    });
    assert.equal(result.status, 200);
    assert.equal(Buffer.from(result.bodyBase64, 'base64').toString(), 'default-lookup-ok');
  } finally {
    stopHost(child);
    server.close();
  }
});

test('TLS 握手被断时只试一次：单个地址无处可换（v0.0.79）', { timeout: 20000 }, async () => {
  // v0.0.79：IP 字面量 URL 只解析出 1 个地址，轮换无处可换——坏端点
  // 只应被试一次（clientHellos === 1），报错直接说明这一点（已试
  // 地址 + 仅 1 个地址）。直连/代理都一样：失败就是失败，诊断在
  // 报错里（Rust 层还会再 append 解析总数）。
  const { server: bad, hits } = await startBadTlsServer('127.0.0.2', 0);
  const port = bad.address().port;
  const { child, base } = await startHost();
  try {
    const result = await postRequest(base, {
      url: `https://127.0.0.2:${port}/x`,
      method: 'GET',
      headers: { 'User-Agent': 'Mozilla/5.0' },
      timeoutMs: 10000,
      proxyUrl: null,
      pinnedAddrs: [],
    });
    assert.equal(result.code, 'NETWORK');
    assert.match(result.error, /Client network socket disconnected before secure TLS connection was established/);
    assert.equal(hits.clientHellos, 1, '单个地址无处可换，坏端点只应被试一次');
    assert.match(result.error, /已试: 127\.0\.0\.2；仅 1 个地址，无处可换/);
  } finally {
    stopHost(child);
    bad.close();
  }
});

test('TLS 握手被断时换下一个地址重试（v0.0.79）', { timeout: 20000 }, async () => {
  // v0.0.79 回归：真机上玩偶/木偶站报的 TLS 断开。首个地址 TCP 接通、
  // TLS 握手被断时，Node 的 Happy Eyeballs 不会试下一个（只在 TCP
  // 接通失败时回落）；doFetch 必须显式排除掉 TLS 失败的地址再试
  // 下一个。localhost 解析出 2 个地址：坏端点放第一个、好端点放
  // 第二个（同一端口），请求最终应成功，且坏端点确实被试过。
  const addrs = await resolveLocalhost();
  assert.ok(addrs.length >= 2, `localhost 应至少解析出 2 个地址，实际：${JSON.stringify(addrs)}`);
  const { server: bad, hits } = await startBadTlsServer(addrs[0].address, 0);
  const port = bad.address().port;
  const good = https.createServer({ key: certs.key, cert: certs.cert }, (req, res) => {
    res.writeHead(200, { 'content-type': 'text/plain' });
    res.end('secure-ok');
  });
  await new Promise((resolve) => good.listen(port, addrs[1].address, resolve));
  const { child, base } = await startHost({ NODE_EXTRA_CA_CERTS: certs.ca });
  try {
    const result = await postRequest(base, {
      url: `https://localhost:${port}/x`,
      method: 'GET',
      headers: { 'User-Agent': 'Mozilla/5.0' },
      timeoutMs: 10000,
      proxyUrl: null,
      pinnedAddrs: [],
    });
    assert.equal(result.status, 200);
    assert.equal(Buffer.from(result.bodyBase64, 'base64').toString(), 'secure-ok');
    assert.ok(hits.clientHellos >= 1, '坏端点应先被试过（TLS 失败）再轮换到好端点');
  } finally {
    stopHost(child);
    bad.close();
    good.close();
  }
});

test('TLS 握手被断时换下一个 pinned 地址重试（v0.0.79）', { timeout: 20000 }, async () => {
  // pinnedAddrs 非空时轮换用它当地址列表（测试、或将来恢复固定时用；
  // 生产上 Rust 自 v0.0.76 起传空，走 dns.lookup）。首个 pinned 地址
  // TLS 被断 → 换第二个。
  const { server: bad, hits } = await startBadTlsServer('127.0.0.2', 0);
  const port = bad.address().port;
  const good = https.createServer({ key: certs.key, cert: certs.cert }, (req, res) => {
    res.writeHead(200, { 'content-type': 'text/plain' });
    res.end('secure-ok');
  });
  await new Promise((resolve) => good.listen(port, '127.0.0.1', resolve));
  const { child, base } = await startHost({ NODE_EXTRA_CA_CERTS: certs.ca });
  try {
    const result = await postRequest(base, {
      url: `https://myhost.test:${port}/x`,
      method: 'GET',
      headers: { 'User-Agent': 'Mozilla/5.0' },
      timeoutMs: 10000,
      proxyUrl: null,
      pinnedAddrs: [`127.0.0.2:${port}`, `127.0.0.1:${port}`],
    });
    assert.equal(result.status, 200);
    assert.equal(Buffer.from(result.bodyBase64, 'base64').toString(), 'secure-ok');
    assert.ok(hits.clientHellos >= 1, '坏端点应先被试过（TLS 失败）再轮换');
  } finally {
    stopHost(child);
    bad.close();
    good.close();
  }
});

test('全部地址都 TLS 失败时报错带已试地址与总数（v0.0.79）', { timeout: 20000 }, async () => {
  // 两个地址都断：报错应是首个错误（与轮换前一致），附上全部已试
  // 地址与解析总数，让真机测试能区分“一个坏边缘”与“全部都坏”。
  const addrs = await resolveLocalhost();
  assert.ok(addrs.length >= 2, `localhost 应至少解析出 2 个地址，实际：${JSON.stringify(addrs)}`);
  const { server: bad1, hits: hits1 } = await startBadTlsServer(addrs[0].address, 0);
  const port = bad1.address().port;
  const { server: bad2, hits: hits2 } = await startBadTlsServer(addrs[1].address, port);
  const { child, base } = await startHost();
  try {
    const result = await postRequest(base, {
      url: `https://localhost:${port}/x`,
      method: 'GET',
      headers: { 'User-Agent': 'Mozilla/5.0' },
      timeoutMs: 10000,
      proxyUrl: null,
      pinnedAddrs: [],
    });
    assert.equal(result.code, 'NETWORK');
    assert.match(result.error, /Client network socket disconnected before secure TLS connection was established/);
    assert.ok(
      result.error.includes(`（已试: ${addrs[0].address}, ${addrs[1].address}；共 2 个地址）`),
      `报错应带全部已试地址与解析总数：${result.error}`,
    );
    assert.ok(hits1.clientHellos >= 1, '第一个坏端点应被试过');
    assert.ok(hits2.clientHellos >= 1, '第二个坏端点也应被试过');
  } finally {
    stopHost(child);
    bad1.close();
    bad2.close();
  }
});

test('超时不轮换（v0.0.79）', { timeout: 20000 }, async () => {
  // 黑洞端点：接通后不响应，烧满超时——超时不是可重试的失败，
  // 好端点一次都不应被试（轮换不会把单次请求拖得比超时更长）。
  const addrs = await resolveLocalhost();
  assert.ok(addrs.length >= 2, `localhost 应至少解析出 2 个地址，实际：${JSON.stringify(addrs)}`);
  const sockets = new Set();
  const blackhole = net.createServer((socket) => {
    sockets.add(socket);
    socket.on('close', () => sockets.delete(socket));
  });
  await new Promise((resolve) => blackhole.listen(0, addrs[0].address, resolve));
  const port = blackhole.address().port;
  let goodConnections = 0;
  const good = https.createServer({ key: certs.key, cert: certs.cert }, (req, res) => {
    res.writeHead(200, { 'content-type': 'text/plain' });
    res.end('secure-ok');
  });
  good.on('connection', () => { goodConnections += 1; });
  await new Promise((resolve) => good.listen(port, addrs[1].address, resolve));
  const { child, base } = await startHost({ NODE_EXTRA_CA_CERTS: certs.ca });
  try {
    const result = await postRequest(base, {
      url: `https://localhost:${port}/x`,
      method: 'GET',
      headers: { 'User-Agent': 'Mozilla/5.0' },
      timeoutMs: 2000,
      proxyUrl: null,
      pinnedAddrs: [],
    });
    assert.equal(result.code, 'TIMEOUT');
    assert.equal(goodConnections, 0, '超时不应轮换，好端点一次都不应被试');
  } finally {
    stopHost(child);
    for (const s of sockets) s.destroy();
    blackhole.close();
    good.close();
  }
});

test('已收到响应后不轮换（v0.0.79）', { timeout: 20000 }, async () => {
  // 首个地址 TLS 建好、响应头发了一半就断开——请求字节已发出，
  // 重发不安全，好端点一次都不应被试。
  const addrs = await resolveLocalhost();
  assert.ok(addrs.length >= 2, `localhost 应至少解析出 2 个地址，实际：${JSON.stringify(addrs)}`);
  const partial = https.createServer({ key: certs.key, cert: certs.cert }, (req, res) => {
    res.writeHead(200, { 'Content-Type': 'text/plain', 'Content-Length': '100' });
    res.write('partial', () => req.socket.destroy());
  });
  await new Promise((resolve) => partial.listen(0, addrs[0].address, resolve));
  const port = partial.address().port;
  let goodConnections = 0;
  const good = https.createServer({ key: certs.key, cert: certs.cert }, (req, res) => {
    res.writeHead(200, { 'content-type': 'text/plain' });
    res.end('secure-ok');
  });
  good.on('connection', () => { goodConnections += 1; });
  await new Promise((resolve) => good.listen(port, addrs[1].address, resolve));
  const { child, base } = await startHost({ NODE_EXTRA_CA_CERTS: certs.ca });
  try {
    const result = await postRequest(base, {
      url: `https://localhost:${port}/x`,
      method: 'GET',
      headers: { 'User-Agent': 'Mozilla/5.0' },
      timeoutMs: 10000,
      proxyUrl: null,
      pinnedAddrs: [],
    });
    assert.equal(result.code, 'NETWORK');
    assert.equal(goodConnections, 0, '已收到响应后不应轮换，好端点一次都不应被试');
  } finally {
    stopHost(child);
    partial.close();
    good.close();
  }
});

test('resolveAddrs：系统顺序、不重排、解析失败回空列表（v0.0.79）', { timeout: 20000 }, async () => {
  const script = `
    const { resolveAddrs } = require(${JSON.stringify(HOST)});
    (async () => {
      const local = await resolveAddrs('localhost');
      console.log('LOCAL ' + JSON.stringify(local));
      const ip = await resolveAddrs('127.0.0.2');
      console.log('IP ' + JSON.stringify(ip));
      const bad = await resolveAddrs('not a host');
      console.log('BAD ' + JSON.stringify(bad));
      process.exit(0);
    })();
  `;
  const out = execFileSync(process.execPath, ['-e', script], { encoding: 'utf8' });
  const lines = out.trim().split('\n');
  const line = (prefix) => lines.find((l) => l.startsWith(prefix)).slice(prefix.length);
  const local = JSON.parse(line('LOCAL '));
  assert.ok(local.length >= 2, 'localhost 应至少解析出 2 个地址');
  assert.ok(local.some((a) => a.address === '127.0.0.1' && a.family === 4));
  assert.ok(local.some((a) => a.address === '::1' && a.family === 6));
  // 顺序以系统为准，不硬编码；但两个族都要在，不应被重排丢掉。
  assert.deepEqual(JSON.parse(line('IP ')), [{ address: '127.0.0.2', family: 4 }]);
  assert.deepEqual(JSON.parse(line('BAD ')), []);
});

test('CONNECT 代理握手：请求行、认证、200 后发 TLS ClientHello', { timeout: 20000 }, async () => {
  let connectHead = null;
  let tlsFirstByte = null;
  const proxy = net.createServer((socket) => {
    let buf = Buffer.alloc(0);
    socket.on('data', (chunk) => {
      if (connectHead === null) {
        buf = Buffer.concat([buf, chunk]);
        const end = buf.indexOf('\r\n\r\n');
        if (end === -1) return;
        connectHead = buf.subarray(0, end).toString('latin1');
        socket.write('HTTP/1.1 200 Connection Established\r\n\r\n');
      } else if (tlsFirstByte === null) {
        tlsFirstByte = chunk[0];
        socket.destroy();
      }
    });
  });
  await new Promise((resolve) => proxy.listen(0, '127.0.0.1', resolve));
  const port = proxy.address().port;
  const { child, base } = await startHost();
  try {
    const result = await postRequest(base, {
      url: 'https://example.com/',
      method: 'GET',
      headers: { 'User-Agent': 'Mozilla/5.0' },
      timeoutMs: 10000,
      proxyUrl: `http://user:pass@127.0.0.1:${port}`,
      pinnedAddrs: [],
    });
    // mock 代理在 ClientHello 后就断开：上游 TLS 失败，走 500 + NETWORK。
    assert.equal(result.code, 'NETWORK');
    assert.equal(result.error !== undefined, true);
    await waitFor(() => (connectHead === null ? undefined : connectHead));
    assert.match(connectHead, /^CONNECT example\.com:443 HTTP\/1\.1\r\n/);
    assert.match(connectHead, /\r\nHost: example\.com:443\r\n/);
    assert.match(connectHead, /\r\nProxy-Authorization: Basic dXNlcjpwYXNz$/);
    await waitFor(() => (tlsFirstByte === null ? undefined : tlsFirstByte));
    assert.equal(tlsFirstByte, 0x16, 'CONNECT 200 后应发出 TLS ClientHello');
  } finally {
    stopHost(child);
    proxy.close();
  }
});

test('HTTP 目标经 HTTP 代理：请求行写绝对 URI', { timeout: 20000 }, async () => {
  let requestLine = null;
  const proxy = net.createServer((socket) => {
    let buf = Buffer.alloc(0);
    socket.on('data', (chunk) => {
      buf = Buffer.concat([buf, chunk]);
      const end = buf.indexOf('\r\n\r\n');
      if (end === -1) return;
      requestLine = buf.subarray(0, end).toString('latin1').split('\r\n')[0];
      socket.end('HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok');
    });
  });
  await new Promise((resolve) => proxy.listen(0, '127.0.0.1', resolve));
  const port = proxy.address().port;
  const { child, base } = await startHost();
  try {
    const result = await postRequest(base, {
      url: 'http://example.com/x',
      method: 'GET',
      headers: { 'User-Agent': 'Mozilla/5.0' },
      timeoutMs: 10000,
      proxyUrl: `http://127.0.0.1:${port}`,
      pinnedAddrs: [],
    });
    assert.equal(result.status, 200);
    assert.equal(Buffer.from(result.bodyBase64, 'base64').toString(), 'ok');
    await waitFor(() => (requestLine === null ? undefined : requestLine));
    assert.equal(requestLine, 'GET http://example.com/x HTTP/1.1');
  } finally {
    stopHost(child);
    proxy.close();
  }
});

test('SOCKS5h 握手：域名由代理解析，成功后发 TLS ClientHello', { timeout: 20000 }, async () => {
  const seen = {};
  const proxy = net.createServer((socket) => {
    const reader = makeReader(socket);
    (async () => {
      const greet = await reader.read(3);
      seen.greet = [...greet];
      socket.write(Buffer.from([0x05, 0x00]));
      const head = await reader.read(4);
      assert.equal(head[3], 0x03, 'socks5h 应送域名（ATYP 0x03）');
      const len = (await reader.read(1))[0];
      const domain = (await reader.read(len)).toString();
      const portBuf = await reader.read(2);
      seen.connect = { domain, port: portBuf.readUInt16BE(0) };
      socket.write(Buffer.from([0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0]));
      const next = await reader.read(1);
      seen.tlsFirstByte = next[0];
      socket.destroy();
    })().catch(() => {});
  });
  await new Promise((resolve) => proxy.listen(0, '127.0.0.1', resolve));
  const port = proxy.address().port;
  const { child, base } = await startHost();
  try {
    const result = await postRequest(base, {
      url: 'https://example.com/',
      method: 'GET',
      headers: { 'User-Agent': 'Mozilla/5.0' },
      timeoutMs: 10000,
      proxyUrl: `socks5h://127.0.0.1:${port}`,
      pinnedAddrs: ['93.184.216.34:443'],
    });
    assert.equal(result.code, 'NETWORK');
    await waitFor(() => (seen.connect === undefined ? undefined : seen.connect));
    assert.deepEqual(seen.greet, [0x05, 0x01, 0x00]);
    assert.equal(seen.connect.domain, 'example.com');
    assert.equal(seen.connect.port, 443);
    await waitFor(() => (seen.tlsFirstByte === undefined ? undefined : seen.tlsFirstByte));
    assert.equal(seen.tlsFirstByte, 0x16);
  } finally {
    stopHost(child);
    proxy.close();
  }
});

test('SOCKS5 用户名密码认证 + socks5 送固定 IP', { timeout: 20000 }, async () => {
  const seen = {};
  const proxy = net.createServer((socket) => {
    const reader = makeReader(socket);
    (async () => {
      const greet = await reader.read(4);
      seen.greet = [...greet];
      socket.write(Buffer.from([0x05, 0x02]));
      const ver = (await reader.read(1))[0];
      assert.equal(ver, 0x01);
      const ulen = (await reader.read(1))[0];
      const user = (await reader.read(ulen)).toString();
      const plen = (await reader.read(1))[0];
      const pass = (await reader.read(plen)).toString();
      seen.auth = { user, pass };
      socket.write(Buffer.from([0x01, 0x00]));
      const head = await reader.read(4);
      assert.equal(head[3], 0x01, 'socks5 应送 IP（ATYP 0x01）');
      const ip = [...(await reader.read(4))];
      const portBuf = await reader.read(2);
      seen.connect = { ip, port: portBuf.readUInt16BE(0) };
      socket.write(Buffer.from([0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0]));
      const next = await reader.read(1);
      seen.tlsFirstByte = next[0];
      socket.destroy();
    })().catch(() => {});
  });
  await new Promise((resolve) => proxy.listen(0, '127.0.0.1', resolve));
  const port = proxy.address().port;
  const { child, base } = await startHost();
  try {
    const result = await postRequest(base, {
      url: 'https://example.com/',
      method: 'GET',
      headers: { 'User-Agent': 'Mozilla/5.0' },
      timeoutMs: 10000,
      proxyUrl: `socks5://user:pass@127.0.0.1:${port}`,
      pinnedAddrs: ['93.184.216.34:443'],
    });
    assert.equal(result.code, 'NETWORK');
    await waitFor(() => (seen.connect === undefined ? undefined : seen.connect));
    assert.deepEqual(seen.greet, [0x05, 0x02, 0x00, 0x02]);
    assert.deepEqual(seen.auth, { user: 'user', pass: 'pass' });
    assert.deepEqual(seen.connect.ip, [93, 184, 216, 34]);
    assert.equal(seen.connect.port, 443);
    await waitFor(() => (seen.tlsFirstByte === undefined ? undefined : seen.tlsFirstByte));
    assert.equal(seen.tlsFirstByte, 0x16);
  } finally {
    stopHost(child);
    proxy.close();
  }
});

test('SOCKS4 握手：送固定 IP 与用户名', { timeout: 20000 }, async () => {
  const seen = {};
  const proxy = net.createServer((socket) => {
    const reader = makeReader(socket);
    (async () => {
      const head = await reader.read(8);
      assert.equal(head[0], 0x04);
      assert.equal(head[1], 0x01);
      const port = head.subarray(2, 4).readUInt16BE(0);
      const ip = [...head.subarray(4, 8)];
      // 用户名以 NUL 结尾：逐字节读到 0x00。
      let user = '';
      for (;;) {
        const b = (await reader.read(1))[0];
        if (b === 0x00) break;
        user += String.fromCharCode(b);
      }
      seen.request = { port, ip, user };
      socket.write(Buffer.from([0x00, 0x5a, 0, 0, 0, 0, 0, 0]));
      const next = await reader.read(1);
      seen.tlsFirstByte = next[0];
      socket.destroy();
    })().catch(() => {});
  });
  await new Promise((resolve) => proxy.listen(0, '127.0.0.1', resolve));
  const port = proxy.address().port;
  const { child, base } = await startHost();
  try {
    const result = await postRequest(base, {
      url: 'https://example.com/',
      method: 'GET',
      headers: { 'User-Agent': 'Mozilla/5.0' },
      timeoutMs: 10000,
      proxyUrl: `socks4://socksuser@127.0.0.1:${port}`,
      pinnedAddrs: ['93.184.216.34:443'],
    });
    assert.equal(result.code, 'NETWORK');
    await waitFor(() => (seen.request === undefined ? undefined : seen.request));
    assert.deepEqual(seen.request, { port: 443, ip: [93, 184, 216, 34], user: 'socksuser' });
    await waitFor(() => (seen.tlsFirstByte === undefined ? undefined : seen.tlsFirstByte));
    assert.equal(seen.tlsFirstByte, 0x16);
  } finally {
    stopHost(child);
    proxy.close();
  }
});
