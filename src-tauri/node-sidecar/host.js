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
// - DNS：v0.0.76 起 Rust 不再把解析结果钉给 Node（pinnedAddrs 为
//   空），Node 用默认 lookup（与 Arcane 完全一致）；Rust 侧的解析只
//   做预检与诊断。v0.0.79：直连时 doFetch 自己拿系统顺序的地址列表
//   逐个试（TLS 被断换地址重试）；SNI 与 Host 仍是目标 hostname，绝不
//   把 IP 当 SNI 发出去。
// - 顶层捕获异常、绝不调 process.exit()：引擎 fatal 会杀掉整个 App。
// - process.cwd() 是 `/`，这里不依赖相对路径。
'use strict';

const http = require('http');
const https = require('https');
const net = require('net');
const tls = require('tls');
const dns = require('dns');
const fs = require('fs');
const path = require('path');

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
// lookup 必须按 opts.all 回值（见 makeLookup 注释，v0.0.73 的教训）。
// v0.0.79：直连时 doFetch 不用 pinnedAddrs（Rust 自 v0.0.76 起传空），
// 而是自己 dns.lookup 拿系统顺序地址列表、每次只钉一个地址逐个试
// （TLS 被断换地址重试；TCP 接通后 TLS 被断 Node 自己不会回落——
// Happy Eyeballs 只管 TCP 接通失败）。
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

// 与 Rust network_policy.rs 的 allowed_ip 同款放行校验（v0.0.80，
// DoH 答案也要过这一关）：内网/保留地址拒绝、FakeIP 198.18.0.0/15
// 放行。IPv4 映射的 IPv6（::ffff:a.b.c.d）按 IPv4 规则；其他内嵌
// IPv4 的形式不在全局单播内，直接拒绝。
function isAllowedIp(ip) {
  const family = net.isIP(ip);
  if (family === 4) {
    const [a, b, c] = ip.split('.').map(Number);
    if (a === 198 && (b === 18 || b === 19)) return true;
    return !(a === 0 || a === 10 || a === 127 || a >= 224
      || (a === 100 && b >= 64 && b <= 127)
      || (a === 169 && b === 254)
      || (a === 172 && b >= 16 && b <= 31)
      || (a === 192 && b === 168)
      || (a === 192 && b === 0 && (c === 0 || c === 2))
      || (a === 192 && b === 88 && c === 99)
      || (a === 198 && b === 51 && c === 100)
      || (a === 203 && b === 0 && c === 113));
  }
  if (family === 6) {
    const mapped = ip.match(/^::ffff:(\d+\.\d+\.\d+\.\d+)$/i);
    if (mapped) return isAllowedIp(mapped[1]);
    if (ip.includes('.')) return false;
    const bytes = ipv6Bytes(ip);
    const seg = [];
    for (let i = 0; i < 8; i++) seg.push(bytes.readUInt16BE(i * 2));
    return (seg[0] & 0xe000) === 0x2000
      && seg[0] !== 0x2002
      && !(seg[0] === 0x2001 && (seg[1] < 0x0200 || seg[1] === 0x0db8))
      && !(seg[0] === 0x3fff && seg[1] < 0x1000);
  }
  return false;
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
//
// v0.0.76：去掉 pinning 的决定性实验——与 Arcane 完全一致，Node 用
// 默认 lookup，不传自定义 lookup、不显式设 autoSelectFamily。
// 证据（见 node-engine.md §7）：① tokio 的 lookup_host 走 std
// getaddrinfo（系统解析器），Rust 并没有绕过 Karing 自建 DNS；
// ② 真机 pinned 列表曾是 24 个 IPv4（v0.0.72 的 [Object] 个数），
// 不是单个 FakeIP；③ Node 18.20.4 的 autoSelectFamily 会在首个地址
// 250ms 未接通时并行试下一个地址（lib/net.js lookupAndConnectMultiple
// + internalConnectMultiple），请求可能落到直连边缘而不是首个地址，
// 而 Arcane 不设它（Node 18 默认关，只试首个地址）。
// v0.0.75 的 doFetch 重试循环随 pinning 一起撤掉（没有 pinned 列表
// 就无地址可换）；诊断改由 Rust 层做（报错末尾的（解析: …），见
// hpp network/api.rs with_resolved_addrs）。
// v0.0.79：直连 TLS 被断换地址重试又回来了——但不用 pinning：doFetch
// 自己拿系统顺序的地址列表逐个试（调用方给了 pinnedAddrs 就用它，
// 否则 dns.lookup；生产上 Rust 自 v0.0.76 起传空，走 dns.lookup）；
// TLS 被断排除该地址再试下一个。背景：v0.0.75 撤掉重试后，直连下第一次出现 TLS
// 被断（2026-11-24 06:25，玩偶 臻选，解析 198.20.0.19:443；直连与
// 跟随系统失败完全一致、解析地址也相同，代理彻底洗清；Arcane 同一
// 时刻能出）。重试第一次有机会真正起作用。代理路径不动
// （CONNECT/SOCKS 由代理自己解析域名，轮换不适用）；v0.0.78 的窄
// 回落不动（仍只管 5xx 与代理握手失败）。
// ---------------------------------------------------------------------------
// 系统顺序的地址列表（dns.lookup，all:true，verbatim:true——不做
// IPv6 优先重排，与 v0.0.74 的结论一致）。解析失败时回 []：doFetch
// 退回 Node 默认 lookup 的单次尝试（与 v0.0.76–78 的行为一致）；
// Rust 侧的预检已先挡过 DNS 失败，这里只是 TOCTOU 窗口里的兜底。
function resolveAddrs(hostname) {
  return new Promise((resolve) => {
    dns.lookup(hostname, { all: true, verbatim: true }, (err, addrs) => {
      if (err || !addrs || addrs.length === 0) return resolve([]);
      resolve(addrs.map((a) => ({ address: a.address, family: a.family })));
    });
  });
}

// ---------------------------------------------------------------------------
// DoH 补充地址（v0.0.80）。背景：v0.0.79 真机证明手机系统解析器对
// 玩偶域名只回 1 个地址（198.20.0.19，06:25/06:33/07:27/07:36 四次
// 全相同），轮换无处可换；但公开 DNS（Google DoH）对同一域名回 5 个
// A 记录（104.17.255.47、172.66.199.198、104.20.56.211、
// 172.66.194.190、172.66.133.214，另有 5 个 AAAA）。DoH 只在系统
// 列表不够用时才查（见 doFetch），查到就补进轮换列表；查不到、
// 失败、超时都静默退回，不报错、不阻塞。答案按主机名在内存里缓存
// 60 秒，同一 burst 里多次请求不重复查。查询走当次请求的同一代理
// 设置（直连就直连——触发只在直连路径；跟随系统就走代理）。
// 测试用环境变量 HG_DOH_BASE_URL 可改 DoH 端点（默认 dns.google）。
// ---------------------------------------------------------------------------
const DOH_CACHE_TTL_MS = 60000;
const dohCache = new Map(); // hostname -> { addrs, expiresAt }

function dohBaseUrl() {
  return (process.env.HG_DOH_BASE_URL || 'https://dns.google').replace(/\/+$/, '');
}

// 一次 DoH 查询（JSON API）：ok 表示 DoH 服务正常应答（Status 为
// 数字，含 NXDOMAIN 等无记录的回答）；ips 只取目标类型的 data。
async function queryDoh(hostname, type, timeoutMs, proxyUrl) {
  try {
    const url = `${dohBaseUrl()}/resolve?name=${encodeURIComponent(hostname)}&type=${type}`;
    const res = await fetchOnce(
      { url, method: 'GET', headers: { accept: 'application/dns-json' }, timeoutMs, proxyUrl: proxyUrl || null, pinnedAddrs: [] },
      [],
      timeoutMs,
    );
    if (res.status !== 200) return { ok: false, ips: [] };
    const json = JSON.parse(Buffer.from(res.bodyBase64 || '', 'base64').toString('utf8'));
    if (!json || typeof json.Status !== 'number' || !Array.isArray(json.Answer)) return { ok: false, ips: [] };
    const want = type === 'A' ? 1 : 28;
    return {
      ok: true,
      ips: json.Answer.filter((a) => a && a.type === want && typeof a.data === 'string').map((a) => a.data),
    };
  } catch {
    return { ok: false, ips: [] };
  }
}

// A + AAAA 并行查，过放行校验、去重；state：ok（有可用地址，
// 60 秒缓存）、empty（服务应答但无可用地址）、failed（查不到）。
async function fetchDohAddrs(hostname, body, deadline) {
  const cached = dohCache.get(hostname);
  if (cached && cached.expiresAt > Date.now()) return { state: 'ok', addrs: cached.addrs };
  const budget = Math.max(1, Math.min(5000, deadline - Date.now()));
  const [a, aaaa] = await Promise.all([
    queryDoh(hostname, 'A', budget, body.proxyUrl),
    queryDoh(hostname, 'AAAA', budget, body.proxyUrl),
  ]);
  const seen = new Set();
  const addrs = [];
  for (const ip of [...a.ips, ...aaaa.ips]) {
    if (typeof ip !== 'string' || !net.isIP(ip) || seen.has(ip) || !isAllowedIp(ip)) continue;
    seen.add(ip);
    addrs.push({ address: ip, family: net.isIP(ip) });
  }
  if (addrs.length > 0) {
    dohCache.set(hostname, { addrs, expiresAt: Date.now() + DOH_CACHE_TTL_MS });
    return { state: 'ok', addrs };
  }
  return { state: a.ok || aaaa.ok ? 'empty' : 'failed', addrs: [] };
}

// 已试地址后缀（v0.0.80）：区分系统列表与 DoH 列表；DoH 没用过时
// 与 v0.0.79 逐字相同。DoH 查过但没拿到可用地址时写明“DoH 无结果”。
function triedSuffix({ sysAddrs, dohAddrs, tried, dohQueried }) {
  const sysTried = tried.filter((a) => !dohAddrs.some((d) => d.address === a));
  const dohTried = tried.filter((a) => dohAddrs.some((d) => d.address === a));
  const sysList = sysTried.length > 0 ? sysTried : sysAddrs.map((a) => a.address);
  const dohList = dohTried.length > 0 ? dohTried : dohAddrs.map((a) => a.address);
  const total = sysAddrs.length + dohAddrs.length;
  const parts = [];
  if (sysList.length > 0) parts.push(`已试: ${sysList.join(', ')}`);
  if (dohList.length > 0) parts.push(`DoH: ${dohList.join(', ')}`);
  if (dohQueried && dohAddrs.length === 0) parts.push('DoH 无结果');
  if (parts.length === 0) return '';
  const count = total === 1 ? '仅 1 个地址，无处可换' : `共 ${total} 个地址`;
  return `（${parts.join('；')}；${count}）`;
}

// 直连时：首个地址 TCP 接通后 TLS 被断，Node 不会试下一个
// （握手失败不是 Happy Eyeballs 的回落条件——v0.0.74 的真机报错与之
// 一字不差，见 tests/test_host.mjs 的 TLS 回落用例）。所以这里显式
// 按地址重试：每次排除掉失败的那个地址，直到列表试完或预算耗尽。
// 只在“请求字节还没发出去”时重试：TCP 没接通（没发），或已接通但
// TLS 没完成（https 的请求字节在握手之后发）；已收到响应、超时、
// 握手已完成都不重试——请求字节已发出，重发不安全。http 没有 TLS：
// 接通后被断说明请求字节已发，不轮换；TCP 没接通时可以轮换（没发）。
// 代理路径不走重试：HTTP CONNECT 由代理自己解析域名，SOCKS 只用首个
// pinned 地址且失败的那个地址从 socket 上认不出来。
async function doFetch(body) {
  const timeoutMs = Math.min(30000, Math.max(1000, Number(body.timeoutMs) || 15000));
  const deadline = Date.now() + timeoutMs;
  const direct = !body.proxyUrl;
  const url = new URL(body.url);
  const secure = url.protocol === 'https:';
  // 轮换的地址列表：调用方给了 pinnedAddrs（测试、或将来恢复固定）
  // 就用它；否则自己 dns.lookup（生产路径，Rust 自 v0.0.76 起传空）。
  // 两个来源都是系统顺序，不重排。
  const pinned = (body.pinnedAddrs || []).map(parsePinnedAddr).filter(Boolean);
  let addrs = direct
    ? (pinned.length > 0 ? pinned : await resolveAddrs(url.hostname))
    : [];
  // 系统列表（DoH 追加后 addrs 会变长，sysAddrs 保持原始系统答案）。
  const sysAddrs = addrs;
  const excluded = new Set();
  const tried = [];
  let firstError = null;
  // v0.0.80：DoH 补充状态（只直连路径用；dohSummary 随响应/错误带
  // 回 Rust，写进 ext.log——主机名、拿到几个地址、成功/失败）。
  let dohAddrs = [];
  let dohQueried = false;
  let dohSummary = null;
  for (;;) {
    const remaining = addrs.filter((a) => !excluded.has(a.address));
    const budget = deadline - Date.now();
    if (budget <= 0) {
      throw upstreamError('TIMEOUT', '请求超时');
    }
    const attempt = remaining.length > 0 ? [remaining[0]] : [];
    try {
      const result = await fetchOnce(body, attempt, Math.max(1, budget));
      if (dohSummary) result.doh = dohSummary;
      return result;
    } catch (e) {
      if (!firstError) firstError = e;
      const attempted = attempt.length > 0 ? attempt[0].address : null;
      if (attempted && !tried.includes(attempted)) tried.push(attempted);
      const noBytesSent = !e._connected || (secure && !e._handshakeDone);
      const retryable = direct
        && e && e.code === 'NETWORK'
        && !e._responded
        && noBytesSent
        && attempted
        && remaining.length > 1;
      if (!retryable) {
        // v0.0.80：系统列表已无未试地址（进到这里时 retryable 只可能
        // 因 remaining <= 1 而失败），且失败是无字节发出的 NETWORK
        // 失败时，向 DoH 补地址再轮换——只查一次，系统列表永远先试。
        // DoH 查不到/无结果时静默退回，只记后缀与 doh 字段，不阻塞。
        if (direct && !dohQueried && e && e.code === 'NETWORK' && !e._responded && noBytesSent) {
          dohQueried = true;
          const doh = await fetchDohAddrs(url.hostname, body, deadline);
          const fresh = doh.addrs.filter((a) => !addrs.some((x) => x.address === a.address) && !excluded.has(a.address));
          if (fresh.length > 0) {
            if (attempted) excluded.add(attempted);
            dohAddrs = fresh;
            addrs = [...addrs, ...fresh];
            dohSummary = `${url.hostname}：补充 ${fresh.length} 个地址`;
            continue;
          }
          dohSummary = doh.state === 'ok'
            ? `${url.hostname}：无新地址`
            : doh.state === 'empty'
              ? `${url.hostname}：无可用地址`
              : `${url.hostname}：查询失败`;
        }
        // 报错里带上已试地址与解析总数，下次真机测试能区分“一个坏
        // 边缘”与“全部都坏”；只回一个地址时直接说明无处可换；没接
        // 通过任何地址时列出全部解析地址（v0.0.75 同款规则）。
        // Rust 层还会再 append（解析: …）——那是预检的答案，与这里
        // Node 自己解析的答案互相印证（DNS 轮换时两者可能不同）。
        if (direct && firstError && firstError.code === 'NETWORK') {
          const suffix = triedSuffix({ sysAddrs, dohAddrs, tried, dohQueried });
          if (suffix) firstError.message = `${firstError.message}${suffix}`;
        }
        if (dohSummary) firstError._doh = dohSummary;
        throw firstError;
      }
      excluded.add(attempted);
    }
  }
}

function fetchOnce(body, attemptAddrs, timeoutMs) {
  const { secure, requestBody, ...reqOpts } = requestOptions(body, timeoutMs, attemptAddrs);
  const lib = secure ? https : http;
  return new Promise((resolve, reject) => {
    // 诊断用：记下这次尝试是否接通、TLS 是否完成、是否收到响应。
    // 只有“请求字节还没发出去”才是可重试的失败（见 doFetch）。
    let connected = false;
    let handshakeDone = false;
    let responded = false;
    const fail = (e) => {
      const err = classifyError(e);
      err._connected = connected;
      err._handshakeDone = handshakeDone;
      err._responded = responded;
      reject(err);
    };
    const req = lib.request(
      reqOpts,
      (res) => {
        responded = true;
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
        res.on('error', (e) => fail(e));
      },
    );
    req.on('socket', (socket) => {
      // keepAlive:false，每次都是新 socket，'socket' 事件在接通前触发；
      // 万一已接通（复用），直接记下。
      if (socket.connecting) {
        socket.once('connect', () => { connected = true; });
      } else {
        connected = true;
      }
      socket.once('secureConnect', () => { handshakeDone = true; });
    });
    const timeoutError = new Error('请求超时');
    timeoutError.code = 'NODE_REQUEST_TIMEOUT';
    req.setTimeout(timeoutMs, () => req.destroy(timeoutError));
    req.on('error', (e) => fail(e));
    if (requestBody) req.end(requestBody);
    else req.end();
  });
}

function requestOptions(body, timeoutMs, addrs) {
  const url = new URL(body.url);
  const method = String(body.method || 'GET').toUpperCase();
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
  // addrs：doFetch 直连轮换时传入的本次地址（[{address, family}]，
  // 通常 1 个）；未传时沿用 body.pinnedAddrs（测试与将来恢复固定时
  // 用）。绝不显式传 autoSelectFamily（见下面）：Node 18 默认关，
  // 与 Arcane 一致；轮换是 doFetch 手动的，自动 Happy Eyeballs 会
  // 把“哪个地址失败”藏起来。
  const lookup = addrs && addrs.length > 0
    ? makeLookup(addrs.map((a) => `${bracket(a.address)}:${targetPort}`))
    : makeLookup(body.pinnedAddrs || []);
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
  return {
    // lib.request 的入参原样（lookup 只在 pinnedAddrs 非空时出现，
    // autoSelectFamily 永不出现——v0.0.76 的契约，见 tests/test_host.mjs），
    // 外加 doFetch 要用的 secure 与 requestBody。
    secure,
    requestBody,
    hostname: targetHost,
    port: targetPort,
    path,
    method,
    headers,
    agent,
    servername: targetHost,
    ...(lookup ? { lookup } : {}),
  };
}

// ---------------------------------------------------------------------------
// 猫源（Node 猫源，见 docs/developers/cat-source.md §5）：加载用户 JS
// 模块并调它的 start(config)，发现它起的 HTTP 服务端口，管生命周期。
// 内容调用不经过边车（不设 /cat/call）：Rust 侧直接调源的 HTTP 服务。
// 配置对象只在内存里传给 start，绝不记日志；GET /config 的原始响应
// 只取站点列表（key、name、type、api），绝不整体返回、不记日志。
// ---------------------------------------------------------------------------
const catSources = new Map(); // key -> { module, port, baseUrl }

// 宿主提供的全局：CatPaw 系猫源的 start() 里以
// `{serverFactory: catServerFactory}` 创建 Fastify 服务，期望宿主在
// 加载前提供该全局（见 docs/developers/cat-source.md §5.3）。等价于
// Fastify 的默认工厂：接请求处理函数、返回一个 http.Server；
// Fastify 调它时可能多传一个 opts 参数，直接忽略。
globalThis.catServerFactory = (handler) => http.createServer(handler);

// 取一个空闲端口：临时 net.Server 绑端口 0，读到端口后关闭。
function allocFreePort() {
  return new Promise((resolve, reject) => {
    const s = net.createServer();
    s.once('error', reject);
    s.listen(0, '127.0.0.1', () => {
      const port = s.address().port;
      s.close(() => resolve(port));
    });
  });
}

// 向源的 HTTP 服务 GET 一条路径（2 秒超时）：收到任何响应即算通。
function probeCat(port, pathname) {
  return new Promise((resolve) => {
    const req = http.get(
      { host: '127.0.0.1', port, path: pathname, timeout: 2000 },
      (res) => {
        res.resume();
        resolve(true);
      },
    );
    req.on('timeout', () => { req.destroy(); resolve(false); });
    req.on('error', () => resolve(false));
  });
}

// GET /config，解析 JSON；拿不到或解析失败回 null。
function fetchCatConfig(port) {
  return new Promise((resolve) => {
    const req = http.get(
      { host: '127.0.0.1', port, path: '/config', timeout: 2000 },
      (res) => {
        const chunks = [];
        res.on('data', (c) => chunks.push(c));
        res.on('end', () => {
          try {
            resolve(JSON.parse(Buffer.concat(chunks).toString('utf8')));
          } catch {
            resolve(null);
          }
        });
      },
    );
    req.on('timeout', () => { req.destroy(); resolve(null); });
    req.on('error', () => resolve(null));
  });
}

// 猫源配置归一化（见 docs/developers/cat-source.md §5.2）：
// 1) esbuild 打包的 ESM 默认导出：index.config.js 的 require 结果是
//    {default: 真实配置, __esModule: true}，要解包，否则 start 里
//    e.config.sites 是 undefined，读 .list 即炸（真机已复现）。
// 2) 缺键补最小形状：真源 start() 里 e.config.sites.list、
//    e.config.pans.list 是不设防的读法，e.config.tgsou[t]、
//    e.config.danmu.urls 也是；无配置的源（网络导入）否则同样会炸。
//    只补结构键；网盘凭证仍需真实配置文件。
function normalizeCatConfig(config) {
  if (!config || typeof config !== 'object') config = {};
  if (config.__esModule === true && config.default && typeof config.default === 'object') {
    config = config.default;
  }
  if (!config.sites || typeof config.sites !== 'object') config.sites = { list: [] };
  else if (!Array.isArray(config.sites.list)) config.sites.list = [];
  if (!config.pans || typeof config.pans !== 'object') config.pans = { list: [] };
  else if (!Array.isArray(config.pans.list)) config.pans.list = [];
  if (config.tgsou === undefined) config.tgsou = {};
  if (config.danmu === undefined) config.danmu = {};
  return config;
}

async function handleCatLoad(req, res) {
  const fail = (message) => sendJson(res, 500, { success: false, error: message });
  try {
    const body = await readJsonBody(req);
    const key = body && body.key;
    const file = body && body.file;
    const configFile = body && body.configFile;
    if (typeof key !== 'string' || !key || typeof file !== 'string' || !file) {
      fail('缺少猫源 key 或文件路径');
      return;
    }
    if (!fs.existsSync(file)) {
      fail('猫源文件不存在');
      return;
    }
    // 同 key 已加载时先停掉旧的（尽力而为、抛错也继续），再覆盖。
    const old = catSources.get(key);
    if (old) {
      catSources.delete(key);
      try {
        if (old.module && typeof old.module.stop === 'function') await old.module.stop();
      } catch {
        // 停旧源失败也不阻塞新加载。
      }
    }
    // 空闲端口 + 数据目录（plugin.js 所在目录下的 data 子目录）。
    const port = await allocFreePort();
    const dataDir = path.join(path.dirname(file), 'data');
    fs.mkdirSync(dataDir, { recursive: true });
    // 调 start 前设好：JsonDB 在 start() 内创建，位置由 NODE_PATH 定；
    // 先于 require 设，覆盖 JsonDB 万一在顶层创建的情况。
    process.env.DEV_HTTP_PORT = String(port);
    process.env.NODE_PATH = dataDir;
    let mod;
    try {
      delete require.cache[require.resolve(file)];
      mod = require(file);
    } catch (e) {
      // 缺模块是最常见的加载失败（源 require 了未提供的第三方包），
      // 单独给明错；其他加载失败（语法错等）带原始文案另报。
      if (e && e.code === 'MODULE_NOT_FOUND') {
        fail(`该源依赖未提供的模块：${e.message}`);
      } else {
        fail(`猫源文件加载失败：${(e && e.message) || String(e)}`);
      }
      return;
    }
    if (!mod || typeof mod.start !== 'function') {
      fail('该文件不是猫源：缺少 start');
      return;
    }
    // 配置：有文件就 require 得对象，否则空对象；只在内存里传给 start。
    let config = {};
    if (typeof configFile === 'string' && configFile && fs.existsSync(configFile)) {
      try {
        delete require.cache[require.resolve(configFile)];
        config = require(configFile);
      } catch (e) {
        fail(`配置文件读取失败：${(e && e.message) || String(e)}`);
        return;
      }
    }
    // 配置归一化：ESM 默认导出解包 + 缺键补最小形状（见 normalizeCatConfig）。
    config = normalizeCatConfig(config);
    // start() 本身没超时：源内部一旦挂住（等网络等），这个 handler 永不
    // 回，Rust 侧只能等预算超时。30 秒还没起就按失败报。
    let startTimer;
    try {
      await Promise.race([
        mod.start(config),
        new Promise((_, reject) => {
          startTimer = setTimeout(() => reject(new Error('猫源启动超时（30 秒）')), 30000);
        }),
      ]);
    } catch (e) {
      fail(`猫源启动失败：${(e && e.message) || String(e)}`);
      return;
    } finally {
      if (startTimer) clearTimeout(startTimer);
    }
    // 探活：/test 不通试 /config；仍不通按 EADDRINUSE 重试逻辑向后探 +1…+5。
    let actualPort = 0;
    for (let p = port; p <= port + 5; p++) {
      if (await probeCat(p, '/test') || await probeCat(p, '/config')) {
        actualPort = p;
        break;
      }
    }
    if (!actualPort) {
      fail('猫源服务未启动');
      return;
    }
    // 拿站点列表：只保留 key、name、type、api 四个字段；原始响应不返回、不记日志。
    const raw = await fetchCatConfig(actualPort);
    const sites = raw && raw.video && Array.isArray(raw.video.sites)
      ? raw.video.sites.map((s) => ({ key: s.key, name: s.name, type: s.type, api: s.api }))
      : [];
    if (sites.length === 0) {
      fail('该源没有可用站点');
      return;
    }
    const baseUrl = `http://127.0.0.1:${actualPort}`;
    catSources.set(key, { module: mod, port: actualPort, baseUrl });
    sendJson(res, 200, { success: true, port: actualPort, baseUrl, sites });
  } catch (e) {
    sendJson(res, 500, { success: false, error: (e && e.message) || String(e) });
  }
}

async function handleCatUnload(req, res) {
  try {
    const body = await readJsonBody(req);
    const key = body && body.key;
    const entry = typeof key === 'string' ? catSources.get(key) : undefined;
    if (entry) {
      catSources.delete(key);
      try {
        if (entry.module && typeof entry.module.stop === 'function') await entry.module.stop();
      } catch {
        // 尽力而为：stop 抛错也静默成功。
      }
    }
  } catch {
    // 尽力而为：请求体读失败也静默成功。
  }
  sendJson(res, 200, { success: true });
}

function handleCatList(req, res) {
  sendJson(res, 200, { sources: [...catSources.keys()] });
}

// ---------------------------------------------------------------------------
// HTTP 服务：/health、/request、/diagnose、/cat/*。
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

// ---------------------------------------------------------------------------
// 网络诊断探针（v0.0.80）。只读：不改 net.request、回落、clearance、
// 代理决议，不碰 Cookie 罐，结果不持久化。入口是设置页的“网络诊断”
// 按钮（hpp 命令 net.diagnose → Rust Network::diagnose → POST
// /diagnose）。与真实请求同一条路、同一代理决议（proxyUrl 由 Rust
// 按对话框当前选中的模式传进来，v0.0.82；之前是已保存的设置）；DoH
// 对照直接调 fetchDohAddrs，不受 doFetch“系统列表不够用才查”的
// 限制——探针就是要对照两边。v0.0.82 起 /diagnose 除报告文本外再回
// 一份域名小结（每域名的实测计数），Rust 侧拼结论行用。
// ---------------------------------------------------------------------------
const DIAGNOSE_TLS_TIMEOUT_MS = 5000;
const DIAGNOSE_HTTP_TIMEOUT_MS = 10000;
const DIAGNOSE_DEADLINE_MS = 60000;
const CHALLENGE_RE = /just a moment|cf-browser|access denied|attention required|cf_chl/i;

// 单个边缘的 TLS 握手：SNI=域名、host=IP。'connect' 表示 TCP 通，
// 'secureConnect' 表示 TLS 通（实测：RST 只触发 error、黑洞触发
// timeout；见 tests/test_host.mjs 的探针用例）。port 默认 443；
// 测试用 port 参数指到本机临时端口（Rust 侧不传，永远是 443）。
// 探针报告每地址只占一行：错误信息（尤其 OpenSSL 的，尾部常带
// 换行）先压成单行再拼进报告。
function oneLine(s) {
  return String(s ?? '').replace(/\s+/g, ' ').trim();
}

function probeTls(host, ip, timeoutMs, port = 443) {
  return new Promise((resolve) => {
    const started = Date.now();
    let tcpOk = false;
    let done = false;
    const finish = (result) => {
      if (done) return;
      done = true;
      resolve({ ip, ms: Date.now() - started, ...result });
    };
    const socket = tls.connect({ host: ip, port, servername: host, timeout: timeoutMs });
    socket.on('connect', () => { tcpOk = true; });
    socket.on('secureConnect', () => {
      const cipher = socket.getCipher() || {};
      finish({ tcpOk: true, tlsOk: true, cipher: `${cipher.version || '?'} / ${cipher.name || '?'}`, alpn: socket.alpnProtocol || null });
      socket.destroy();
    });
    socket.on('timeout', () => {
      finish({ tcpOk, tlsOk: false, error: tcpOk ? 'TLS 握手超时' : 'TCP 连接超时' });
      socket.destroy();
    });
    socket.on('error', (e) => {
      finish({ tcpOk, tlsOk: false, error: oneLine((e && e.message) || String(e)) });
    });
    socket.on('close', () => {
      finish({ tcpOk, tlsOk: false, error: '连接被关闭' });
    });
  });
}

// 握手通的边缘再发一次 HTTPS GET（钉死该 IP，与真实请求同款
// UA/头）：记状态码与 body 是否挑战页。返回 {text, ok}：ok 只在
// 2xx 且非挑战页时为真（v0.0.82：结论行要按边缘计数，文本不够）。
async function probeHttp(host, ip, port, proxyUrl, deadline) {
  const timeoutMs = Math.max(1, Math.min(DIAGNOSE_HTTP_TIMEOUT_MS, deadline - Date.now()));
  try {
    const res = await fetchOnce(
      { url: `https://${host}${port === 443 ? '' : `:${port}`}/`, method: 'GET', headers: { 'User-Agent': 'Mozilla/5.0', accept: 'text/html,application/xhtml+xml' }, timeoutMs, proxyUrl: proxyUrl || null, pinnedAddrs: [`${ip}:${port}`] },
      [{ address: ip, family: net.isIP(ip) }],
      timeoutMs,
    );
    const body = Buffer.from(res.bodyBase64 || '', 'base64').toString('utf8');
    const challenge = CHALLENGE_RE.test(body);
    return { text: `${res.status}，${challenge ? '挑战页' : '非挑战页'}`, ok: res.status >= 200 && res.status < 300 && !challenge };
  } catch (e) {
    return { text: `失败（${oneLine((e && e.message) || String(e))}）`, ok: false };
  }
}

// 一个域名：系统解析与 DoH 各查一遍（并行），去重后逐边缘握手
// （并行），握手通的再发 HTTPS GET（并行）。返回 {lines, summary}：
// summary 是本次实测的计数（v0.0.82：Rust 侧拼结论行用；edges 是
// 去重后的边缘数，是 tlsOk/tlsFail/httpOk 的分母——sys 与 doh
// 可能重叠，不能直接相加）。
async function diagnoseDomain(host, proxyUrl, port, deadline) {
  const lines = [];
  const [sys, doh] = await Promise.all([
    resolveAddrs(host),
    fetchDohAddrs(host, { proxyUrl }, deadline),
  ]);
  const fmt = (addrs) => addrs.map((a) => a.address).join(', ');
  let dnsLine = `${host}：系统 ${sys.length} 个 [${fmt(sys)}]`;
  if (sys.length === 0) dnsLine += '（无记录）';
  dnsLine += `；DoH ${doh.addrs.length} 个 [${fmt(doh.addrs)}]`;
  if (doh.state === 'failed') dnsLine += '（查询失败）';
  else if (doh.state === 'empty') dnsLine += '（无记录）';
  lines.push(dnsLine);
  const seen = new Set();
  const ips = [];
  for (const a of [...sys, ...doh.addrs]) {
    if (!seen.has(a.address)) { seen.add(a.address); ips.push(a.address); }
  }
  const summary = { host, sys: sys.length, doh: doh.addrs.length, dohState: doh.state, edges: ips.length, tlsOk: 0, tlsFail: 0, httpOk: 0 };
  if (ips.length === 0) {
    lines.push('  无可用地址，未做握手');
    return { lines, summary };
  }
  const handshakes = await Promise.all(ips.map((ip) => probeTls(host, ip, DIAGNOSE_TLS_TIMEOUT_MS, port)));
  const httpResults = new Map();
  await Promise.all(handshakes.filter((h) => h.tlsOk).map(async (h) => {
    httpResults.set(h.ip, await probeHttp(host, h.ip, port, proxyUrl, deadline));
  }));
  for (const h of handshakes) {
    if (!h.tcpOk) lines.push(`  ${h.ip}：TCP 不通（${h.error}，${h.ms}ms）`);
    else if (!h.tlsOk) lines.push(`  ${h.ip}：TCP 通，TLS 失败（${h.error}，${h.ms}ms）`);
    else lines.push(`  ${h.ip}：TCP 通，TLS 通（${h.cipher}，ALPN ${h.alpn || '无 ALPN'}，${h.ms}ms）；HTTP ${httpResults.get(h.ip).text}`);
  }
  summary.tlsOk = handshakes.filter((h) => h.tlsOk).length;
  summary.tlsFail = handshakes.filter((h) => h.tcpOk && !h.tlsOk).length;
  summary.httpOk = [...httpResults.values()].filter((r) => r.ok).length;
  return { lines, summary };
}

// 出口 IP：cloudflare trace 优先（顺带记 colo），不可用时换下一个。
async function diagnoseEgress(egressUrls, proxyUrl, deadline) {
  for (const url of egressUrls) {
    const timeoutMs = Math.max(1, Math.min(DIAGNOSE_HTTP_TIMEOUT_MS, deadline - Date.now()));
    try {
      const res = await fetchOnce(
        { url, method: 'GET', headers: { 'User-Agent': 'Mozilla/5.0', accept: 'text/plain' }, timeoutMs, proxyUrl: proxyUrl || null, pinnedAddrs: [] },
        [],
        timeoutMs,
      );
      if (res.status !== 200) continue;
      const text = Buffer.from(res.bodyBase64 || '', 'base64').toString('utf8');
      if (url.includes('cdn-cgi/trace')) {
        const ip = /ip=([^\r\n]+)/.exec(text)?.[1]?.trim();
        const colo = /colo=([^\r\n]+)/.exec(text)?.[1]?.trim();
        if (ip) return `出口 IP：${ip}${colo ? `（colo=${colo}，cloudflare trace）` : '（cloudflare trace）'}`;
      } else {
        const ip = text.trim();
        if (net.isIP(ip)) return `出口 IP：${ip}（${new URL(url).hostname}）`;
      }
    } catch {
      // 换下一个出口查询地址。
    }
  }
  return '出口 IP：未知（出口查询失败）';
}

async function diagnose(body) {
  const proxyUrl = (body && body.proxyUrl) || null;
  const proxyDesc = (body && body.proxyDesc) || '未知';
  const domains = Array.isArray(body && body.domains) ? body.domains.filter((d) => typeof d === 'string' && d) : [];
  const egressUrls = Array.isArray(body && body.egressUrls) ? body.egressUrls.filter((u) => typeof u === 'string' && u) : [];
  // port 只给测试用（本机临时端口）；生产永远是 443（Rust 不传）。
  const port = body && Number(body.port) > 0 ? Math.floor(Number(body.port)) : 443;
  const deadline = Date.now() + DIAGNOSE_DEADLINE_MS;
  const lines = [`网络诊断 ${new Date().toLocaleString('zh-CN', { hour12: false })}（代理：${proxyDesc}）`];
  const perDomain = await Promise.all(domains.map((d) => diagnoseDomain(d, proxyUrl, port, deadline)));
  for (const part of perDomain) lines.push(...part.lines);
  lines.push(await diagnoseEgress(egressUrls, proxyUrl, deadline));
  // v0.0.82：除报告文本外再回一份域名小结（每域名的实测计数），
  // Rust 侧拼结论行用；报告文本本身一字不动。
  return { report: lines.join('\n'), summary: { domains: perDomain.map((p) => p.summary) } };
}

async function handleDiagnose(req, res) {
  try {
    const body = await readJsonBody(req);
    const { report, summary } = await diagnose(body || {});
    sendJson(res, 200, { report, summary });
  } catch (e) {
    sendJson(res, 500, { error: (e && e.message) || String(e), code: 'INTERNAL' });
  }
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
    // 绝不把传输失败伪装成 HTTP 状态码。v0.0.80：doFetch 的 DoH
    // 使用情况随 doh 字段带回（Rust 写进 ext.log）。
    const payload = { error: message, code };
    if (e && e._doh) payload.doh = e._doh;
    sendJson(res, 500, payload);
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
  if (req.method === 'POST' && pathname === '/diagnose') {
    handleDiagnose(req, res).catch((e) => {
      console.error(`handleDiagnose failed: ${e && e.stack || e}`);
      if (!res.headersSent) sendJson(res, 500, { error: 'Node 边车内部错误', code: 'NETWORK' });
    });
    return;
  }
  if (req.method === 'POST' && pathname === '/cat/load') {
    handleCatLoad(req, res).catch((e) => {
      console.error(`handleCatLoad failed: ${e && e.stack || e}`);
      if (!res.headersSent) sendJson(res, 500, { success: false, error: 'Node 边车内部错误' });
    });
    return;
  }
  if (req.method === 'POST' && pathname === '/cat/unload') {
    handleCatUnload(req, res).catch((e) => {
      console.error(`handleCatUnload failed: ${e && e.stack || e}`);
      if (!res.headersSent) sendJson(res, 200, { success: true });
    });
    return;
  }
  if (req.method === 'GET' && pathname === '/cat/list') {
    handleCatList(req, res);
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

// 测试用：关闭进程内边车服务器。集成测试 require host.js 后监听
// socket 会拖住测试进程退出（node --test 等不到进程结束）；
// after 里调一次。只关当前在监听的 server（健康时一定在监听）。
function closeServer() {
  return new Promise((resolve) => {
    if (!server || !server.listening) { resolve(); return; }
    server.close(() => resolve());
  });
}

// 供集成测试直接覆盖 lookup 契约（见 tests/test_host.mjs）：本机 Node 24
// 默认 autoSelectFamily 为 true，端到端测不到 Node 18 的 all:false 路径，
// 只能把 makeLookup 导出后单独调。requestOptions 也导出：v0.0.76 的
// “无自定义 lookup、不设 autoSelectFamily”契约要能被测试直接断言。
// resolveAddrs 也导出：v0.0.79 的轮换地址列表（系统顺序）要能被测试
// 直接断言（本机 localhost 回 2 个地址；顺序以系统解析器为准，测试
// 里不硬编码顺序）。
// doFetch 也导出：v0.0.80 的 DoH 轮换要在测试进程内直接调——沙箱会
// 拦截跨进程发往非 loopback 地址的 TCP（即使本机有监听；同进程内
// 直达），spawned 边车子进程连不上 DoH 给的非保留 IP。入口运行时
// 无影响。
module.exports = { makeLookup, parsePinnedAddr, requestOptions, resolveAddrs, isAllowedIp, probeTls, doFetch, closeServer, normalizeCatConfig };
