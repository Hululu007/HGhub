# Node 引擎许可证与第三方 NOTICE

HGhub 的 iOS 包内嵌 nodejs-mobile v18.20.4（Node.js 18.20.4），
用于插件 net 请求（见 `docs/developers/node-engine.md`）。
本文件随包分发 Node.js 许可全文与第三方 NOTICE 摘要。

Node.js 许可全文（nodejs-mobile 与 Node.js 同许可，MIT）：

```
Node.js is licensed for use as follows:

"""
Copyright Node.js contributors. All rights reserved.

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to
deal in the Software without restriction, including without limitation the
rights to use, copy, modify, merge, publish, distribute, sublicense, and/or
sell copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in
all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS
IN THE SOFTWARE.
"""

This license applies to parts of Node.js originating from the
https://github.com/joyent/node repository:

"""
Copyright Joyent, Inc. and other Node contributors. All rights reserved.
Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to
deal in the Software without restriction, including without limitation the
rights to use, copy, modify, merge, publish, distribute, sublicense, and/or
sell copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in
all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS
IN THE SOFTWARE.
"""
```

## 第三方 NOTICE 摘要

Node.js 内含的外部维护库（完整许可文本见上游 LICENSE）：

| 组件 | 许可 |
| --- | --- |
| V8 | BSD |
| libuv | MIT |
| OpenSSL 3.0 | Apache-2.0 |
| zlib | zlib |
| c-ares | MIT |
| nghttp2 | MIT |
| Brotli | MIT |
| npm | Artistic 2.0 |
| ICU | Unicode License V3 |
| Acorn | MIT |
| cjs-module-lexer | MIT |

完整许可文本（4011 行复合 bundle）：

- 上游：<https://raw.githubusercontent.com/nodejs/node/v18.20.4/LICENSE>
- nodejs-mobile 仓库：<https://github.com/nodejs-mobile/nodejs-mobile>

注意：Node 18 已 EOL（安全支持止于 2025-04-30），nodejs-mobile
实际停滞；本版是 pin 死 v18.20.4 的假设检验，不是长期运行时承诺
（见 `docs/developers/node-engine.md` §3.4）。
