#!/usr/bin/env node
/**
 * desktop-verify MCP server —— 桌面端快应用的**无头**验证通道。
 *
 * ## 为什么要有它
 * 调试 desktop-runtime 时最烦的一件事：每验一次就得 `open 投屏显象.app`，窗口抢焦点、
 * 覆盖用户正在做的事，还可能因为上一个实例没退干净而抢端口（5990 / 39200）→ 黑屏误判。
 * 这个 MCP 把验证搬到 headless chromium 里：加载同一份 `web-runtime/dist`，
 * 用真实的 `?es_pkg=` / `?zipUrl=` 链路走 resolve → 下载 → 解压 → 渲染，
 * 然后把**探针数据 + 截图路径**返回。全程不碰用户的窗口。
 *
 * ## 它验证的是什么 / 不是什么
 * ✅ 包能不能被后台解析到、能不能下载解密、解压后能不能挂载、有没有 JS 错误、viewport 里有什么
 * ❌ 不验证 Tauri 特有能力（DLNA、WebDAV、endpoint 真端口）—— 那些走
 *    `scripts/verify-messageboard-endpoint.mjs`（Node 模拟 Rust）或真机冒烟
 *
 * ## 工具
 *   desktop_verify    载入一个快应用包并断言（pkg 或 zipUrl 二选一）
 *   desktop_catalog   读后台 `/api/runtime/catalog`，看有哪些包、版本、图标
 *   desktop_screenshot 只截图（给定 url）
 *   desktop_log       读桌面端日志尾部（默认 /tmp/desk.log）
 *
 * ## 用法（stdio MCP）
 * 在 `~/.workbuddy/mcp.json` 的 mcpServers 里加：
 *   "desktop-verify": {
 *     "command": "node",
 *     "args": ["/Users/chendd/Documents/self/扩展屏快应用/desktop-runtime/tools/desktop-mcp.mjs"],
 *     "env": { "DESKTOP_ROOT": "/Users/chendd/Documents/self/扩展屏快应用" }
 *   }
 * 然后到连接器管理页点"信任"。
 *
 * 单独调试（不开 MCP 也能跑）：
 *   node tools/desktop-mcp.mjs --selftest
 */

import { spawn } from 'node:child_process'
import { mkdtempSync, writeFileSync, readFileSync, existsSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const HERE = dirname(fileURLToPath(import.meta.url))
const DESKTOP_ROOT = process.env.DESKTOP_ROOT || resolve(HERE, '..')
const WORKSPACE = resolve(DESKTOP_ROOT, '..')
const WEB_RUNTIME_DIST = join(WORKSPACE, 'quicktvui', 'packages', 'web-runtime', 'dist')
const RESOLVER = process.env.DESKTOP_RESOLVER || 'https://www.chenddcoder.cn'
const CHROME =
  process.env.CHROME ||
  `${process.env.HOME}/Library/Caches/ms-playwright/chromium_headless_shell-1217/chrome-headless-shell-mac-arm64/chrome-headless-shell`

const sleep = (ms) => new Promise((r) => setTimeout(r, ms))

/* ---------------- 静态服务 ---------------- */

let serveProc = null
let servePort = 0

async function ensureServe(port = 1488) {
  const alive = await fetch(`http://127.0.0.1:${port}/index.html`)
    .then((r) => r.ok)
    .catch(() => false)
  if (alive) {
    servePort = port
    return port
  }
  if (!existsSync(join(WEB_RUNTIME_DIST, 'index.html'))) {
    throw new Error('web-runtime/dist 不存在，先构建：cd quicktvui/packages/web-runtime && npx webpack --mode production')
  }
  serveProc = spawn('node', [join(DESKTOP_ROOT, 'scripts/serve.mjs'), String(port)], {
    stdio: 'ignore',
    detached: false,
  })
  for (let i = 0; i < 40; i++) {
    await sleep(200)
    const ok = await fetch(`http://127.0.0.1:${port}/index.html`)
      .then((r) => r.ok)
      .catch(() => false)
    if (ok) {
      servePort = port
      return port
    }
  }
  throw new Error('静态服务起不来（端口 ' + port + '）')
}

/* ---------------- CDP ---------------- */

class Cdp {
  constructor(ws) {
    this.ws = ws
    this.id = 0
    this.pending = new Map()
    this.logs = []
    this.errors = []
    ws.onmessage = (ev) => {
      const m = JSON.parse(ev.data)
      if (m.id && this.pending.has(m.id)) {
        const { res, rej } = this.pending.get(m.id)
        this.pending.delete(m.id)
        m.error ? rej(new Error(JSON.stringify(m.error))) : res(m.result)
        return
      }
      if (m.method === 'Runtime.consoleAPICalled') {
        this.logs.push(
          `${m.params.type}: ` +
            (m.params.args || []).map((a) => (a.value !== undefined ? a.value : a.description || a.type)).join(' ')
        )
        if (this.logs.length > 400) this.logs.shift()
      }
      if (m.method === 'Runtime.exceptionThrown') {
        this.errors.push(m.params.exceptionDetails?.exception?.description || JSON.stringify(m.params.exceptionDetails))
      }
    }
  }
  send(method, params = {}) {
    const id = ++this.id
    return new Promise((res, rej) => {
      this.pending.set(id, { res, rej })
      this.ws.send(JSON.stringify({ id, method, params }))
    })
  }
  async eval(expression) {
    const r = await this.send('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true })
    if (r.exceptionDetails) return { __error: r.exceptionDetails.text || 'eval error' }
    return r.result?.value
  }
}

async function loadPage(url, waitMs) {
  const prof = mkdtempSync(join(tmpdir(), 'dsk-verify-'))
  const cdpPort = 9400 + Math.floor(Math.random() * 300)
  const child = spawn(
    CHROME,
    [
      `--remote-debugging-port=${cdpPort}`,
      '--disable-gpu',
      '--no-sandbox',
      '--hide-scrollbars',
      `--user-data-dir=${prof}`,
      '--window-size=1920,1080',
      'about:blank',
    ],
    { stdio: 'ignore' }
  )
  try {
    let ok = false
    for (let i = 0; i < 60; i++) {
      try {
        const r = await fetch(`http://127.0.0.1:${cdpPort}/json/version`)
        if (r.ok) {
          ok = true
          break
        }
      } catch {}
      await sleep(250)
    }
    if (!ok) throw new Error('chromium devtools 未就绪')

    const target = await (
      await fetch(`http://127.0.0.1:${cdpPort}/json/new?about:blank`, { method: 'PUT' })
    ).json()
    const ws = new WebSocket(target.webSocketDebuggerUrl)
    await new Promise((res, rej) => {
      ws.onopen = res
      ws.onerror = rej
    })
    const cdp = new Cdp(ws)
    await cdp.send('Runtime.enable')
    await cdp.send('Page.enable')
    await cdp.send('Emulation.setDeviceMetricsOverride', {
      width: 1920,
      height: 1080,
      deviceScaleFactor: 1,
      mobile: false,
    })
    await cdp.send('Page.navigate', { url })
    await sleep(waitMs)

    const probe = await cdp.eval(`(() => {
      const app = document.getElementById('app')
      const t = (document.body.innerText || '').replace(/\\s+/g, ' ')
      const splash = document.getElementById('quickapp-splash')
      const cs = splash ? getComputedStyle(splash) : null
      return JSON.stringify({
        url: location.href,
        appChildren: app ? app.children.length : -1,
        // 画布尺寸比"数量"有用得多：2×4 这种值一眼能看出是尺寸算错（不是没渲染）
        canvases: (() => {
          const out = []
          const cs = document.querySelectorAll('canvas')
          for (let i = 0; i < cs.length && i < 8; i += 1) {
            const c = cs[i]
            const r = c.getBoundingClientRect()
            const p = c.parentElement ? c.parentElement.getBoundingClientRect() : null
            out.push({
              attr: [c.width, c.height],
              rect: [Math.round(r.width), Math.round(r.height)],
              parent: p ? [Math.round(p.width), Math.round(p.height)] : null,
              op: c.parentElement ? getComputedStyle(c.parentElement).opacity : null,
            })
          }
          return out
        })(),
        imgs: document.querySelectorAll('img').length,
        bodyLen: document.body.innerHTML.length,
        textSample: t.slice(0, 240),
        splashVisible: !!(cs && cs.display !== 'none' && Number(cs.opacity || 1) > 0.05),
        loadingText: (document.getElementById('loading-status') || {}).textContent || '',
      })
    })()`)

    const shot = await cdp.send('Page.captureScreenshot', { format: 'png' })
    const shotPath = join(tmpdir(), `desktop-verify-${Date.now()}.png`)
    writeFileSync(shotPath, Buffer.from(shot.data, 'base64'))

    // 只留和应用有关的关键行，DLNA/SSDP 之类噪音不返回值里（日志文件仍完整）
    const interesting = cdp.logs.filter((l) =>
      /Web Runtime|es_pkg|resolve|下载|解压|Entry|Entry file|appRegister|error|Error|失败/.test(l)
    )
    ws.close()
    return { probe, errors: cdp.errors, logs: interesting.slice(-25), shotPath, url }
  } finally {
    try {
      child.kill('SIGKILL')
    } catch {}
  }
}

/* ---------------- 工具实现 ---------------- */

async function toolVerify(args) {
  const pkg = args.pkg || ''
  const zipUrl = args.zipUrl || ''
  if (!pkg && !zipUrl) throw new Error('pkg 或 zipUrl 至少要给一个')
  const port = await ensureServe(Number(args.port || 1488))
  const qs = pkg ? `es_pkg=${encodeURIComponent(pkg)}` : `zipUrl=${encodeURIComponent(zipUrl)}`
  const url = `http://127.0.0.1:${port}/?${qs}&resolver=${encodeURIComponent(args.resolver || RESOLVER)}`
  const r = await loadPage(url, Number(args.waitMs || 26000))
  const probe = JSON.parse(r.probe || '{}')
  const canvasCount = Array.isArray(probe.canvases) ? probe.canvases.length : Number(probe.canvases || 0)
  const ok = r.errors.length === 0 && canvasCount > 0
  return {
    ok,
    url: r.url,
    probe,
    screenshot: r.shotPath,
    errors: r.errors.slice(0, 10),
    logs: r.logs,
  }
}

async function toolCatalog(args) {
  const base = (args.base || RESOLVER).replace(/\/+$/, '')
  const r = await fetch(`${base}/api/runtime/catalog`)
  if (!r.ok) throw new Error(`catalog HTTP ${r.status}`)
  const j = await r.json()
  return { base, count: (j.data || []).length, apps: j.data || [] }
}

async function toolScreenshot(args) {
  if (!args.url) throw new Error('缺少 url')
  const r = await loadPage(args.url, Number(args.waitMs || 12000))
  return { url: args.url, screenshot: r.shotPath, probe: JSON.parse(r.probe || '{}'), errors: r.errors.slice(0, 5) }
}

async function toolLog(args) {
  const p = args.path || '/tmp/desk.log'
  if (!existsSync(p)) return { path: p, lines: [], note: '文件不存在（桌面端可能没启动过或没重定向日志）' }
  const all = readFileSync(p, 'utf8').split('\n')
  const tail = all.slice(-Number(args.lines || 40))
  return {
    path: p,
    total: all.length,
    // DLNA SSDP 广播会刷屏，默认滤掉，只留和应用/服务相关的
    lines: tail.filter((l) => l.trim() && !/dlna_ssdp|NOTIFY \* HTTP/.test(l)),
  }
}

const TOOLS = [
  {
    name: 'desktop_verify',
    description:
      '无头载入一个快应用包并断言（不打开任何窗口）。走真实链路：web-runtime/dist + ?es_pkg= → 后台 resolve → 下载解密 → 解压 → 渲染。返回探针数据、JS 错误、截图路径。',
    inputSchema: {
      type: 'object',
      properties: {
        pkg: { type: 'string', description: '快应用包名，如 es.com.messageboard.tv' },
        zipUrl: { type: 'string', description: '本地或远程 zip 地址（与 pkg 二选一）' },
        waitMs: { type: 'number', description: '渲染等待毫秒，默认 26000' },
        port: { type: 'number', description: '静态服务端口，默认 1488（没起会自动拉起）' },
        resolver: { type: 'string', description: '后台基址，默认 https://www.chenddcoder.cn' },
      },
    },
  },
  {
    name: 'desktop_catalog',
    description: '读后台应用目录（/api/runtime/catalog），确认某个包是否已登记、版本与图标 URL。',
    inputSchema: {
      type: 'object',
      properties: { base: { type: 'string', description: '后台基址，默认 https://www.chenddcoder.cn' } },
    },
  },
  {
    name: 'desktop_screenshot',
    description: '对任意 URL 无头截图（1920×1080），用于看页面视觉是否正常。',
    inputSchema: {
      type: 'object',
      properties: {
        url: { type: 'string', description: '要截图的地址' },
        waitMs: { type: 'number', description: '等待毫秒，默认 12000' },
      },
      required: ['url'],
    },
  },
  {
    name: 'desktop_log',
    description: '读桌面端运行日志尾部（默认 /tmp/desk.log），自动滤掉 DLNA SSDP 广播噪音。',
    inputSchema: {
      type: 'object',
      properties: {
        path: { type: 'string', description: '日志路径，默认 /tmp/desk.log' },
        lines: { type: 'number', description: '取末尾多少行，默认 40' },
      },
    },
  },
]

/* ---------------- MCP stdio 协议 ---------------- */

function send(msg) {
  process.stdout.write(JSON.stringify(msg) + '\n')
}

async function handle(msg) {
  const { id, method, params } = msg
  if (method === 'initialize') {
    return send({
      jsonrpc: '2.0',
      id,
      result: {
        protocolVersion: '2024-11-05',
        capabilities: { tools: {} },
        serverInfo: { name: 'desktop-verify', version: '0.1.0' },
      },
    })
  }
  if (method === 'notifications/initialized' || method === 'notifications/cancelled') return
  if (method === 'tools/list') {
    return send({ jsonrpc: '2.0', id, result: { tools: TOOLS } })
  }
  if (method === 'tools/call') {
    const name = params?.name
    const args = params?.arguments || {}
    try {
      let out
      if (name === 'desktop_verify') out = await toolVerify(args)
      else if (name === 'desktop_catalog') out = await toolCatalog(args)
      else if (name === 'desktop_screenshot') out = await toolScreenshot(args)
      else if (name === 'desktop_log') out = await toolLog(args)
      else throw new Error('未知工具：' + name)
      return send({
        jsonrpc: '2.0',
        id,
        result: { content: [{ type: 'text', text: JSON.stringify(out, null, 2) }] },
      })
    } catch (e) {
      return send({
        jsonrpc: '2.0',
        id,
        result: { content: [{ type: 'text', text: '✗ ' + e.message }], isError: true },
      })
    }
  }
  if (id != null) {
    send({ jsonrpc: '2.0', id, error: { code: -32601, message: 'Method not found: ' + method } })
  }
}

/* ---------------- 入口 ---------------- */

if (process.argv.includes('--selftest')) {
  // 不经过 MCP，直接跑一次验证，方便命令行排查
  const pkg = process.argv.find((a) => a.startsWith('--pkg='))?.slice(6) || ''
  const zipUrl = process.argv.find((a) => a.startsWith('--zipUrl='))?.slice(9) || ''
  const waitMs = Number(process.argv.find((a) => a.startsWith('--waitMs='))?.slice(9) || 0)
  toolVerify({ ...(zipUrl ? { zipUrl } : { pkg: pkg || 'es.com.messageboard.tv' }), ...(waitMs ? { waitMs } : {}) })
    .then((r) => {
      console.log(JSON.stringify(r, null, 2))
      process.exit(r.ok ? 0 : 1)
    })
    .catch((e) => {
      console.error('✗', e.message)
      process.exit(1)
    })
} else {
  let buf = ''
  process.stdin.setEncoding('utf8')
  process.stdin.on('data', (chunk) => {
    buf += chunk
    let idx
    while ((idx = buf.indexOf('\n')) >= 0) {
      const line = buf.slice(0, idx).trim()
      buf = buf.slice(idx + 1)
      if (!line) continue
      let msg
      try {
        msg = JSON.parse(line)
      } catch {
        continue
      }
      handle(msg).catch((e) => {
        if (msg && msg.id != null) {
          send({ jsonrpc: '2.0', id: msg.id, error: { code: -32603, message: String(e && e.message) } })
        }
      })
    }
  })
  process.on('exit', () => {
    try {
      if (serveProc) serveProc.kill('SIGKILL')
    } catch {}
  })
}
