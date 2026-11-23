// HGhub Node 边车入口——引擎启动点（iOS 上由宿主引导调用 node_start）。
//
// 内嵌的 Node.js 引擎（nodejs-mobile v18.20.4）跑这个文件：一个只监听
// 回环地址的 HTTP 服务，把 Rust 侧插件 net 请求翻译成 Node 的
// `https.request` 发出去。只用 Node 核心模块，不引 npm 包。
//
// 行为约定（复刻 Arcane 的 Node 18 https 画像，见
// docs/developers/node-engine.md §5.2）：
// - 不加任何默认头：只发 Rust 传来的头（Node 自己补 Host，有 body 时
//   这里补 Content-Length）；不发 sec-ch-ua、Accept、Accept-Language
//   等浏览器头；不跟重定向；HTTP/1.1；连接不复用（keepAlive: false）。
// - 证书校验保持开启（Node 自带 Mozilla 根证书，不许关）。
// - 代理（CONNECT / SOCKS5/5h / SOCKS4/4a）全部手写核心模块实现，
//   代理地址由 Rust 每跳传来；回环目标不经过这里。
// - DNS 固定：Rust 传来的 pinnedAddrs 经自定义 lookup 返回，
//   SNI 与 Host 仍是目标 hostname，绝不把 IP 当 SNI 发出去。
// - 顶层捕获异常、绝不调 process.exit()：引擎 fatal 会杀掉整个 App。
// - process.cwd() 是 `/`，这里不依赖相对路径。
'use strict';

const http = require('http');
const https = require('https');
const net = require('net');
const tls = require('tls');
const fs = require('fs');

const DEFAULT_PORT = 55101;
const MAX_RESPONSE_BYTES = 8 * 1024 * 1024;
const MAX_REQUEST_JSON_BYTES = 4 * 1024 * 1024;

let actualPort = 0;
let server = null;

function log(...args) {
  console.log(...args);
}

function writeInfoFile() {
  const infoPath = process.env.HG_NODE_INFO_PATH;
  if (!infoPath) return;
  try {
    const info = { apiBase: `http://127.0.0.1:${actualPort}` };
    const tmp = `${infoPath}.tmp`;
    fs.writeFileSync(tmp, JSON.stringify(info));
    fs.renameSync(tmp, infoPath);
    log(`Node sidecar info written to ${infoPath}: ${JSON.stringify(info)}`);
  } catch (e) {
    console.error(`WARNING: cannot write sidecar info file: ${e.message}`);
  }
}

// ---------------------------------------------------------------------------
// 错误分类：Rust 侧 NodeClient 按 code 映射（见 hpp node/client.rs）。
// ---------------------------------------------------------------------------
function upstreamError(code, message) {
  const err = new Error(message);
  err.code = code;
  return err;
}

function classifyError(e) {
  if (e && e.code === 'NODE_REQUEST_TIMEOUT') {
    return upstreamError('TIMEOUT', '请求超时');
  }
  if (e && e.code && ['ECONNREFUSED', 'ENOTFOUND', 'EAI_AGAIN', 'ECONNRESET', 'EPIPE', 'ETIMEDOUT', 'EHOSTUNREACH', 'ENETUNREACH'].includes(e.code)) {
    return upstreamError('NETWORK', e.message);
  }
  // 代理握手里抛出的错文案已写明原因（"无法连接代理目标服务器"等），
  // 直接按 NETWORK 透传；TLS 失败同样是 NETWORK。
  return upstreamError('NETWORK', e && e.message ? e.message : String(e));
}

// ---------------------------------------------------------------------------
// DNS 固定：pinnedAddrs 是 "ip:port" 字符串数组（Rust 已校验）。
// lookup 必须返回 all:true 的完整列表，让 Node 在首个地址失败时试下一个。
// ---------------------------------------------------------------------------
function parsePinnedAddr(s) {
  const m = s.match(/^\[([^\]]+)\]:(\d+)$/) || s.match(/^([^:]+):(\d+)$/);
  if (!m) return null;
  const family = net.isIP(m[1]);
  if (family === 0) return null;
  return { address: m[1], family };
}

function makeLookup(pinnedAddrs) {
  if (!pinnedAddrs || pinnedAddrs.length === 0) return undefined;
  const addrs = pinnedAddrs.map(parsePinnedAddr).filter(Boolean);
  if (addrs.length === 0) return undefined;
  // Node 的 lookup 契约：opts.all 为 true 时回 [{address, family}] 数组，
  // 为 false 时回单个地址字符串 + family。Node 18 上 autoSelectFamily
  // 默认关闭，lookup 以 all:false 被调用；无条件回数组会让 Node 18 把
  // 数组当成单个 IP，抛 Invalid IP address（本机 Node 24 默认 all:true，
  // 所以旧代码在本机测试里一直通过，真机才炸——v0.0.72 的教训）。
  return (_hostname, opts, cb) => {
    if (opts && opts.all) return cb(null, addrs);
    const first = addrs[0];
    return cb(null, first.address, first.family);
  };
}

function bracket(host) {
  return host.includes(':') && !host.startsWith('[') ? `[${host}]` : host;
}

// ---------------------------------------------------------------------------
// 代理地址解析。
// ---------------------------------------------------------------------------
function parseProxy(proxyUrl) {
  const u = new URL(proxyUrl);
  const scheme = u.protocol.replace(':', '');
  const user = u.username ? decodeURIComponent(u.username) : '';
  const pass = u.password ? decodeURIComponent(u.password) : '';
  const port = u.port
    ? Number(u.port)
    : scheme === 'http'
      ? 80
      : scheme === 'https'
        ? 443
        : 1080;
  return {
    scheme,
    host: u.hostname,
    port,
    user,
    // Basic 认证串（CONNECT 与 SOCKS5 用户名密码认证共用）。
    auth: user ? `${user}:${pass}` : null,
  };
}

// ---------------------------------------------------------------------------
// 带缓冲的精确读取器：SOCKS 握手用；握手后剩余字节要原样留给 TLS。
// ---------------------------------------------------------------------------
class Reader {
  constructor(socket) {
    this.socket = socket;
    this.buf = Buffer.alloc(0);
    this.waiters = [];
    // 握手完必须 detach（见 connectViaSocks5/4）：否则这个 data 监听
    // 会继续吃掉 socket 上的响应字节，HTTP 解析器收不到数据。
    this.onData = (chunk) => {
      this.buf = Buffer.concat([this.buf, chunk]);
      this.pump();
    };
    socket.on('data', this.onData);
  }

  detach() {
    this.socket.removeListener('data', this.onData);
  }

  pump() {
    while (this.waiters.length > 0) {
      const w = this.waiters[0];
      if (this.buf.length < w.n) break;
      this.waiters.shift();
      const out = this.buf.subarray(0, w.n);
      this.buf = this.buf.subarray(w.n);
      w.resolve(out);
    }
  }

  read(n) {
    if (this.buf.length >= n) {
      const out = this.buf.subarray(0, n);
      this.buf = this.buf.subarray(n);
      return Promise.resolve(out);
    }
    return new Promise((resolve, reject) => {
      this.waiters.push({ n, resolve, reject });
    });
  }

  leftover() {
    const out = this.buf;
    this.buf = Buffer.alloc(0);
    return out;
  }
}

function withTimeout(promise, ms, message) {
  let timer;
  const timeout = new Promise((_resolve, reject) => {
    timer = setTimeout(() => reject(upstreamError('NETWORK', message)), ms);
  });
  return Promise.race([promise.finally(() => clearTimeout(timer)), timeout]);
}

function ipv6Bytes(ip) {
  // 展开 :: 压缩，返回 16 字节。
  const [head, tail] = ip.split('::');
  const headParts = head ? head.split(':').filter((s) => s.length > 0) : [];
  const tailParts = tail !== undefined && tail ? tail.split(':').filter((s) => s.length > 0) : [];
  const missing = 8 - headParts.length - tailParts.length;
  const parts = [...headParts, ...Array(missing).fill('0'), ...tailParts];
  const out = Buffer.alloc(16);
  parts.forEach((p, i) => out.writeUInt16BE(parseInt(p, 16), i * 2));
  return out;
}

// ---------------------------------------------------------------------------
// HTTP(S) 代理：CONNECT 隧道（只用于 https 目标；http 目标走绝对 URI）。
// ---------------------------------------------------------------------------
function connectViaHttpProxy(proxy, targetHost, targetPort, servername, timeoutMs) {
  return new Promise((resolve, reject) => {
    const socket = net.connect({ host: proxy.host, port: proxy.port });
    let settled = false;
    const timer = setTimeout(() => {
      fail(upstreamError('NETWORK', '无法连接代理目标服务器：代理连接超时'));
    }, timeoutMs);
    const fail = (err) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      socket.destroy();
      reject(err);
    };
    socket.once('error', (e) => {
      fail(upstreamError('NETWORK', `无法连接代理目标服务器：${e.message}`));
    });
    socket.once('connect', () => {
      let req = `CONNECT ${bracket(targetHost)}:${targetPort} HTTP/1.1\r\n`
        + `Host: ${bracket(targetHost)}:${targetPort}\r\n`;
      if (proxy.auth) {
        req += `Proxy-Authorization: Basic ${Buffer.from(proxy.auth).toString('base64')}\r\n`;
      }
      req += '\r\n';
      socket.write(req);
    });
    let buf = Buffer.alloc(0);
    socket.on('data', (chunk) => {
      if (settled) return;
      buf = Buffer.concat([buf, chunk]);
      const end = buf.indexOf('\r\n\r\n');
      if (end === -1) {
        if (buf.length > 65536) fail(upstreamError('NETWORK', '代理响应过大'));
        return;
      }
      const head = buf.subarray(0, end).toString('latin1');
      const status = parseInt(head.split(' ')[1], 10);
      const rest = buf.subarray(end + 4);
      if (status !== 200) {
        fail(upstreamError('NETWORK', `代理拒绝 CONNECT（HTTP ${status}）`));
        return;
      }
      settled = true;
      clearTimeout(timer);
      socket.removeAllListeners('data');
      socket.removeAllListeners('error');
      if (rest.length > 0) socket.unshift(rest);
      const tlsSocket = tls.connect({ socket, servername });
      tlsSocket.once('error', (e) => reject(classifyError(e)));
      resolve(tlsSocket);
    });
  });
}

// ---------------------------------------------------------------------------
// SOCKS5 / SOCKS5h。
// ---------------------------------------------------------------------------
async function connectViaSocks5(proxy, target, servername, secure, timeoutMs) {
  const socket = net.connect({ host: proxy.host, port: proxy.port });
  const reader = new Reader(socket);
  const fail = (message) => {
    socket.destroy();
    throw upstreamError('NETWORK', message);
  };
  try {
    await withTimeout(
      new Promise((resolve, reject) => {
        socket.once('error', reject);
        socket.once('connect', resolve);
      }),
      timeoutMs,
      '无法连接代理目标服务器：代理连接超时',
    );
    // 握手中途的 socket 错误不再单独处理：读超时（withTimeout）与
    // 请求超时兜底；这里只保证它不会变成未捕获的 error 事件杀掉进程。
    socket.on('error', () => {});
    // 问候：有认证时同时报无认证与用户名密码两种方式。
    const methods = proxy.user ? [0x00, 0x02] : [0x00];
    socket.write(Buffer.from([0x05, methods.length, ...methods]));
    const greet = await withTimeout(reader.read(2), timeoutMs, 'SOCKS5 代理握手超时');
    if (greet[0] !== 0x05) await fail('SOCKS5 代理返回无效版本');
    if (greet[1] === 0xff) await fail('SOCKS5 代理拒绝（无可用认证方式）');
    if (greet[1] === 0x02) {
      const user = Buffer.from(proxy.user, 'utf8');
      const pass = Buffer.from(proxy.auth.split(':').slice(1).join(':'), 'utf8');
      socket.write(Buffer.concat([
        Buffer.from([0x01, user.length]),
        user,
        Buffer.from([pass.length]),
        pass,
      ]));
      const auth = await withTimeout(reader.read(2), timeoutMs, 'SOCKS5 代理认证超时');
      if (auth[1] !== 0x00) await fail('SOCKS5 代理认证失败');
    } else if (greet[1] !== 0x00) {
      await fail(`SOCKS5 代理要求不支持的认证方式（0x${greet[1].toString(16)}）`);
    }
    // CONNECT：socks5h 送域名由代理解析；socks5 送 Rust 已解析的 IP。
    let addrPart;
    if (target.type === 'domain') {
      const domain = Buffer.from(target.value, 'utf8');
      addrPart = Buffer.concat([Buffer.from([0x03, domain.length]), domain]);
    } else if (target.family === 6) {
      addrPart = Buffer.concat([Buffer.from([0x04]), ipv6Bytes(target.value)]);
    } else {
      const ip = target.value.split('.').map(Number);
      addrPart = Buffer.from([0x01, ...ip]);
    }
    const portBuf = Buffer.alloc(2);
    portBuf.writeUInt16BE(target.port, 0);
    socket.write(Buffer.concat([
      Buffer.from([0x05, 0x01, 0x00]),
      addrPart,
      portBuf,
    ]));
    const head = await withTimeout(reader.read(4), timeoutMs, 'SOCKS5 代理握手超时');
    if (head[0] !== 0x05) await fail('SOCKS5 代理返回无效版本');
    if (head[1] !== 0x00) await fail(`SOCKS5 代理拒绝连接（REP 0x${head[1].toString(16)}）`);
    const atyp = head[3];
    const addrLen = atyp === 0x01 ? 4 : atyp === 0x04 ? 16 : atyp === 0x03
      ? (await withTimeout(reader.read(1), timeoutMs, 'SOCKS5 代理握手超时'))[0]
      : await fail(`SOCKS5 代理返回未知地址类型（0x${atyp.toString(16)}）`);
    await withTimeout(reader.read(addrLen + 2), timeoutMs, 'SOCKS5 代理握手超时');
    const rest = reader.leftover();
    reader.detach();
    if (rest.length > 0) socket.unshift(rest);
    if (!secure) return socket;
    const tlsSocket = tls.connect({ socket, servername });
    tlsSocket.once('error', () => {});
    return tlsSocket;
  } catch (e) {
    socket.destroy();
    throw e instanceof Error && e.code ? e : classifyError(e);
  }
}

// ---------------------------------------------------------------------------
// SOCKS4 / SOCKS4a。socks4 送 IP（无固定地址时退化成 4a：0.0.0.1 + 域名）。
// ---------------------------------------------------------------------------
async function connectViaSocks4(proxy, target, servername, secure, timeoutMs) {
  const socket = net.connect({ host: proxy.host, port: proxy.port });
  const reader = new Reader(socket);
  try {
    await withTimeout(
      new Promise((resolve, reject) => {
        socket.once('error', reject);
        socket.once('connect', resolve);
      }),
      timeoutMs,
      '无法连接代理目标服务器：代理连接超时',
    );
    socket.on('error', () => {});
    const portBuf = Buffer.alloc(2);
    portBuf.writeUInt16BE(target.port, 0);
    const user = Buffer.from(proxy.user || '', 'utf8');
    let ipPart;
    let domainPart = Buffer.alloc(0);
    if (target.type === 'ip') {
      ipPart = Buffer.from(target.value.split('.').map(Number));
    } else {
      // 4a：IP 填 0.0.0.1，域名跟在用户名后。
      ipPart = Buffer.from([0, 0, 0, 1]);
      domainPart = Buffer.concat([Buffer.from(target.value, 'utf8'), Buffer.from([0x00])]);
    }
    socket.write(Buffer.concat([
      Buffer.from([0x04, 0x01]),
      portBuf,
      ipPart,
      user,
      Buffer.from([0x00]),
      domainPart,
    ]));
    const reply = await withTimeout(reader.read(8), timeoutMs, 'SOCKS4 代理握手超时');
    if (reply[1] !== 0x5a) {
      socket.destroy();
      throw upstreamError('NETWORK', `SOCKS4 代理拒绝连接（状态 0x${reply[1].toString(16)}）`);
    }
    const rest = reader.leftover();
    reader.detach();
    if (rest.length > 0) socket.unshift(rest);
    if (!secure) return socket;
    const tlsSocket = tls.connect({ socket, servername });
    tlsSocket.once('error', () => {});
    return tlsSocket;
  } catch (e) {
    socket.destroy();
    throw e instanceof Error && e.code ? e : classifyError(e);
  }
}

// 代理 agent：按 scheme 选握手；每次请求现选，不缓存。
function proxyAgent(proxy, target, secure, timeoutMs) {
  const Base = secure ? https.Agent : http.Agent;
  return new (class extends Base {
    constructor() {
      super({ keepAlive: false });
    }

    createConnection(options, cb) {
      const servername = options.servername || options.host;
      let p;
      if (proxy.scheme === 'http' || proxy.scheme === 'https') {
        if (secure) {
          p = connectViaHttpProxy(proxy, options.host, options.port, servername, timeoutMs);
        } else {
          // http 目标走绝对 URI：直接连代理，请求行由调用方写成绝对形式。
          p = new Promise((resolve, reject) => {
            const s = net.connect({ host: proxy.host, port: proxy.port });
            s.once('error', (e) => reject(upstreamError(
              'NETWORK',
              `无法连接代理目标服务器：${e.message}`,
            )));
            s.once('connect', () => resolve(s));
          });
        }
      } else if (proxy.scheme === 'socks5' || proxy.scheme === 'socks5h') {
        p = connectViaSocks5(proxy, target, servername, secure, timeoutMs);
      } else if (proxy.scheme === 'socks4' || proxy.scheme === 'socks4a') {
        p = connectViaSocks4(proxy, target, servername, secure, timeoutMs);
      } else {
        p = Promise.reject(upstreamError('NETWORK', `不支持的代理协议：${proxy.scheme}`));
      }
      p.then((socket) => cb(null, socket), (err) => cb(classifyError(err)));
    }
  })();
}

// SOCKS 的目标地址：socks5h/4a 送域名；socks5/4 送 Rust 已解析的 IP
// （pinnedAddrs 首个；没有时退化成送域名）。
function socksTarget(proxy, pinnedAddrs, targetHost, targetPort) {
  const first = (pinnedAddrs || []).map(parsePinnedAddr).find(Boolean);
  if (proxy.scheme === 'socks5h' || proxy.scheme === 'socks4a') {
    return { type: 'domain', value: targetHost, port: targetPort };
  }
  if (first) {
    return { type: 'ip', value: first.address, family: first.family, port: targetPort };
  }
  return { type: 'domain', value: targetHost, port: targetPort };
}

// ---------------------------------------------------------------------------
// /request：一次上游请求。
// ---------------------------------------------------------------------------
function doFetch(body) {
  const url = new URL(body.url);
  const method = String(body.method || 'GET').toUpperCase();
  const timeoutMs = Math.min(30000, Math.max(1000, Number(body.timeoutMs) || 15000));
  const secure = url.protocol === 'https:';
  const targetHost = url.hostname;
  const targetPort = url.port ? Number(url.port) : secure ? 443 : 80;
  const headers = { ...(body.headers || {}) };
  let requestBody = null;
  if (body.bodyBase64) {
    requestBody = Buffer.from(body.bodyBase64, 'base64');
    // Rust 侧已把 Content-Length 过滤掉；有 body 时由这里现算。
    headers['content-length'] = String(requestBody.length);
  }
  const lookup = makeLookup(body.pinnedAddrs);
  let agent;
  let path = url.pathname + url.search;
  if (!body.proxyUrl) {
    agent = secure
      ? new https.Agent({ keepAlive: false })
      : new http.Agent({ keepAlive: false });
  } else {
    const proxy = parseProxy(body.proxyUrl);
    const target = socksTarget(proxy, body.pinnedAddrs, targetHost, targetPort);
    agent = proxyAgent(proxy, target, secure, timeoutMs);
    if (!secure && (proxy.scheme === 'http' || proxy.scheme === 'https')) {
      // http 目标经 HTTP 代理：请求行写绝对 URI。
      path = body.url;
    }
  }
  const lib = secure ? https : http;
  return new Promise((resolve, reject) => {
    const req = lib.request(
      {
        hostname: targetHost,
        port: targetPort,
        path,
        method,
        headers,
        agent,
        servername: targetHost,
        // 显式开 Happy Eyeballs：Node 18 默认 autoSelectFamily 为 false，
        // 不写这里 lookup 只拿到首个地址（首个不通就直接失败）；写死 true
        // 后 lookup 以 all:true 被调用，返回完整 pinned 列表，Node 逐个试。
        autoSelectFamily: true,
        ...(lookup ? { lookup } : {}),
      },
      (res) => {
        const chunks = [];
        let size = 0;
        let tooBig = false;
        res.on('data', (chunk) => {
          size += chunk.length;
          if (size > MAX_RESPONSE_BYTES) {
            tooBig = true;
            req.destroy();
          } else {
            chunks.push(chunk);
          }
        });
        res.on('end', () => {
          if (tooBig) {
            reject(upstreamError('UPSTREAM_ERROR', '响应体超过 8 MiB'));
            return;
          }
          // res.headers 原样：set-cookie 保持数组，其余已合并成字符串。
          resolve({
            status: res.statusCode,
            headers: res.headers,
            bodyBase64: Buffer.concat(chunks).toString('base64'),
          });
        });
        res.on('error', (e) => reject(classifyError(e)));
      },
    );
    const timeoutError = new Error('请求超时');
    timeoutError.code = 'NODE_REQUEST_TIMEOUT';
    req.setTimeout(timeoutMs, () => req.destroy(timeoutError));
    req.on('error', (e) => reject(classifyError(e)));
    if (requestBody) req.end(requestBody);
    else req.end();
  });
}

// ---------------------------------------------------------------------------
// HTTP 服务：/health、/request。
// ---------------------------------------------------------------------------
function sendJson(res, status, value) {
  const text = JSON.stringify(value);
  res.writeHead(status, { 'content-type': 'application/json' });
  res.end(text);
}

function readJsonBody(req) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    let size = 0;
    req.on('data', (chunk) => {
      size += chunk.length;
      if (size > MAX_REQUEST_JSON_BYTES) {
        req.destroy();
        reject(upstreamError('UPSTREAM_ERROR', '请求体超过上限'));
      } else {
        chunks.push(chunk);
      }
    });
    req.on('end', () => {
      try {
        resolve(JSON.parse(Buffer.concat(chunks).toString('utf8')));
      } catch {
        reject(upstreamError('UPSTREAM_ERROR', '请求体不是合法 JSON'));
      }
    });
    req.on('error', (e) => reject(classifyError(e)));
  });
}

async function handleRequest(req, res) {
  try {
    const body = await readJsonBody(req);
    if (!body || typeof body.url !== 'string' || !body.url) {
      sendJson(res, 400, { error: '缺少请求 URL' });
      return;
    }
    const result = await doFetch(body);
    sendJson(res, 200, result);
  } catch (e) {
    const code = e && e.code ? e.code : 'NETWORK';
    const message = e && e.message ? e.message : String(e);
    // 超时、连接失败、TLS 失败、DNS 失败都走 500 + {error, code}，
    // 绝不把传输失败伪装成 HTTP 状态码。
    sendJson(res, 500, { error: message, code });
  }
}

function onRequest(req, res) {
  const pathname = new URL(req.url, 'http://127.0.0.1').pathname;
  if (req.method === 'GET' && pathname === '/health') {
    sendJson(res, 200, { ok: true, node: process.version });
    return;
  }
  if (req.method === 'POST' && pathname === '/request') {
    handleRequest(req, res).catch((e) => {
      console.error(`handleRequest failed: ${e && e.stack || e}`);
      if (!res.headersSent) sendJson(res, 500, { error: 'Node 边车内部错误', code: 'NETWORK' });
    });
    return;
  }
  sendJson(res, 404, { error: 'not found' });
}

// ---------------------------------------------------------------------------
// 自愈监听：默认 55101，被占落临时端口；出错重建监听，不退出进程。
// ---------------------------------------------------------------------------
function startServer(port) {
  const s = http.createServer(onRequest);
  s.on('error', (err) => {
    if (!s.listening && err.code === 'EADDRINUSE' && port !== 0) {
      log(`端口 ${port} 被占，改用临时端口`);
      startServer(0);
      return;
    }
    console.error(`Node sidecar server error: ${err.message}，1 秒后重建监听`);
    setTimeout(() => startServer(port), 1000);
  });
  s.listen(port, '127.0.0.1', () => {
    server = s;
    actualPort = s.address().port;
    writeInfoFile();
    log(`Node sidecar listening on http://127.0.0.1:${actualPort} (node ${process.version})`);
  });
}

// 顶层捕获异常、只记日志，绝不调 process.exit()——退出会杀掉整个 App。
process.on('uncaughtException', (err) => {
  console.error(`uncaughtException: ${err && err.stack || err}`);
});
process.on('unhandledRejection', (reason) => {
  console.error(`unhandledRejection: ${reason}`);
});

log(`Node sidecar starting (node ${process.version}, cwd ${process.cwd()})`);
startServer(DEFAULT_PORT);

// 供集成测试直接覆盖 lookup 契约（见 tests/test_host.mjs）：本机 Node 24
// 默认 autoSelectFamily 为 true，端到端测不到 Node 18 的 all:false 路径，
// 只能把 makeLookup 导出后单独调。入口运行时无影响。
module.exports = { makeLookup, parsePinnedAddr };
