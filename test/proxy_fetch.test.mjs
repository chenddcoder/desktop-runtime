// proxy_fetch.js 单元测试
//
// 背景：es_pkg 加载链路（loader.js → CrossAppResolver → XHR POST run.quicktvui.com）
// 被 proxy_fetch.js 在 document-start 包裹 XHR 原型，转交 Tauri 的 proxy_http 命令绕过 CORS。
// 历史上根因是 proxy_fetch 用了 window.__TAURI__.invoke，而 Tauri v2 全局 API 把 invoke
// 放在 window.__TAURI__.core.invoke（顶层无 invoke），导致运行时 TypeError → resolve 失败 →
// 页面报「无法加载应用：本地无缓存且服务器解析失败」。
//
// 本测试在 Node 里用 vm 模拟 webview 环境，验证拦截/转发/还原逻辑，并锁定 invoke 路径修复。

import { test } from 'node:test'
import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import vm from 'node:vm'
import { fileURLToPath } from 'node:url'

const __dirname = path.dirname(fileURLToPath(import.meta.url))
const PROXY_SRC = fs.readFileSync(
  path.resolve(__dirname, '../src-tauri/proxy_fetch.js'),
  'utf8'
)

const flush = () => new Promise((r) => setTimeout(r, 10))

// invoke mock：记录调用并返回 impl 提供的响应
function makeInvokeMock(impl) {
  const calls = []
  const fn = (cmd, args) => {
    calls.push({ cmd, args })
    return Promise.resolve(
      impl ? impl(cmd, args) : { status: 200, headers: {}, bodyBase64: '' }
    )
  }
  fn.calls = calls
  return fn
}

// 在干净的 sandbox 里执行 proxy_fetch.js（每次重建，避免全局包裹副作用污染）
function buildSandbox(tauriShape) {
  class XHRMock {
    constructor() {
      this.responseType = ''
      this.onload = null
      this.onreadystatechange = null
      this.onerror = null
      this.status = 0
      this.statusText = ''
      this.responseText = ''
      this.response = null
      this.readyState = 0
      this.__ph = {}
      this.__nativeSendCalled = false
      this.__nativeSendBody = undefined
    }
  }
  XHRMock.prototype.open = function () {
    this.__nativeOpenCalled = true
  }
  XHRMock.prototype.send = function (body) {
    this.__nativeSendCalled = true
    this.__nativeSendBody = body
  }
  XHRMock.prototype.setRequestHeader = function (n, v) {
    this.__ph[n] = v
  }

  const win = {
    location: {
      href: 'http://localhost:1420/?es_pkg=es.tv.huan.escast',
      host: 'localhost:1420',
      hostname: 'localhost',
      port: '1420',
      origin: 'http://localhost:1420',
    },
    fetch: () => Promise.resolve(),
    XMLHttpRequest: XHRMock,
    __TAURI__: tauriShape,
  }

  const sandbox = {
    window: win,
    location: win.location,
    XMLHttpRequest: XHRMock,
    URL,
    TextEncoder,
    TextDecoder,
    atob,
    btoa,
    Blob,
    Headers,
    Response,
    Event,
    console,
    setTimeout,
    clearTimeout,
    Uint8Array,
    ArrayBuffer,
  }
  vm.createContext(sandbox)
  vm.runInContext(PROXY_SRC, sandbox)
  return { win, sandbox, XHRMock }
}

// 已知响应体（加密 resolve 响应的替身）
const RESP_BYTES = [9, 9, 9, 9]
const RESP_B64 = Buffer.from(RESP_BYTES).toString('base64')
// 已知请求体（加密 resolve 请求体）
const REQ_BYTES = [1, 2, 3, 4]
const REQ_B64 = Buffer.from(REQ_BYTES).toString('base64')

test('拦截 run.quicktvui.com 的 XHR 并转发 body_base64/method/url/headers', async () => {
  const invoke = makeInvokeMock(() => ({
    status: 200,
    headers: { 'content-type': 'application/octet-stream' },
    bodyBase64: RESP_B64,
  }))
  const { XHRMock } = buildSandbox({ core: { invoke } })

  const xhr = new XHRMock()
  let loaded = false
  xhr.onload = () => {
    loaded = true
  }
  xhr.open('POST', 'https://run.quicktvui.com/api/app/resolve', true)
  xhr.responseType = 'arraybuffer'
  xhr.setRequestHeader('Content-Type', 'application/octet-stream')
  xhr.send(new Uint8Array(REQ_BYTES).buffer)
  await flush()

  // 必须转交 proxy_http
  assert.equal(invoke.calls.length, 1)
  // Tauri v2 command 参数名严格匹配 Rust 签名：`proxy_http(req: ProxyReq)`。
  // 前端 invoke 第二参必须包成 `{ req: {...} }`，不能顶层散开 —— 否则 Tauri 报
  // "missing required key req."，业务链路在运行时静默断链。
  const req = invoke.calls[0].args.req
  assert.ok(req, 'invoke 第二参必须含 req 键（对应 Rust ProxyReq 参数）')
  assert.equal(req.url, 'https://run.quicktvui.com/api/app/resolve')
  assert.equal(req.method, 'POST')
  // 加密请求体必须原样 base64 转发（否则服务端解密失败）
  assert.equal(req.body_base64, REQ_B64)
  assert.equal(req.headers['Content-Type'], 'application/octet-stream')

  // 响应还原：onload 被触发、status=200、response 是 ArrayBuffer 且字节一致
  assert.equal(loaded, true)
  assert.equal(xhr.status, 200)
  assert.ok(xhr.response instanceof ArrayBuffer)
  assert.deepEqual(Array.from(new Uint8Array(xhr.response)), RESP_BYTES)
})

test('*.quicktvui.com 子域也被代理（CDN zip 下载）', async () => {
  const invoke = makeInvokeMock(() => ({ status: 200, headers: {}, bodyBase64: RESP_B64 }))
  const { XHRMock } = buildSandbox({ core: { invoke } })
  const xhr = new XHRMock()
  xhr.onload = () => {}
  xhr.open('GET', 'https://cdn.quicktvui.com/some/app.zip', true)
  xhr.responseType = 'arraybuffer'
  xhr.send(null)
  await flush()
  assert.equal(invoke.calls.length, 1)
})

test('非 quicktvui.com 请求不走代理（走原生 XHR）', async () => {
  const invoke = makeInvokeMock()
  const { XHRMock } = buildSandbox({ core: { invoke } })
  const xhr = new XHRMock()
  xhr.open('GET', 'https://example.com/foo', true)
  xhr.send(null)
  await flush()
  assert.equal(invoke.calls.length, 0, 'invoke 不应被调用')
  assert.equal(xhr.__nativeSendCalled, true, '应回落到原生 send')
})

test('关键回归：v2 全局 API 形态 { core:{ invoke } } 能被正确使用', async () => {
  // 这是本机真实形态（顶层无 invoke）。修复前这里会抛 TypeError → resolve 失败。
  const invoke = makeInvokeMock(() => ({ status: 200, headers: {}, bodyBase64: RESP_B64 }))
  const { XHRMock } = buildSandbox({ core: { invoke } }) // 注意：没有顶层 invoke
  const xhr = new XHRMock()
  xhr.onload = () => {}
  xhr.open('POST', 'https://run.quicktvui.com/api/app/resolve', true)
  xhr.responseType = 'arraybuffer'
  xhr.send(new Uint8Array(REQ_BYTES).buffer)
  await flush()
  assert.equal(invoke.calls.length, 1, '必须用 core.invoke 代理成功')
})

test('兼容 v1 形态：顶层 invoke 也可用', async () => {
  const invoke = makeInvokeMock(() => ({ status: 200, headers: {}, bodyBase64: RESP_B64 }))
  const { XHRMock } = buildSandbox({ invoke }) // 顶层直接有 invoke（v1 形态）
  const xhr = new XHRMock()
  xhr.onload = () => {}
  xhr.open('POST', 'https://run.quicktvui.com/api/app/resolve', true)
  xhr.responseType = 'arraybuffer'
  xhr.send(new Uint8Array(REQ_BYTES).buffer)
  await flush()
  assert.equal(invoke.calls.length, 1)
})

test('invoke 完全不可用时：graceful onerror，而非同步崩溃', async () => {
  const { XHRMock } = buildSandbox({}) // 没有任何 invoke
  const xhr = new XHRMock()
  let errored = false
  xhr.onerror = () => {
    errored = true
  }
  // 不应抛出同步异常（旧代码 window.__TAURI__.invoke(...) 会在这里崩）
  assert.doesNotThrow(() => {
    xhr.open('POST', 'https://run.quicktvui.com/api/app/resolve', true)
    xhr.responseType = 'arraybuffer'
    xhr.send(new Uint8Array(REQ_BYTES).buffer)
  })
  await flush()
  assert.equal(errored, true, '应触发 onerror 让上层降级')
})

// ============================ dev-proxy 形式（autoProxy 改写） ============================
// 背景：web-runtime dev 模式下 autoProxy 把跨域请求改写到 `<current-host>/proxy?url=<encoded>`
// 若不识别这形式，proxy_fetch.js 会放行（host=localhost 不命中），请求落到 serve.mjs 的 SPA
// fallback → 返回整页 HTML 当响应 → CrossAppResolver 拿 HTML 解密 → fail。
//
// 修复：识别 dev-proxy 形式时解开 `url` 参数，把原始 URL 直接交给 Rust proxy_http 转发。

test('dev-proxy 形式 XHR 被解开拦截（autoProxy 改写到 /proxy?url=...）', async () => {
  const invoke = makeInvokeMock(() => ({ status: 200, headers: {}, bodyBase64: RESP_B64 }))
  const { XHRMock } = buildSandbox({ core: { invoke } })
  const xhr = new XHRMock()
  xhr.onload = () => {}
  // dev 模式 autoProxy 会把 https://run.quicktvui.com/... 改写成 http://localhost:1420/proxy?url=<encoded>
  const devProxyUrl =
    'http://localhost:1420/proxy?url=' +
    encodeURIComponent('https://run.quicktvui.com/api/app/resolve')
  xhr.open('POST', devProxyUrl, true)
  xhr.responseType = 'arraybuffer'
  xhr.send(new Uint8Array(REQ_BYTES).buffer)
  await flush()

  assert.equal(invoke.calls.length, 1, '必须转给 proxy_http')
  const req = invoke.calls[0].args.req
  // 必须是解开后的原始 URL（不走 localhost 中转）
  assert.equal(req.url, 'https://run.quicktvui.com/api/app/resolve')
  assert.equal(req.method, 'POST')
  assert.equal(req.body_base64, REQ_B64)
})

test('dev-proxy 形式里 url 指向非 quicktvui.com 时放行', async () => {
  const invoke = makeInvokeMock()
  const { XHRMock } = buildSandbox({ core: { invoke } })
  const xhr = new XHRMock()
  xhr.open('GET', 'http://localhost:1420/proxy?url=' + encodeURIComponent('https://example.com/foo'), true)
  xhr.send(null)
  await flush()
  assert.equal(invoke.calls.length, 0, '不应用 quicktvui 规则束缚 example.com')
  assert.equal(xhr.__nativeSendCalled, true)
})

test('dev-proxy 形式 fetch 也被解开拦截', async () => {
  const invoke = makeInvokeMock(() => ({ status: 200, headers: {}, bodyBase64: RESP_B64 }))
  let captured = null
  const origFetch = () => {
    captured = 'native'
    return Promise.resolve(new Response())
  }
  const ctx = buildSandbox({ core: { invoke } })
  ctx.win.fetch = origFetch
  ctx.sandbox.fetch = origFetch
  // 重新在 sandbox 里加载（fetch mock 必须装到 sandbox 后才能被脚本捕获）
  vm.runInContext(PROXY_SRC, ctx.sandbox)

  const devProxyUrl =
    'http://localhost:1420/proxy?url=' +
    encodeURIComponent('https://cdn.quicktvui.com/app.zip')
  const resp = await ctx.win.fetch(devProxyUrl)
  await flush()

  assert.equal(captured, null, 'native fetch 不应被调用')
  assert.equal(invoke.calls.length, 1)
  assert.equal(invoke.calls[0].args.req.url, 'https://cdn.quicktvui.com/app.zip')
})
