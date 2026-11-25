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
// v0.0.80：DoH 取全量边缘地址 + 轮换（系统列表不够用时才查，
// mock DoH 端点由 HG_DOH_BASE_URL 注入；DoH 答案过放行校验，
// 127/8 全拒，所以轮换用例用本机可绑的非保留 IP 做“好地址”，
// 见下面 findUsableIp）；网络诊断探针（POST /diagnose，
// port 参数只给测试指临时端口，生产永远 443）。
// v0.0.82：/diagnose 除报告文本外再回域名小结 summary（Rust 侧
// 拼结论行用），另有无可用地址分支的小结全零用例——沙箱解析器
// 给任何合法主机名都造 FakeIP，该用例用非法主机名 'a..b' 触发。
// 轮换用例在测试进程内直接调 doFetch（不在 spawned 边车子进程
// 里跑）：沙箱会拦截跨进程发往非 loopback 地址的 TCP，子进程
// 连不上 DoH 给的非保留 IP；且沙箱拦截一切发往非 loopback 的
// TCP（同进程、即使本机有监听也一样，直连公网 IP 则黑洞），
// 所以 DoH 的“好地址”在本机永远不可达——“补充地址→成功”用
// 例显式跳过，成功路径由 v0.0.80 真机验收验证；可测的部分是
// DoH 查询、过滤、轮换尝试（全失败用例的后缀证明 6 个地址都
// 被试了）、_doh 字段与 60 秒缓存（缓存用例用两次失败请求，
// 不依赖好服务器）。doFetch 因此导出（见 host.js 末尾注释）。
// 懒加载 getHost() 必须在“默认端口被占时落临时端口”用例之后第
// 一次调用——require host.js 会在测试进程内占 55101。
// DoH 缓存按主机名记在模块级内存里：轮换用例各用一个主机名
// （doh-allfail/doh-cache.test），避免缓存串用例。
//
// 跑：node --test src-tauri/node-sidecar/tests/test_host.mjs
import { test, before, after } from 'node:test';
import assert from 'node:assert/strict';
import { spawn, execFileSync } from 'node:child_process';
import { createRequire } from 'node:module';
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

// 懒加载 host.js：require 会在测试进程内起一个边车服务器（占
// 55101 或自愈到随机端口；无 HG_NODE_INFO_PATH 时不写 info 文件）。
// 必须在“默认端口被占时落临时端口”用例跑完之后才第一次调用——
// 那用例要占 55101，若测试进程已占住它，occupier.listen 永远不
// 回调。DoH 进程内用例定义在它之后，node:test 按定义顺序串行跑，
// 所以第一次调用一定在它之后。只供 DoH 轮换用例调 doFetch。
const requireHost = createRequire(import.meta.url);
let hostMod = null;
function getHost() {
  if (!hostMod) hostMod = requireHost(HOST);
  return hostMod;
}

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

async function postDiagnose(base, body) {
  const r = await fetch(`${base}/diagnose`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body),
  });
  return r.json();
}

// mock DoH 端点（v0.0.80）：按 type 回 A/AAAA 记录，记调用次数。
// failStatus 非 0 时回该 HTTP 状态（测 DoH 端点不可用/失败）。
function startMockDoh({ a = [], aaaa = [], status = 0, failStatus = 0 } = {}) {
  const hits = { total: 0, a: 0, aaaa: 0 };
  const server = http.createServer((req, res) => {
    const u = new URL(req.url, 'http://127.0.0.1');
    if (u.pathname !== '/resolve') { res.writeHead(404); res.end(); return; }
    hits.total += 1;
    const type = u.searchParams.get('type');
    if (type === 'A') hits.a += 1; else if (type === 'AAAA') hits.aaaa += 1;
    if (failStatus) { res.writeHead(failStatus); res.end(); return; }
    const ips = type === 'A' ? a : aaaa;
    const answer = ips.map((ip) => ({ name: 'example.test.', type: type === 'A' ? 1 : 28, TTL: 60, data: ip }));
    res.writeHead(200, { 'content-type': 'application/dns-json' });
    res.end(JSON.stringify({ Status: status, Answer: answer }));
  });
  return new Promise((resolve) => server.listen(0, '127.0.0.1', () => resolve({ server, hits, port: server.address().port })));
}

// mock 出口 IP 端点（v0.0.80）：/cdn-cgi/trace 回 trace 文本，
// /ip 回裸 IP；traceStatus 非 200 时 trace 端点回该状态（测回落）。
function startMockEgress({ traceIp = '1.2.3.4', colo = 'HKG', ip = '5.6.7.8', traceStatus = 200 } = {}) {
  const server = http.createServer((req, res) => {
    if (req.url === '/cdn-cgi/trace') {
      if (traceStatus !== 200) { res.writeHead(traceStatus); res.end(); return; }
      res.writeHead(200, { 'content-type': 'text/plain' });
      res.end(`fl=1\nip=${traceIp}\nloc=CN\ncolo=${colo}\n`);
      return;
    }
    if (req.url === '/ip') {
      res.writeHead(200, { 'content-type': 'text/plain' });
      res.end(ip);
      return;
    }
    res.writeHead(404); res.end();
  });
  return new Promise((resolve) => server.listen(0, '127.0.0.1', () => resolve({ server, port: server.address().port })));
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

after(async () => {
  // 进程内 require 过 host.js 时（DoH 轮换用例），先关掉它起的
  // 监听服务器，否则测试进程退不出、node --test 一直等。
  if (hostMod) await hostMod.closeServer();
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
    // v0.0.80：系统列表耗尽后会查一次 DoH（无可用地址），后缀如
    // 实写明 DoH 无结果。
    assert.match(result.error, /已试: 127\.0\.0\.2；DoH 无结果；仅 1 个地址，无处可换/);
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
      result.error.includes(`（已试: ${addrs[0].address}, ${addrs[1].address}；DoH 无结果；共 2 个地址）`),
      `报错应带全部已试地址、DoH 无结果与解析总数：${result.error}`,
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

// ---------------------------------------------------------------------------
// v0.0.80：DoH 取全量边缘地址 + 轮换、网络诊断探针。
//
// DoH 端到端用例需要一个“过放行校验且本机可绑”的 IP（DoH 答案里的
// 内网/保留地址会被滤掉，127/8 全拒）：运行时从本机网卡找一个
// 非内网 IPv4（本机是 198.19.0.2，属放行的 FakeIP 段）。找不到时
// 相关用例跳过（环境限制，不是代码问题）；其余 DoH 用例（失败
// 退回、缓存、私网过滤、isAllowedIp 单元）不依赖它。
// ---------------------------------------------------------------------------
function findUsableIp() {
  for (const addrs of Object.values(os.networkInterfaces())) {
    for (const a of addrs || []) {
      // Node 18.4+ 的 family 是字符串 'IPv4'，老版本是数字 4。
      if ((a.family !== 4 && a.family !== 'IPv4') || a.internal) continue;
      const [o1, o2, o3] = a.address.split('.').map(Number);
      if (o1 === 10 || o1 === 127 || o1 >= 224) continue;
      if (o1 === 100 && o2 >= 64 && o2 <= 127) continue;
      if (o1 === 169 && o2 === 254) continue;
      if (o1 === 172 && o2 >= 16 && o2 <= 31) continue;
      if (o1 === 192 && o2 === 168) continue;
      if (o1 === 192 && o2 === 0 && (o3 === 0 || o3 === 2)) continue;
      if (o1 === 192 && o2 === 88 && o3 === 99) continue;
      if (o1 === 198 && o2 === 51 && o3 === 100) continue;
      if (o1 === 203 && o2 === 0 && o3 === 113) continue;
      return a.address;
    }
  }
  return null;
}
const usableIp = findUsableIp();
if (!usableIp) console.log('注意：本机无可用非保留 IP，DoH 轮换端到端用例将跳过');

test('DoH 补充地址：系统 1 个坏地址 + DoH 回 5 个、其中 1 个好 → 成功（v0.0.80）', {
  // 本机不可测，显式跳过：沙箱拦截一切发往非 loopback 地址的
  // TCP（同进程、即使本机有监听也一样；直连公网 IP 则黑洞），
  // DoH 给的非保留地址永远到不了本地“好服务器”——明文 HTTP
  // 直连 198.19.0.2 也被网关截获（Parse Error），TLS 更不用说。
  // 而 loopback 地址又过不了 isAllowedIp 放行校验，两者无交集。
  // 成功路径（DoH 地址通 → 200）由 v0.0.80 真机验收验证：
  // 直连模式玩偶分类能出来即证明轮换成功。可测的部分（DoH 查
  // 询、过滤、轮换尝试、后缀、缓存）由其余用例覆盖。
  skip: '本机沙箱拦截非 loopback TCP，DoH 好地址不可达；成功路径由真机验收验证',
}, () => {});

test('DoH 全部失败时报错列出系统与 DoH 两组地址及总数（v0.0.80）', { timeout: 30000, skip: !usableIp }, async () => {
  // 进程内调 doFetch。系统 1 个坏地址 + DoH 5 个坏地址（首个是
  // 本机 IP 上的坏 TLS 端点，其余 4 个是真实 Cloudflare 边缘
  // IP——本机沙箱会拦截发往它们的 TCP 并回明文拒绝或 RST，TLS
  // 报 wrong version number 或被断，同样是无字节发出的 NETWORK
  // 失败，轮换会一路试完）：报错应是首个错误，附两组已试地址与总数。
  // 不断言 DoH 坏端点的 clientHellos：沙箱在 TLS 层拦截，ClientHello
  // 到不了本机坏服务器；后缀已证明该地址被试过。
  const { server: bad1, hits: hits1 } = await startBadTlsServer('127.0.0.2', 0);
  const port = bad1.address().port;
  const { server: bad2 } = await startBadTlsServer(usableIp, port);
  const doh = await startMockDoh({ a: [usableIp, '104.17.255.47', '172.66.199.198', '104.20.56.211', '172.66.194.190'] });
  const prevDoh = process.env.HG_DOH_BASE_URL;
  process.env.HG_DOH_BASE_URL = `http://127.0.0.1:${doh.port}`;
  try {
    let err = null;
    try {
      await getHost().doFetch({
        url: `https://doh-allfail.test:${port}/x`,
        method: 'GET',
        headers: { 'User-Agent': 'Mozilla/5.0' },
        timeoutMs: 10000,
        proxyUrl: null,
        pinnedAddrs: [`127.0.0.2:${port}`],
      });
    } catch (e) { err = e; }
    assert.ok(err, '应抛出错误');
    assert.equal(err.code, 'NETWORK');
    assert.match(err.message, /Client network socket disconnected before secure TLS connection was established/);
    assert.ok(
      err.message.includes(`（已试: 127.0.0.2；DoH: ${usableIp}, 104.17.255.47, 172.66.199.198, 104.20.56.211, 172.66.194.190；共 6 个地址）`),
      `报错应带系统与 DoH 两组地址及总数：${err.message}`,
    );
    assert.equal(err._doh, 'doh-allfail.test：补充 5 个地址');
    assert.ok(hits1.clientHellos >= 1, '系统坏端点应被试过');
  } finally {
    if (prevDoh === undefined) delete process.env.HG_DOH_BASE_URL;
    else process.env.HG_DOH_BASE_URL = prevDoh;
    bad1.close();
    bad2.close();
    doh.server.close();
  }
});

test('DoH 端点不可达时静默退回系统错误（v0.0.80）', { timeout: 20000 }, async () => {
  // DoH 端点直接连不上：不报错、不阻塞，报错仍是系统那 1 个地址
  // 的 TLS 失败，后缀写明 DoH 无结果。
  const dead = await new Promise((resolve) => {
    const s = http.createServer();
    s.listen(0, '127.0.0.1', () => { const port = s.address().port; s.close(() => resolve(port)); });
  });
  const { server: bad, hits } = await startBadTlsServer('127.0.0.2', 0);
  const port = bad.address().port;
  const { child, base } = await startHost({ HG_DOH_BASE_URL: `http://127.0.0.1:${dead}` });
  try {
    const result = await postRequest(base, {
      url: `https://myhost.test:${port}/x`,
      method: 'GET',
      headers: { 'User-Agent': 'Mozilla/5.0' },
      timeoutMs: 10000,
      proxyUrl: null,
      pinnedAddrs: [`127.0.0.2:${port}`],
    });
    assert.equal(result.code, 'NETWORK');
    assert.match(result.error, /Client network socket disconnected before secure TLS connection was established/);
    assert.match(result.error, /已试: 127\.0\.0\.2；DoH 无结果；仅 1 个地址，无处可换/);
    assert.equal(result.doh, 'myhost.test：查询失败');
    assert.equal(hits.clientHellos, 1, '系统坏端点只应被试一次');
  } finally {
    stopHost(child);
    bad.close();
  }
});

test('同一主机 60 秒内第二次请求不重复查 DoH（v0.0.80）', { timeout: 30000, skip: !usableIp }, async () => {
  // 进程内调 doFetch。两次请求同一主机（都失败——好地址在本机
  // 不可达，见成功用例的跳过原因）：第一次查 DoH（A+AAAA 各
  // 1 次），第二次走内存缓存，不再发 DoH 查询；两次报错都应带
  // 同样的 _doh，证明第二次确实用了缓存里的地址轮换。
  const { server: bad } = await startBadTlsServer('127.0.0.2', 0);
  const port = bad.address().port;
  const doh = await startMockDoh({ a: [usableIp, '104.17.255.47', '172.66.199.198', '104.20.56.211', '172.66.194.190'] });
  const prevDoh = process.env.HG_DOH_BASE_URL;
  process.env.HG_DOH_BASE_URL = `http://127.0.0.1:${doh.port}`;
  const body = {
    url: `https://doh-cache.test:${port}/x`,
    method: 'GET',
    headers: { 'User-Agent': 'Mozilla/5.0' },
    timeoutMs: 10000,
    proxyUrl: null,
    pinnedAddrs: [`127.0.0.2:${port}`],
  };
  const runOnce = async () => {
    try {
      await getHost().doFetch(body);
      return null;
    } catch (e) { return e; }
  };
  try {
    const first = await runOnce();
    assert.ok(first, '第一次请求应失败（好地址不可达）');
    assert.equal(first.code, 'NETWORK');
    assert.equal(first._doh, 'doh-cache.test：补充 5 个地址');
    const second = await runOnce();
    assert.ok(second, '第二次请求也应失败');
    assert.equal(second.code, 'NETWORK');
    assert.equal(second._doh, 'doh-cache.test：补充 5 个地址');
    assert.equal(doh.hits.a, 1, '第二次请求不应重复查 DoH（A）');
    assert.equal(doh.hits.aaaa, 1, '第二次请求不应重复查 DoH（AAAA）');
  } finally {
    if (prevDoh === undefined) delete process.env.HG_DOH_BASE_URL;
    else process.env.HG_DOH_BASE_URL = prevDoh;
    bad.close();
    doh.server.close();
  }
});

test('DoH 回内网地址时被滤掉不试（v0.0.80）', { timeout: 20000 }, async () => {
  // DoH 只回内网地址：全部被放行校验滤掉，报错里不出现它们，
  // 后缀写明 DoH 无结果。
  const { server: bad } = await startBadTlsServer('127.0.0.2', 0);
  const port = bad.address().port;
  const doh = await startMockDoh({ a: ['10.0.0.1', '192.168.1.1'] });
  const { child, base } = await startHost({ HG_DOH_BASE_URL: `http://127.0.0.1:${doh.port}` });
  try {
    const result = await postRequest(base, {
      url: `https://myhost.test:${port}/x`,
      method: 'GET',
      headers: { 'User-Agent': 'Mozilla/5.0' },
      timeoutMs: 10000,
      proxyUrl: null,
      pinnedAddrs: [`127.0.0.2:${port}`],
    });
    assert.equal(result.code, 'NETWORK');
    assert.match(result.error, /DoH 无结果/);
    assert.ok(!result.error.includes('10.0.0.1'), '内网地址不应被试、不应出现在报错里');
    assert.ok(!result.error.includes('192.168.1.1'), '内网地址不应被试、不应出现在报错里');
    assert.equal(result.doh, 'myhost.test：无可用地址');
  } finally {
    stopHost(child);
    bad.close();
    doh.server.close();
  }
});

test('isAllowedIp：与 Rust allowed_ip 同款放行校验（v0.0.80）', () => {
  const script = `
    const { isAllowedIp } = require(${JSON.stringify(HOST)});
    const cases = [
      ['104.17.255.47', true], ['172.66.199.198', true], ['8.8.8.8', true],
      ['198.18.25.104', true], ['198.19.0.1', true],
      ['10.0.0.1', false], ['127.0.0.1', false], ['192.168.1.1', false],
      ['172.16.0.1', false], ['172.31.255.255', false], ['172.32.0.1', true],
      ['100.64.0.1', false], ['100.127.255.255', false], ['100.128.0.1', true],
      ['169.254.1.1', false], ['0.0.0.0', false], ['224.0.0.1', false],
      ['192.0.0.1', false], ['192.0.2.1', false], ['192.0.1.1', true],
      ['192.88.99.1', false], ['198.51.100.1', false], ['203.0.113.1', false],
      ['2606:4700:4700::1111', true], ['::1', false], ['fe80::1', false],
      ['2001:db8::1', false], ['2002::1', false], ['3fff::1', false],
      ['::ffff:104.17.255.47', true], ['::ffff:10.0.0.1', false],
      ['not-an-ip', false],
    ];
    for (const [ip, want] of cases) {
      const got = isAllowedIp(ip);
      if (got !== want) { console.error('FAIL ' + ip + ' got ' + got + ' want ' + want); process.exit(1); }
    }
    console.log('OK');
    process.exit(0);
  `;
  const out = execFileSync(process.execPath, ['-e', script], { encoding: 'utf8' });
  // require host.js 会先打一行启动日志（Node sidecar starting…），
  // 只看最后一行。
  assert.equal(out.trim().split('\n').pop(), 'OK');
});

test('probeTls：TCP 不通时报 refused（v0.0.80）', () => {
  // 探针 TCP/TLS 区分的最小单元：127.0.0.1:443 本机无监听，应是
  // TCP 不通（connect ECONNREFUSED），而不是 TLS 失败。
  const script = `
    const { probeTls } = require(${JSON.stringify(HOST)});
    (async () => {
      const r = await probeTls('example.test', '127.0.0.1', 5000);
      console.log(JSON.stringify(r));
      process.exit(0);
    })();
  `;
  const out = execFileSync(process.execPath, ['-e', script], { encoding: 'utf8' });
  // require host.js 会先打一行启动日志，只解析最后一行 JSON。
  const r = JSON.parse(out.trim().split('\n').pop());
  assert.equal(r.tcpOk, false);
  assert.equal(r.tlsOk, false);
  assert.match(r.error, /ECONNREFUSED/);
});

test('网络诊断探针：两组 DNS 地址、握手区分、出口 IP（v0.0.80）', { timeout: 30000, skip: !usableIp }, async () => {
  // localhost 系统解析 2 个地址：坏 TLS 端点放第一个、好端点放
  // 第二个（同一端口）；DoH 回本机可用 IP（过放行校验；该端口无
  // 监听，应报 TCP 不通——三种握手结果一次齐）；出口走 mock trace 端点。
  const addrs = await resolveLocalhost();
  assert.ok(addrs.length >= 2, `localhost 应至少解析出 2 个地址，实际：${JSON.stringify(addrs)}`);
  const { server: bad, hits } = await startBadTlsServer(addrs[0].address, 0);
  const port = bad.address().port;
  const good = https.createServer({ key: certs.key, cert: certs.cert }, (req, res) => {
    res.writeHead(200, { 'content-type': 'text/plain' });
    res.end('secure-ok');
  });
  await new Promise((resolve) => good.listen(port, addrs[1].address, resolve));
  const doh = await startMockDoh({ a: [usableIp] });
  const egress = await startMockEgress({});
  const { child, base } = await startHost({ NODE_EXTRA_CA_CERTS: certs.ca, HG_DOH_BASE_URL: `http://127.0.0.1:${doh.port}` });
  try {
    const result = await postDiagnose(base, {
      domains: ['localhost'],
      proxyUrl: null,
      proxyDesc: '直连',
      port,
      egressUrls: [`http://127.0.0.1:${egress.port}/cdn-cgi/trace`, `http://127.0.0.1:${egress.port}/ip`],
    });
    assert.ok(result.report, `应返回报告：${JSON.stringify(result)}`);
    const lines = result.report.split('\n');
    assert.match(lines[0], /^网络诊断 .*（代理：直连）$/);
    assert.ok(
      result.report.includes(`localhost：系统 2 个 [${addrs[0].address}, ${addrs[1].address}]；DoH 1 个 [${usableIp}]`),
      `DNS 对照行应列出系统与 DoH 两组地址：${result.report}`,
    );
    assert.ok(
      result.report.includes(`${addrs[0].address}：TCP 通，TLS 失败（Client network socket disconnected before secure TLS connection was established，`),
      `坏端点应报 TLS 失败：${result.report}`,
    );
    assert.match(result.report, new RegExp(`${addrs[1].address.replace(/\./g, '\\.')}：TCP 通，TLS 通\\（TLSv1\\.3[^）]*，ALPN 无 ALPN，\\d+ms\\）；HTTP 200，非挑战页`));
    // DoH 给的地址在真机上应报 TCP 不通（无监听）；但本机沙箱会
    // 拦截发往非 loopback 的 TCP、替它回一段明文，于是报成
    // “TCP 通，TLS 失败（wrong version number）”。这里不断言它
    // 的具体结果，只断言这一行存在——TCP 不通分支由 probeTls
    // 单元测试覆盖。
    assert.ok(
      result.report.includes(`${usableIp}：`),
      `DoH 地址应有一行握手结果：${result.report}`,
    );
    assert.ok(result.report.includes('出口 IP：1.2.3.4（colo=HKG，cloudflare trace）'), `出口 IP 应来自 trace：${result.report}`);
    assert.equal(lines.length, 6, `报告应为 6 行（标题 + DNS + 3 握手 + 出口），实际：${result.report}`);
    assert.ok(hits.clientHellos >= 1, '坏端点应被试过');
  } finally {
    stopHost(child);
    bad.close();
    good.close();
    doh.server.close();
    egress.server.close();
  }
});

test('网络诊断探针：出口 trace 不可用时回落到备用地址（v0.0.80）', { timeout: 30000 }, async () => {
  const egress = await startMockEgress({ traceStatus: 500, ip: '5.6.7.8' });
  const { child, base } = await startHost();
  try {
    const result = await postDiagnose(base, {
      domains: [],
      proxyUrl: null,
      proxyDesc: '直连',
      egressUrls: [`http://127.0.0.1:${egress.port}/cdn-cgi/trace`, `http://127.0.0.1:${egress.port}/ip`],
    });
    assert.ok(result.report, `应返回报告：${JSON.stringify(result)}`);
    assert.ok(result.report.includes('出口 IP：5.6.7.8（127.0.0.1）'), `trace 失败时应回落到备用地址：${result.report}`);
    assert.ok(!result.report.includes('未知'), `回落后不应写未知：${result.report}`);
  } finally {
    stopHost(child);
    egress.server.close();
  }
});

test('网络诊断探针：握手通的边缘返回挑战页时标出（v0.0.80）', { timeout: 30000 }, async () => {
  // 好端点返回 Cloudflare 挑战页：HTTP 行应标“挑战页”，而不是
  // “非挑战页”。::1 上无监听，应报 TCP 不通。
  const addrs = await resolveLocalhost();
  assert.ok(addrs.length >= 2);
  const good = https.createServer({ key: certs.key, cert: certs.cert }, (req, res) => {
    res.writeHead(403, { 'content-type': 'text/html' });
    res.end('<html><head><title>Just a moment...</title></head><body>cf-browser-verification</body></html>');
  });
  await new Promise((resolve) => good.listen(0, '127.0.0.1', resolve));
  const port = good.address().port;
  const doh = await startMockDoh({});
  const egress = await startMockEgress({});
  const { child, base } = await startHost({ NODE_EXTRA_CA_CERTS: certs.ca, HG_DOH_BASE_URL: `http://127.0.0.1:${doh.port}` });
  try {
    const result = await postDiagnose(base, {
      domains: ['localhost'],
      proxyUrl: null,
      proxyDesc: '直连',
      port,
      egressUrls: [`http://127.0.0.1:${egress.port}/cdn-cgi/trace`],
    });
    assert.ok(result.report, `应返回报告：${JSON.stringify(result)}`);
    assert.ok(result.report.includes('HTTP 403，挑战页'), `挑战页应被标出：${result.report}`);
    assert.ok(result.report.includes('TCP 不通'), `无监听的地址应报 TCP 不通：${result.report}`);
  } finally {
    stopHost(child);
    good.close();
    doh.server.close();
    egress.server.close();
  }
});

test('网络诊断探针：返回域名小结（v0.0.82）', { timeout: 30000, skip: !usableIp }, async () => {
  // 与“两组 DNS 地址”用例同一套布景（坏 TLS + 好 HTTPS + mock
  // DoH + mock 出口）：除报告文本外，/diagnose 还应回 summary，
  // Rust 侧拼结论行用。edges 是去重后的边缘数（系统 2 + DoH 1，
  // 无重叠 = 3）；tlsFail 只计 TCP 通而 TLS 失败的边缘——DoH 给
  // 的地址在本机沙箱里可能报 TCP 不通（不计入）或 TCP 通/TLS 失
  // 败（计入），所以只断言 ≥1，不断死数。
  const addrs = await resolveLocalhost();
  assert.ok(addrs.length >= 2, `localhost 应至少解析出 2 个地址，实际：${JSON.stringify(addrs)}`);
  const { server: bad } = await startBadTlsServer(addrs[0].address, 0);
  const port = bad.address().port;
  const good = https.createServer({ key: certs.key, cert: certs.cert }, (req, res) => {
    res.writeHead(200, { 'content-type': 'text/plain' });
    res.end('secure-ok');
  });
  await new Promise((resolve) => good.listen(port, addrs[1].address, resolve));
  const doh = await startMockDoh({ a: [usableIp] });
  const egress = await startMockEgress({});
  const { child, base } = await startHost({ NODE_EXTRA_CA_CERTS: certs.ca, HG_DOH_BASE_URL: `http://127.0.0.1:${doh.port}` });
  try {
    const result = await postDiagnose(base, {
      domains: ['localhost'],
      proxyUrl: null,
      proxyDesc: '直连',
      port,
      egressUrls: [`http://127.0.0.1:${egress.port}/cdn-cgi/trace`, `http://127.0.0.1:${egress.port}/ip`],
    });
    assert.ok(result.report, `应返回报告：${JSON.stringify(result)}`);
    assert.ok(result.summary, `应返回小结：${JSON.stringify(result)}`);
    assert.equal(result.summary.domains.length, 1);
    const d = result.summary.domains[0];
    assert.equal(d.host, 'localhost');
    assert.equal(d.sys, 2, `系统应解析出 2 个地址：${JSON.stringify(d)}`);
    assert.equal(d.doh, 1, `DoH 应回 1 个地址：${JSON.stringify(d)}`);
    assert.equal(d.dohState, 'ok');
    assert.equal(d.edges, 3, `去重后应为 3 条边缘：${JSON.stringify(d)}`);
    assert.equal(d.tlsOk, 1, `好端点应 TLS 通：${JSON.stringify(d)}`);
    assert.ok(d.tlsFail >= 1, `坏端点应 TLS 失败：${JSON.stringify(d)}`);
    assert.equal(d.httpOk, 1, `好端点应 HTTP 200：${JSON.stringify(d)}`);
  } finally {
    stopHost(child);
    bad.close();
    good.close();
    doh.server.close();
    egress.server.close();
  }
});

test('网络诊断探针：无可用地址时小结全零、报告写未做握手（v0.0.82）', { timeout: 30000 }, async () => {
  // 沙箱解析器给任何合法主机名都造一个 FakeIP，走不到无地址分支；
  // 只有非法主机名（如 'a..b'，双点）才回 ENOTFOUND——用它走到该
  // 分支。真机上该分支由真正无记录的域名触发。
  const doh = await startMockDoh({});
  const egress = await startMockEgress({});
  const { child, base } = await startHost({ HG_DOH_BASE_URL: `http://127.0.0.1:${doh.port}` });
  try {
    const result = await postDiagnose(base, {
      domains: ['a..b'],
      proxyUrl: null,
      proxyDesc: '直连',
      egressUrls: [`http://127.0.0.1:${egress.port}/cdn-cgi/trace`],
    });
    assert.ok(result.report, `应返回报告：${JSON.stringify(result)}`);
    assert.ok(result.report.includes('a..b：系统 0 个 []'), `DNS 行应写系统 0 个：${result.report}`);
    assert.ok(result.report.includes('DoH 0 个 []（无记录）'), `DNS 行应写 DoH 无记录：${result.report}`);
    assert.ok(result.report.includes('无可用地址，未做握手'), `无地址时应写未做握手：${result.report}`);
    assert.ok(result.summary, `应返回小结：${JSON.stringify(result)}`);
    assert.equal(result.summary.domains.length, 1);
    assert.deepEqual(result.summary.domains[0], {
      host: 'a..b', sys: 0, doh: 0, dohState: 'empty', edges: 0, tlsOk: 0, tlsFail: 0, httpOk: 0,
    });
  } finally {
    stopHost(child);
    doh.server.close();
    egress.server.close();
  }
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

// ---------------------------------------------------------------------------
// 猫源 /cat/*（见 docs/developers/cat-source.md §5）：边车加载用户 JS
// 模块、调 start(config)、探活端口、回站点列表。mock 猫源模块是
// CommonJS，导出 {start, stop}；start 里用 http 起服务监听
// process.env.DEV_HTTP_PORT，GET /test 回 200、GET /config 回站点
// 列表；stop 关服务。start 收到的 config 与当时的环境变量写进同
// 目录的 env.json（供测试断言 start({}) 与 NODE_PATH/端口）；stop
// 被调过写 stopped.json。
// ---------------------------------------------------------------------------
async function postCat(base, pathname, body) {
  const r = await fetch(`${base}${pathname}`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body),
  });
  return { status: r.status, json: await r.json() };
}

async function getCatList(base) {
  const r = await fetch(`${base}/cat/list`);
  return r.json();
}

function writeMockCat(dir, siteKey, siteName) {
  fs.mkdirSync(dir, { recursive: true });
  const code = `
    'use strict';
    const http = require('http');
    const fs = require('fs');
    const path = require('path');
    let server = null;
    async function start(config) {
      fs.writeFileSync(path.join(__dirname, 'env.json'), JSON.stringify({
        config,
        devHttpPort: process.env.DEV_HTTP_PORT,
        nodePath: process.env.NODE_PATH,
      }));
      const port = Number(process.env.DEV_HTTP_PORT || 9988);
      server = http.createServer((req, res) => {
        if (req.url === '/test') { res.writeHead(200); res.end('ok'); return; }
        if (req.url === '/config') {
          res.writeHead(200, { 'content-type': 'application/json' });
          res.end(JSON.stringify({ video: { sites: [{ key: ${JSON.stringify(siteKey)}, name: ${JSON.stringify(siteName)}, type: 3, api: '/spider/${siteKey}/3' }] } }));
          return;
        }
        res.writeHead(404); res.end();
      });
      await new Promise((resolve) => server.listen(port, '127.0.0.1', resolve));
    }
    async function stop() {
      fs.writeFileSync(path.join(__dirname, 'stopped.json'), JSON.stringify({ stopped: true }));
      if (!server) return;
      await new Promise((resolve) => server.close(() => resolve()));
      server = null;
    }
    module.exports = { start, stop };
  `;
  fs.writeFileSync(path.join(dir, 'plugin.js'), code);
  return path.join(dir, 'plugin.js');
}

test('猫源 /cat/load：加载成功回 port/baseUrl/sites，/cat/list 可见', { timeout: 20000 }, async () => {
  const dir = path.join(TMP, 'cat-a');
  const file = writeMockCat(dir, 'site-a', '站点 A');
  const { child, base } = await startHost();
  try {
    const { status, json } = await postCat(base, '/cat/load', { key: 'cat-a', file, configFile: null });
    assert.equal(status, 200);
    assert.equal(json.success, true);
    assert.ok(Number.isInteger(json.port) && json.port > 0);
    assert.equal(json.baseUrl, `http://127.0.0.1:${json.port}`);
    assert.deepEqual(json.sites, [{ key: 'site-a', name: '站点 A', type: 3, api: '/spider/site-a/3' }]);
    // start 收到的环境：端口与数据目录（plugin.js 所在目录下的 data）。
    const env = JSON.parse(fs.readFileSync(path.join(dir, 'env.json'), 'utf8'));
    assert.equal(Number(env.devHttpPort), json.port);
    assert.equal(env.nodePath, path.join(dir, 'data'));
    assert.ok(fs.existsSync(path.join(dir, 'data')), '边车应创建数据目录');
    // /test 探活过的服务确实在跑。
    const probe = await fetch(`${json.baseUrl}/test`);
    assert.equal(probe.status, 200);
    const list = await getCatList(base);
    assert.deepEqual(list.sources, ['cat-a']);
  } finally {
    stopHost(child);
  }
});

test('猫源 /cat/load：同 key 重新加载覆盖旧源', { timeout: 20000 }, async () => {
  const dirA = path.join(TMP, 'cat-a2');
  const fileA = writeMockCat(dirA, 'site-a', '站点 A');
  const dirB = path.join(TMP, 'cat-b2');
  const fileB = writeMockCat(dirB, 'site-b', '站点 B');
  const { child, base } = await startHost();
  try {
    const first = await postCat(base, '/cat/load', { key: 'dup', file: fileA, configFile: null });
    assert.equal(first.json.success, true);
    const second = await postCat(base, '/cat/load', { key: 'dup', file: fileB, configFile: null });
    assert.equal(second.json.success, true);
    assert.deepEqual(second.json.sites, [{ key: 'site-b', name: '站点 B', type: 3, api: '/spider/site-b/3' }]);
    // 旧源的 stop 被调过（写了 stopped.json），list 里 key 只出现一次。
    assert.ok(fs.existsSync(path.join(dirA, 'stopped.json')), '重新加载时应先停掉旧源');
    const list = await getCatList(base);
    assert.deepEqual(list.sources, ['dup']);
  } finally {
    stopHost(child);
  }
});

test('猫源 /cat/load：文件不存在报明错', { timeout: 20000 }, async () => {
  const { child, base } = await startHost();
  try {
    const { status, json } = await postCat(base, '/cat/load', { key: 'nope', file: path.join(TMP, 'no-such', 'plugin.js'), configFile: null });
    assert.equal(status, 500);
    assert.equal(json.success, false);
    assert.match(json.error, /猫源文件不存在/);
    const list = await getCatList(base);
    assert.deepEqual(list.sources, []);
  } finally {
    stopHost(child);
  }
});

test('猫源 /cat/unload：卸载后 list 里没有，重复卸载也成功', { timeout: 20000 }, async () => {
  const dir = path.join(TMP, 'cat-c');
  const file = writeMockCat(dir, 'site-c', '站点 C');
  const { child, base } = await startHost();
  try {
    const loaded = await postCat(base, '/cat/load', { key: 'cat-c', file, configFile: null });
    assert.equal(loaded.json.success, true);
    const un = await postCat(base, '/cat/unload', { key: 'cat-c' });
    assert.equal(un.status, 200);
    assert.equal(un.json.success, true);
    assert.ok(fs.existsSync(path.join(dir, 'stopped.json')), '卸载应调 stop()');
    const list = await getCatList(base);
    assert.deepEqual(list.sources, []);
    // key 不存在时卸载也静默成功。
    const un2 = await postCat(base, '/cat/unload', { key: 'cat-c' });
    assert.equal(un2.status, 200);
    assert.equal(un2.json.success, true);
  } finally {
    stopHost(child);
  }
});

test('猫源 /cat/load：configFile 不存在时 start({}) 不报错', { timeout: 20000 }, async () => {
  const dir = path.join(TMP, 'cat-d');
  const file = writeMockCat(dir, 'site-d', '站点 D');
  const { child, base } = await startHost();
  try {
    const { status, json } = await postCat(base, '/cat/load', {
      key: 'cat-d', file, configFile: path.join(dir, 'index.config.js'),
    });
    assert.equal(status, 200);
    assert.equal(json.success, true);
    const env = JSON.parse(fs.readFileSync(path.join(dir, 'env.json'), 'utf8'));
    assert.deepEqual(env.config, {});
  } finally {
    stopHost(child);
  }
});

test('猫源 /cat/load：configFile 存在时配置对象传给 start', { timeout: 20000 }, async () => {
  const dir = path.join(TMP, 'cat-e');
  const file = writeMockCat(dir, 'site-e', '站点 E');
  fs.writeFileSync(path.join(dir, 'index.config.js'), 'module.exports = { marker: "cfg-ok" };\n');
  const { child, base } = await startHost();
  try {
    const { status, json } = await postCat(base, '/cat/load', {
      key: 'cat-e', file, configFile: path.join(dir, 'index.config.js'),
    });
    assert.equal(status, 200);
    assert.equal(json.success, true);
    const env = JSON.parse(fs.readFileSync(path.join(dir, 'env.json'), 'utf8'));
    assert.deepEqual(env.config, { marker: 'cfg-ok' });
  } finally {
    stopHost(child);
  }
});
