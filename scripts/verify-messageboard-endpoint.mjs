/**
 * 留言板「自起服务」链路验收 —— 不启动 Tauri，用 Node 等价实现验证协议两端对齐。
 *
 * 验的是四件事：
 *   ① web-renderer 的 EndpointModule 能把 `initHttp` 转成 `invoke('endpoint_start')`
 *   ② Rust 侧转过来的 `endpoint://request` 事件能落到应用的 `EventBus.$on('HttpRequest')`
 *   ③ 应用算出来的响应能经 `invoke('endpoint_reply')` 写回 HTTP 客户端
 *   ④ 投一张纸后墙上真的多一张（count 0 → 1）
 *
 * ## 为什么用 Node 模拟而不是真跑 Tauri
 * Tauri 窗口在 CI/无头环境里不好断言，而这条链路的关键是**协议两侧的字段与事件名**
 * （`initHttp([port])` / `HttpRequest` 的 payload / `sendHttpResponse([...])` 四元组）。
 * 用 Node 起一个真 HTTP 服务 + CDP 注入 `__TAURI__` 桩，能精确打中这四点，
 * 且跑一次十几秒。Rust 侧（src-tauri/src/endpoint.rs）靠 cargo check + 真机冒烟覆盖。
 *
 * ## 前置
 *   node scripts/serve.mjs 1488                        # 提供 web-runtime/dist
 *   # 打留言板 zip（构建产物变化后要重打）：
 *   python3 -c "import zipfile,os;src='../../EsApp/messageboard-tv/dist/android';z=zipfile.ZipFile('../../quicktvui/packages/web-runtime/dist/mb.zip','w',zipfile.ZIP_DEFLATED);[z.write(os.path.join(src,f),f) for f in ['index.android.js','vendor.android.js']];z.close()"
 *
 * ## 跑
 *   node scripts/verify-messageboard-endpoint.mjs
 *   BRIDGE_DEBUG=1 node ...                              # 额外打印页面 console
 */
import { spawn } from 'node:child_process'
import { mkdtempSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { createServer } from 'node:http'

const CHROME =
  process.env.CHROME ||
  '/Users/chendd/Library/Caches/ms-playwright/chromium_headless_shell-1217/chrome-headless-shell-mac-arm64/chrome-headless-shell'
const CDP_PORT = Number(process.env.CDP_PORT || 9334)
const PAGE_PORT = Number(process.env.PAGE_PORT || 1488)
const ENDPOINT_PORT = Number(process.env.ENDPOINT_PORT || 39200)
const SHOT_PATH = process.env.SHOT || '/tmp/mb-e2e.png'
const PAGE_URL = `http://127.0.0.1:${PAGE_PORT}/?zipUrl=/mb.zip`

const sleep = (ms) => new Promise((r) => setTimeout(r, ms))
const prof = mkdtempSync(join(tmpdir(), 'mb-e2e-'))
const child = spawn(
  CHROME,
  [
    `--remote-debugging-port=${CDP_PORT}`,
    '--disable-gpu',
    '--no-sandbox',
    '--hide-scrollbars',
    `--user-data-dir=${prof}`,
    '--window-size=1920,1080',
    'about:blank',
  ],
  { stdio: 'ignore' }
)

async function waitVersion() {
  for (let i = 0; i < 60; i++) {
    try {
      const r = await fetch(`http://127.0.0.1:${CDP_PORT}/json/version`)
      if (r.ok) return await r.json()
    } catch {}
    await sleep(250)
  }
  throw new Error('devtools 未就绪')
}
const ver = await waitVersion()
const target = await (await fetch(`http://127.0.0.1:${CDP_PORT}/json/new?about:blank`, { method: 'PUT' })).json()
const ws = new WebSocket(target.webSocketDebuggerUrl)
await new Promise((r, j) => {
  ws.onopen = r
  ws.onerror = j
})

let hid = 0
const pending = new Map()
const logs = []
const errors = []
let onBinding = null

ws.onmessage = (ev) => {
  const msg = JSON.parse(ev.data)
  if (msg.id && pending.has(msg.id)) {
    const { res, rej } = pending.get(msg.id)
    pending.delete(msg.id)
    msg.error ? rej(new Error(JSON.stringify(msg.error))) : res(msg.result)
    return
  }
  if (msg.method === 'Runtime.consoleAPICalled') {
    logs.push(
      `${msg.params.type}: ` +
        (msg.params.args || []).map((a) => (a.value !== undefined ? a.value : a.description || a.type)).join(' ')
    )
  }
  if (msg.method === 'Runtime.exceptionThrown') {
    errors.push(msg.params.exceptionDetails?.exception?.description || JSON.stringify(msg.params.exceptionDetails))
  }
  if (msg.method === 'Runtime.bindingCalled' && msg.params.name === '__TAURI_MSG__' && onBinding) {
    onBinding(msg.params.payload)
  }
}

function send(method, params = {}) {
  const id = ++hid
  return new Promise((res, rej) => {
    pending.set(id, { res, rej })
    ws.send(JSON.stringify({ id, method, params }))
  })
}
const evalJs = async (expr) => {
  const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true })
  if (r.exceptionDetails) return { __error: r.exceptionDetails.text }
  return r.result?.value
}

await send('Runtime.enable')
await send('Page.enable')
await send('Emulation.setDeviceMetricsOverride', { width: 1920, height: 1080, deviceScaleFactor: 1, mobile: false })
await send('Runtime.addBinding', { name: '__TAURI_MSG__' })

/* ---------- 模拟 Rust 侧 endpoint server ---------- */
let reqSeq = 0
/** requestId -> { resolve(bodyObj) } */
const inflight = new Map()
let endpointInfo = null

const emitToPage = async (requestId, method, path, headers, body) => {
  const r = await evalJs(`(() => {
    try {
      const cb = window.__LISTENERS__ && window.__LISTENERS__['endpoint://request']
      if (typeof cb !== 'function') return 'no-listener:' + Object.keys(window.__LISTENERS__ || {}).join(',')
      cb({ payload: ${JSON.stringify({ requestId, method, path, headers, body })} })
      return 'ok'
    } catch (e) {
      return 'throw:' + (e && e.message)
    }
  })()`)
  return r
}

const httpSrv = createServer((req, res) => {
  const chunks = []
  req.on('data', (c) => chunks.push(c))
  req.on('end', async () => {
    const id = ++reqSeq
    const body = Buffer.concat(chunks).toString('utf8')
    const path = req.url || '/'
    const p = new Promise((resolve) => inflight.set(id, resolve))
    const ok = await emitToPage(id, req.method.toUpperCase(), path, JSON.stringify(req.headers), body)
    if (ok !== 'ok') {
      inflight.delete(id)
      console.log('[e2e] emit failed ->', JSON.stringify(ok))
      res.writeHead(503, { 'Content-Type': 'application/json' })
      res.end(JSON.stringify({ ok: false, error: 'no listener in page' }))
      return
    }
    const timeout = setTimeout(() => {
      if (inflight.has(id)) {
        inflight.delete(id)
        res.writeHead(504, { 'Content-Type': 'application/json' })
        res.end(JSON.stringify({ ok: false, error: 'js timeout' }))
      }
    }, 5000)
    const reply = await p
    clearTimeout(timeout)
    res.writeHead(reply.status || 200, { 'Content-Type': reply.contentType || 'application/json' })
    res.end(reply.body || '')
  })
})
await new Promise((r) => httpSrv.listen(ENDPOINT_PORT, '0.0.0.0', r))
console.log('[e2e] 模拟 Rust endpoint 监听 :' + ENDPOINT_PORT)

/* ---------- 页面侧 __TAURI__ mock ---------- */
onBinding = async (raw) => {
  let m
  try {
    m = JSON.parse(raw)
  } catch {
    return
  }
  const { id, cmd, args } = m
  let result = null
  if (cmd === 'endpoint_start') {
    endpointInfo = { success: true, httpPort: ENDPOINT_PORT, ip: '192.168.1.99', deviceUuid: 'sim-desktop' }
    result = endpointInfo
    console.log('[e2e] invoke endpoint_start ->', JSON.stringify(result))
  } else if (cmd === 'endpoint_reply') {
    const wait = inflight.get(Number(args.requestId))
    if (wait) {
      inflight.delete(Number(args.requestId))
      wait({ status: args.status, body: args.body, contentType: args.contentType })
    }
    result = true
  } else if (cmd === 'endpoint_stop') {
    result = true
  } else {
    result = null
  }
  await evalJs(`(() => { const f = window.__TAURI_PENDING__[${id}]; delete window.__TAURI_PENDING__[${id}]; if (f) f(${JSON.stringify(result)}); return 1 })()`)
}

const mockTauri = `
(() => {
  window.__LISTENERS__ = window.__LISTENERS__ || {}
  window.__TAURI_PENDING__ = {}
  let seq = 0
  window.__TAURI__ = {
    core: {
      invoke: (cmd, args) => new Promise((resolve, reject) => {
        const id = ++seq
        window.__TAURI_PENDING__[id] = resolve
        try { window.__TAURI_MSG__(JSON.stringify({ id, cmd, args })) } catch (e) { reject(e) }
      })
    },
    event: {
      listen: (name, cb) => { window.__LISTENERS__[name] = cb; return Promise.resolve(() => { delete window.__LISTENERS__[name] }) }
    }
  }
})();
`

console.log('[e2e] browser:', ver.Browser)
await send('Page.addScriptToEvaluateOnNewDocument', { source: mockTauri })
await send('Page.navigate', { url: PAGE_URL })

// 应用加载 + 初始化（zip 下载/解压 + initHost 多轮探测）
await sleep(26000)

/* ---------- 打几个真实 HTTP 请求 ---------- */
async function probe(method, path, body) {
  const r = await fetch(`http://127.0.0.1:${ENDPOINT_PORT}${path}`, {
    method,
    headers: { 'Content-Type': 'application/json' },
    body: body ? JSON.stringify(body) : undefined,
  })
  const text = await r.text()
  return { status: r.status, body: text.slice(0, 400) }
}

const diag = await evalJs(`JSON.stringify({
  hasTauri: !!window.__TAURI__,
  hasListen: !!(window.__TAURI__ && window.__TAURI__.event && window.__TAURI__.event.listen),
  listeners: Object.keys(window.__LISTENERS__ || {}),
  pendingCount: Object.keys(window.__TAURI_PENDING__ || {}).length
})`)
console.log('[e2e] DIAG ->', diag)

const ping = await probe('GET', '/api/ping')
console.log('[e2e] GET /api/ping ->', ping.status, ping.body)

const state = await probe('GET', '/api/state?room=1234')
console.log('[e2e] GET /api/state ->', state.status, state.body)

const posted = await probe('POST', '/api/messages', {
  id: 'e2e-' + Date.now(),
  layers: [],
  blocks: [],
  canvas: { bg: '#fff8e8' },
})
console.log('[e2e] POST /api/messages ->', posted.status, posted.body)

await sleep(2500)

const probeDom = await evalJs(`(() => {
  const t = (document.getElementById('root') || document.body).innerText || ''
  return JSON.stringify({
    online: !/离线/.test(t),
    hasRoomCode: /\\b\\d{4}\\b/.test(t),
    qrImgs: document.querySelectorAll('canvas').length,
    imgs: document.querySelectorAll('img').length,
    textHasTest: t.indexOf('桌面端测试') >= 0,
    textSample: t.replace(/\\s+/g, ' ').slice(0, 300),
  })
})()`)
console.log('[e2e] DOM ->', probeDom)

const shot = await send('Page.captureScreenshot', { format: 'png' })
writeFileSync(SHOT_PATH, Buffer.from(shot.data, 'base64'))

console.log('=== ERRORS (' + errors.length + ') ===')
errors.slice(0, 8).forEach((e) => console.log(String(e).split('\n').slice(0, 2).join(' | ')))
console.log('=== CONSOLE tail ===')
logs.slice(-30).forEach((l) => console.log(l.slice(0, 200)))

ws.close()
child.kill('SIGKILL')
httpSrv.close()
process.exit(0)
