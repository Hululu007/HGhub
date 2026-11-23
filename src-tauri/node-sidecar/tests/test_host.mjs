// Node 边车 host.js 的集成测试：用本机 Node（v24）跑 host.js，
// 对本地 HTTP/HTTPS 服务器与 mock 代理发请求。
//
// 注意：本机 Node 是 v24，目标引擎是 Node 18.20.4——所用核心 API
// （http/https/net/tls、自定义 agent、lookup）在 18→24 间稳定，
// 仍要在真机上以 Node 18 复核（见 node-engine.md §10.1）。
//
// 跑：node --test src-tauri/node-sidecar/tests/test_host.mjs
import { test, before, after } from 'node:test';
import assert from 'node:assert/strict';
import { spawn, execFileSync } from 'node:child_process';
import http from 'node:http';
import https from 'node:https';
import net from 'node:net';
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
    '[req]\ndistinguished_name=dn\n[dn]\n[v3]\nsubjectAltName=DNS:myhost.test\n',
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
