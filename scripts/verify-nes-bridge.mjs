/**
 * 桌面端验收桥（Node 侧）—— 与 dev 无关，只在验收时被 verify-web.mjs 加载。
 *
 * `verify-web.mjs --bridge scripts/verify-nes-bridge.mjs` 会把它当"Rust 后端"：
 * 页面里的 `window.__TAURI__.core.invoke(cmd, args)` → CDP binding → 这里。
 *
 * 实现与 `src-tauri/src/webdav.rs` **同一套口径**：
 *   · 默认端口 5990，被占顺延（对应 Rust 的 `PREFERRED_PORT..+40`）
 *   · URL 路径直接映射到落盘根下（`/es_files/files/a.nes` → `<root>/es_files/files/a.nes`）
 *   · PUT 成功 → emit `webdav://upload`（页面据此跳进游戏）
 *   · `es_read_file_base64` 只允许读落盘根下的文件
 *
 * ⚠️ 这里不是"再造一个后端"，而是把 Rust 的行为在 Node 里复刻一份，
 *    为了让 headless 验收能真的跑通「起端口 → 收字节 → 落盘 → 读回」。
 *    协议层与 Rust 一一对应，任何一边改了另一个必须同步。
 */
import { createServer } from 'node:http'
import { createSocket } from 'node:dgram'
import { mkdirSync, writeFileSync, readFileSync, readdirSync, statSync, rmSync, unlinkSync } from 'node:fs'
import { networkInterfaces, tmpdir } from 'node:os'
import path from 'node:path'

const ROOT = path.join(tmpdir(), 'es-desktop-verify', 'es_files')
const PREFERRED_PORT = 5990
/** 手机手柄 UDP 端口（与 `src-tauri/src/pad.rs`、手机端 `channel.js` 同一常量） */
const PAD_PORT = 5991

let emit = async () => {}
let server = null
let boundPort = 0
let boundUrl = ''

/**
 * 流式下载会话（对应 Rust 的 `http_download_open/read/close`）。
 *
 * ⛔ 必须实现：桌面端 es_pkg 拉包走的是 `tauriEnv.tauriDownload()` → invoke
 *    http_download_open，而不是普通 fetch。少这几个命令，页面直接报
 *    「未知命令: http_download_open」，连 bundle 都进不去。
 *    分块按 base64 字符串回传 —— tauriEnv 已兼容这个形态（见其 read 分支）。
 */
const dlSessions = new Map()
let dlSeq = 0
const DL_SIZE_CAP = 8 * 1024 * 1024

export function setEmit(fn) {
  emit = fn
}

function lanIp() {
  const ifs = networkInterfaces()
  const skip = /^(lo|utun|awdl|llw|bridge|gif|stf|ipsec|tun|tap|vmenet|vmnet|anpi|pdp_ip)/
  for (const [name, addrs] of Object.entries(ifs)) {
    if (skip.test(name)) continue
    for (const a of addrs || []) {
      if (a.family === 'IPv4' && !a.internal && !a.address.startsWith('169.254.')) {
        return a.address
      }
    }
  }
  return '127.0.0.1'
}

function safeJoin(urlPath) {
  const out = [ROOT]
  for (const seg of urlPath.split('/')) {
    if (!seg || seg === '.') continue
    if (seg === '..') return null
    out.push(decodeURIComponent(seg))
  }
  return path.join(...out)
}

function xmlEscape(s) {
  return String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
}

function propfindBody(urlPath) {
  const target = safeJoin(urlPath)
  const items = []
  if (target && statSync(target, { throwIfNoEntry: false })?.isDirectory()) {
    items.push({ href: urlPath, dir: true, size: 0 })
    for (const name of readdirSync(target)) {
      const p = path.join(target, name)
      const st = statSync(p)
      items.push({
        href: (urlPath.endsWith('/') ? urlPath : urlPath + '/') + name,
        dir: st.isDirectory(),
        size: st.size,
      })
    }
  } else if (target && statSync(target, { throwIfNoEntry: false })?.isFile()) {
    items.push({ href: urlPath, dir: false, size: statSync(target).size })
  }
  let xml =
    '<?xml version="1.0" encoding="utf-8"?>\n<D:multistatus xmlns:D="DAV:">\n'
  for (const it of items) {
    xml += `<D:response><D:href>${xmlEscape(it.href)}</D:href><D:propstat><D:prop><D:resourcetype>${
      it.dir ? '<D:collection/>' : ''
    }</D:resourcetype><D:getcontentlength>${it.size}</D:getcontentlength></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>\n`
  }
  xml += '</D:multistatus>\n'
  return xml
}

const DAV_HEADERS = {
  DAV: '1,2',
  Allow: 'OPTIONS, GET, HEAD, PUT, DELETE, MKCOL, PROPFIND',
  'MS-Author-Via': 'DAV',
}

function listFiles(dir = ROOT, base = ROOT, depth = 0, out = []) {
  if (depth > 4) return out
  let names = []
  try {
    names = readdirSync(dir)
  } catch (_e) {
    return out
  }
  for (const n of names) {
    const p = path.join(dir, n)
    const st = statSync(p)
    if (st.isDirectory()) listFiles(p, base, depth + 1, out)
    else out.push(path.relative(base, p))
  }
  return out
}

async function startWebdav() {
  if (server) return { url: boundUrl, ip: lanIp(), port: boundPort, root: ROOT, running: true }
  mkdirSync(path.join(ROOT, 'es_files'), { recursive: true })

  const handler = (req, res) => {
    const urlPath = decodeURIComponent((req.url || '/').split('?')[0])
    if (urlPath === '/__info') {
      const body = JSON.stringify({
        running: !!server,
        url: boundUrl,
        port: boundPort,
        root: ROOT,
        files: listFiles(),
      })
      res.writeHead(200, { 'Content-Type': 'application/json' })
      res.end(body)
      return
    }
    const target = safeJoin(urlPath)
    if (!target) {
      res.writeHead(403)
      res.end('forbidden')
      return
    }

    if (req.method === 'OPTIONS') {
      res.writeHead(204, DAV_HEADERS)
      res.end()
      return
    }
    if (req.method === 'PUT') {
      const chunks = []
      req.on('data', (c) => chunks.push(c))
      req.on('end', () => {
        const buf = Buffer.concat(chunks)
        mkdirSync(path.dirname(target), { recursive: true })
        writeFileSync(target, buf)
        console.log(`[bridge] PUT ${urlPath} (${buf.length} B) → ${target}`)
        emit('webdav://upload', {
          path: target,
          name: path.basename(target),
          size: buf.length,
        })
        res.writeHead(201, DAV_HEADERS)
        res.end()
      })
      return
    }
    if (req.method === 'GET' || req.method === 'HEAD') {
      try {
        const b = readFileSync(target)
        res.writeHead(200, { 'Content-Type': 'application/octet-stream' })
        res.end(req.method === 'HEAD' ? undefined : b)
      } catch (_e) {
        res.writeHead(404, DAV_HEADERS)
        res.end()
      }
      return
    }
    if (req.method === 'DELETE') {
      let ok = false
      try {
        const st = statSync(target)
        if (st.isDirectory()) rmSync(target, { recursive: true, force: true })
        else unlinkSync(target)
        ok = true
      } catch (_e) {
        ok = false
      }
      res.writeHead(ok ? 204 : 404, DAV_HEADERS)
      res.end()
      return
    }
    if (req.method === 'MKCOL') {
      mkdirSync(target, { recursive: true })
      res.writeHead(201, DAV_HEADERS)
      res.end()
      return
    }
    if (req.method === 'PROPFIND') {
      const xml = propfindBody(urlPath)
      res.writeHead(207, { ...DAV_HEADERS, 'Content-Type': 'application/xml; charset=utf-8' })
      res.end(xml)
      return
    }
    res.writeHead(405, DAV_HEADERS)
    res.end('method not allowed')
  }

  const tryListen = (port) =>
    new Promise((resolve, reject) => {
      const s = createServer(handler)
      s.once('error', reject)
      s.listen(port, '0.0.0.0', () => resolve(s))
    })

  let listener = null
  for (let p = PREFERRED_PORT; p < PREFERRED_PORT + 40; p += 1) {
    try {
      listener = await tryListen(p)
      boundPort = p
      break
    } catch (_e) {
      // 端口被占，顺延
    }
  }
  if (!listener) throw new Error('没有可用端口（5990-6029 全被占）')
  server = listener
  boundUrl = `http://${lanIp()}:${boundPort}`
  console.log(`[bridge] WebDAV 已起: ${boundUrl}  root=${ROOT}`)
  return { url: boundUrl, ip: lanIp(), port: boundPort, root: ROOT, running: true }
}

async function stopWebdav() {
  if (!server) return false
  await new Promise((r) => server.close(r))
  server = null
  boundUrl = ''
  boundPort = 0
  return true
}

/**
 * 模拟"手机那一侧"投一个卡带上来（scenario 的 `{ "bridge": "putRom" }` 用）。
 *
 * 走的是真 HTTP PUT（真端口、真 Content-Length、真落盘），不是直接改内存状态 ——
 * 这样验的是「服务端能不能收」而不是「我能不能把数据塞进去」。
 */
export async function putRom(opts = {}) {
  if (!server) await startWebdav()
  const name = opts.name || 'g2048.nes'
  const file = opts.file
    ? path.resolve(opts.file)
    : path.resolve(import.meta.dirname, '..', '..', 'EsApp', 'esapp-nes', 'verify', 'g2048.nes')
  const body = readFileSync(file)
  const url = `${boundUrl}/es_files/files/${name}`
  const res = await fetch(url, { method: 'PUT', body })
  return { status: res.status, size: body.length, url }
}

/**
 * 模拟手机手柄发一个包（scenario 的 `{ "bridge": "putPad" }` 用）。
 *
 * 与真机同路径：PUT 到 `es_files/files/pad/pad_<token>.json`，
 * 页面靠 `upload_success` 感知后用 `es_read_file_base64` 读回解析。
 */
export async function putPad(opts = {}) {
  if (!server) await startWebdav()
  const name = opts.name || 'pad_verify.json'
  const packet = opts.packet || { v: 1, seq: 1, t: Date.now(), b: [0, 0, 0, 0, 1, 0, 0, 0] }
  const body = Buffer.from(JSON.stringify(packet))
  const url = `${boundUrl}/es_files/files/pad/${name}`
  const res = await fetch(url, { method: 'PUT', body })
  return { status: res.status, size: body.length, url }
}

export async function listSaved() {
  return listFiles()
}

/* ---------------- 手机手柄 UDP 5991（复刻 `src-tauri/src/pad.rs`） ---------------- */

/**
 * 与 `pad.rs` 同一套口径：
 *   · 手机 → 电视 `seq,mask,t`；电视 → 手机 `ack,seq`；`ping` 回 `ack,-1`
 *   · 只在位图**变化**时推 `pad://input`（手机松手后 20Hz 发中立包，逐包推没意义）
 *   · 判活靠 `lastRecvAgoMs`，不是「recv 有没有增量」
 *
 * ⚠️ 这不是"再造一个后端"：桌面壳里跑的是 Rust 那份，这里复刻是为了让 headless
 *    验收能真的走一遍「手机发包 → 电视回执 → 位图进模拟器」。
 */
const pad = {
  sock: null,
  port: 0,
  recv: 0,
  applied: 0,
  seqGap: 0,
  dropped: 0,
  lastSeq: -1,
  lastRecvAt: 0,
  mask: 0,
  target: '',
  error: '',
}

/** 位图 → [上,下,左,右,A,B,SEL,STA]，与 `pad-core.ts` 的 `b[]` 同序 */
function bitsOf(mask) {
  return [
    (mask >> 4) & 1,
    (mask >> 5) & 1,
    (mask >> 6) & 1,
    (mask >> 7) & 1,
    mask & 1,
    (mask >> 1) & 1,
    (mask >> 2) & 1,
    (mask >> 3) & 1,
  ]
}

function padInfo() {
  return { ok: !!pad.sock, port: pad.port, target: pad.target, error: pad.error }
}

function onPadPacket(buf, rinfo) {
  const text = buf.toString('utf8').trim()
  if (!text) return
  if (text.toLowerCase() === 'ping') {
    try {
      pad.sock.send('ack,-1', rinfo.port, rinfo.address)
    } catch (_e) {
      /* 对端已走 */
    }
    return
  }
  const m = /^(-?\d+)\s*,\s*(\d+)\s*(?:,\s*-?\d+)?$/.exec(text)
  if (!m) {
    pad.dropped += 1
    return
  }
  const seq = Number(m[1])
  const mask = Number(m[2]) >>> 0
  try {
    pad.sock.send('ack,' + seq, rinfo.port, rinfo.address)
  } catch (_e) {
    /* ignore */
  }
  pad.recv += 1
  pad.lastRecvAt = Date.now()
  if (pad.lastSeq >= 0 && seq > pad.lastSeq) {
    const jump = seq - pad.lastSeq - 1
    if (jump > 0 && jump < 1000) pad.seqGap += jump
  }
  pad.lastSeq = seq
  const changed = pad.mask !== mask
  pad.mask = mask
  if (changed) pad.applied += 1
  if (changed) {
    console.log(`[bridge] pad mask=${mask} bits=${bitsOf(mask).join('')} seq=${seq}`)
    void emit('pad://input', { mask, bits: bitsOf(mask), seq })
  }
}

async function startPad() {
  if (pad.sock) return padInfo()
  for (let p = PAD_PORT; p < PAD_PORT + 10; p += 1) {
    const sock = createSocket('udp4')
    const ok = await new Promise((resolve) => {
      sock.once('error', () => resolve(false))
      sock.bind(p, '0.0.0.0', () => resolve(true))
    })
    if (!ok) {
      try {
        sock.close()
      } catch (_e) {
        /* ignore */
      }
      continue
    }
    pad.sock = sock
    pad.port = p
    sock.on('message', onPadPacket)
    console.log(`[bridge] 手柄 UDP 已起: 0.0.0.0:${p}`)
    return padInfo()
  }
  pad.error = '没有可用端口（5991-6000 全被占）'
  return padInfo()
}

/**
 * 模拟手机按一下（scenario 的 `{ "bridge": "putPadUdp", "arg": { "mask": 8 } }`）。
 *
 * @param {{mask?:number, seq?:number, hold?:number}} opts
 *   `hold`（ms）> 0 时先按住再松开，让页面有机会观察到「按下 → 抬起」两个状态。
 */
export async function putPadUdp(opts = {}) {
  if (!pad.sock) await startPad()
  if (!pad.sock) throw new Error(pad.error || '手柄端口没起来')
  const mask = Number(opts.mask) || 0
  const seq = Number(opts.seq) || pad.lastSeq + 1 || 1
  const hold = Math.max(0, Number(opts.hold) || 0)

  const client = createSocket('udp4')
  const acks = []
  client.on('message', (b) => acks.push(b.toString('utf8').trim()))
  /** ⛔ 包体是 `seq,mask,t`（**序号在前**）—— 写反了电视会把 mask 当序号回执，按键全错位 */
  const send = (s, m) =>
    new Promise((resolve) => {
      client.send(s + ',' + m + ',' + Date.now(), pad.port, '127.0.0.1', () => resolve())
    })

  await send(seq, mask)
  await new Promise((r) => setTimeout(r, 120))
  if (hold > 0) {
    await new Promise((r) => setTimeout(r, hold))
    await send(seq + 1, 0)
    await new Promise((r) => setTimeout(r, 120))
  }
  try {
    client.close()
  } catch (_e) {
    /* ignore */
  }
  return { port: pad.port, mask, seq, acks, recv: pad.recv, applied: pad.applied }
}

export function padStats() {
  return {
    running: !!pad.sock,
    port: pad.port,
    recv: pad.recv,
    applied: pad.applied,
    seqGap: pad.seqGap,
    dropped: pad.dropped,
    lastRecvAgoMs: pad.lastRecvAt ? Date.now() - pad.lastRecvAt : 0,
    mask: pad.mask,
    target: pad.target,
    error: pad.error,
  }
}

export async function invoke(cmd, args) {
  switch (cmd) {
    case 'proxy_http': {
      // 与 desktop-runtime/src-tauri/src/proxy.rs 同契约：{req:{url,method,headers,body_base64}}
      const req = (args && args.req) || {}
      const init = { method: req.method || 'GET', headers: req.headers || {} }
      if (req.body_base64) init.body = Buffer.from(req.body_base64, 'base64')
      try {
        const resp = await fetch(req.url, init)
        const buf = Buffer.from(await resp.arrayBuffer())
        const headers = {}
        resp.headers.forEach((v, k) => {
          headers[k] = v
        })
        return JSON.stringify({
          status: resp.status,
          headers,
          bodyBase64: buf.toString('base64'),
        })
      } catch (e) {
        return JSON.stringify({
          status: 502,
          headers: {},
          bodyBase64: Buffer.from(String((e && e.message) || e)).toString('base64'),
        })
      }
    }
    case 'http_download_open': {
      const url = String((args && args.url) || '')
      const headers = (args && args.headers) || {}
      const resp = await fetch(url, { headers })
      const buf = Buffer.from(await resp.arrayBuffer())
      const sessionId = 'dl-' + (++dlSeq).toString(16).padStart(16, '0')
      dlSessions.set(sessionId, { buf, off: 0 })
      const hdrs = {}
      resp.headers.forEach((v, k) => {
        hdrs[k] = v
      })
      return { sessionId, status: resp.status, headers: hdrs, totalBytes: buf.length }
    }
    case 'http_download_read': {
      const sid = String((args && args.sessionId) || '')
      const s = dlSessions.get(sid)
      // 会话已结束/已关闭 → 返回空串，前端按 byteLength===0 收尾（与 Rust 幂等语义一致）
      if (!s) return ''
      const size = Math.min(Number((args && args.size) || 1048576) || 1048576, DL_SIZE_CAP)
      const chunk = s.buf.subarray(s.off, s.off + size)
      s.off += chunk.length
      if (s.off >= s.buf.length) dlSessions.delete(sid)
      return chunk.toString('base64')
    }
    case 'http_download_close': {
      dlSessions.delete(String((args && args.sessionId) || ''))
      return null
    }
    case 'webdav_start':
      return startWebdav()
    case 'webdav_info':
      return {
        url: boundUrl,
        ip: boundUrl ? lanIp() : '',
        port: boundPort,
        root: ROOT,
        running: !!server,
      }
    case 'webdav_stop':
      return stopWebdav()
    case 'es_read_file_base64': {
      const p = path.resolve(String((args && args.path) || ''))
      if (!p.startsWith(ROOT)) throw new Error('拒绝读取落盘目录之外的文件')
      return readFileSync(p).toString('base64')
    }
    case 'es_list_files':
      return listFiles()
    case 'pad_start':
      return startPad()
    case 'pad_stats':
      return padStats()
    case 'pad_target':
      pad.target = String((args && args.target) || '')
      return true
    case 'pad_stop': {
      const was = !!pad.sock
      if (pad.sock) {
        try {
          pad.sock.close()
        } catch (_e) {
          /* 已关 */
        }
      }
      pad.sock = null
      pad.port = 0
      pad.mask = 0
      return was
    }
    case 'pad_reset':
      pad.recv = 0
      pad.applied = 0
      pad.seqGap = 0
      pad.dropped = 0
      pad.lastSeq = -1
      pad.mask = 0
      pad.lastRecvAt = 0
      return true
    default:
      throw new Error('未知命令: ' + cmd)
  }
}
