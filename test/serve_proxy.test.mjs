// serve.mjs /proxy 转发的端到端测试
//
// 真起一个 mock upstream HTTP server（A 端口）和 serve.mjs（B 端口），
// 然后从 node:test 里用 fetch 走 serve.mjs/proxy?url=http://localhost:A/...，
// 验证 method/body/headers 转发、CORS 头、SSRF 防护、OPTIONS preflight。

import { test } from 'node:test'
import assert from 'node:assert/strict'
import { createServer } from 'node:http'
import { spawn } from 'node:child_process'
import fs from 'node:fs'
import { fileURLToPath } from 'node:url'
import path from 'node:path'

const __dirname = path.dirname(fileURLToPath(import.meta.url))
const SERVE_SCRIPT = path.resolve(__dirname, '../scripts/serve.mjs')
// 测试用 dist：随便指个空目录即可（这次测试只关心 /proxy；不会触发 static-fallback）
const FAKE_DIST = path.resolve(__dirname, 'fixtures', 'fake-dist')

// 起 mock upstream
function startUpstream() {
  return new Promise((resolve) => {
    const received = []
    const srv = createServer((req, res) => {
      let body = ''
      req.on('data', (c) => (body += c.toString()))
      req.on('end', () => {
        received.push({
          method: req.method,
          url: req.url,
          headers: { ...req.headers },
          body,
        })
        if (req.url === '/json') {
          res.writeHead(200, { 'Content-Type': 'application/json' })
          res.end(JSON.stringify({ ok: true, bodyReceived: body, method: req.method }))
        } else if (req.url === '/echo-headers') {
          res.writeHead(200, { 'Content-Type': 'application/json' })
          res.end(JSON.stringify({ forwarded: req.headers }))
        } else if (req.url === '/png') {
          // 1x1 PNG
          res.writeHead(200, { 'Content-Type': 'image/png' })
          res.end(Buffer.from([
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a,
            0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
            0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01,
            0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
            0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41,
            0x54, 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00,
            0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae,
            0x42, 0x60, 0x82,
          ]))
        } else {
          res.writeHead(404).end('not-found')
        }
      })
    })
    srv.listen(0, '127.0.0.1', () => {
      srv.received = received
      resolve(srv)
    })
  })
}

// 起 serve.mjs 子进程
// 传端口 0 让 OS 分配空闲端口，避免并发测试间 EADDRINUSE；
// 从 stdout 里解析 `localhost:NNN` 取得真实端口。
function startServe(envExtra = {}) {
  return new Promise((resolve, reject) => {
    const proc = spawn('node', [SERVE_SCRIPT, '0', FAKE_DIST], {
      stdio: ['ignore', 'pipe', 'pipe'],
      env: { ...process.env, ...envExtra },
    })
    let out = ''
    let err = ''
    let settled = false
    const timer = setTimeout(() => {
      if (!settled) reject(new Error('serve.mjs startup timeout; out=' + out.slice(0, 300) + ' err=' + err))
    }, 5000)
    proc.stdout.on('data', (c) => {
      out += c.toString()
      if (!settled && out.includes('[serve]')) {
        const m = out.match(/localhost:(\d+)/)
        if (!m) {
          settled = true
          clearTimeout(timer)
          reject(new Error('cannot parse port from serve.mjs output: ' + out.slice(0, 200)))
          return
        }
        settled = true
        clearTimeout(timer)
        resolve({ proc, port: Number(m[1]) })
      }
    })
    proc.stderr.on('data', (c) => (err += c.toString()))
    proc.on('exit', (code) => {
      if (!settled) {
        settled = true
        clearTimeout(timer)
        reject(new Error('serve.mjs exited early code=' + code + ' stderr=' + err))
      }
    })
  })
}

function stopServe(p) {
  return new Promise((resolve) => {
    if (p.proc.exitCode !== null || p.proc.signalCode !== null) { resolve(); return }
    let done = false
    const finish = () => { if (!done) { done = true; resolve() } }
    p.proc.on('exit', finish)
    try { p.proc.kill() } catch (_) {}
    setTimeout(finish, 800)
  })
}

test('GET /proxy?url=... 转发到上游 method/url/headers（origin 头被剥）', async (t) => {
  const up = await startUpstream()
  t.after(() => new Promise((r) => up.close(r)))
  const upPort = up.address().port
  const sv = await startServe({ ALLOW_PRIVATE_HOSTS: 'true' })
  t.after(() => stopServe(sv))

  const upstreamBase = `http://127.0.0.1:${upPort}`
  const res = await fetch(`http://127.0.0.1:${sv.port}/proxy?url=${encodeURIComponent(upstreamBase + '/echo-headers')}`)
  assert.equal(res.status, 200)
  const json = await res.json()
  assert.equal(res.headers.get('access-control-allow-origin'), '*')
  // forwarded headers 中不应含 origin/referer（被剥离）
  assert.equal(json.forwarded.origin, undefined)
  assert.equal(json.forwarded.referer, undefined)
  // 上游应该看到 1 个请求
  assert.ok(up.received.length >= 1, 'upstream received ≥1 request')
  const recv = up.received[0]
  assert.equal(recv.method, 'GET')
  assert.equal(recv.url, '/echo-headers')
})

test('POST /proxy 转发 method + body + Content-Type', async (t) => {
  const up = await startUpstream()
  t.after(() => new Promise((r) => up.close(r)))
  const upPort = up.address().port
  const sv = await startServe({ ALLOW_PRIVATE_HOSTS: 'true' })
  t.after(() => stopServe(sv))

  const payload = 'encrypted-blob-data-abc-123'
  const res = await fetch(
    `http://127.0.0.1:${sv.port}/proxy?url=${encodeURIComponent(`http://127.0.0.1:${upPort}/json`)}`,
    {
      method: 'POST',
      headers: { 'Content-Type': 'application/octet-stream', 'X-Custom': 'hi' },
      body: payload,
    }
  )
  assert.equal(res.status, 200)
  const json = await res.json()
  assert.equal(json.ok, true)
  assert.equal(json.method, 'POST')
  assert.equal(json.bodyReceived, payload)
  // 上游应该收到完整 body 与自定义请求头
  const recv = up.received[0]
  assert.equal(recv.headers['x-custom'], 'hi')
  assert.equal(recv.body, payload)
})

test('GET /proxy 返回图片二进制（PNG 字节流透传）', async (t) => {
  const up = await startUpstream()
  t.after(() => new Promise((r) => up.close(r)))
  const upPort = up.address().port
  const sv = await startServe({ ALLOW_PRIVATE_HOSTS: 'true' })
  t.after(() => stopServe(sv))

  const res = await fetch(
    `http://127.0.0.1:${sv.port}/proxy?url=${encodeURIComponent(`http://127.0.0.1:${upPort}/png`)}`
  )
  assert.equal(res.status, 200)
  assert.equal(res.headers.get('content-type'), 'image/png')
  const buf = new Uint8Array(await res.arrayBuffer())
  // PNG magic 头 8 字节校验
  assert.equal(buf[0], 0x89)
  assert.equal(buf[1], 0x50)
  assert.equal(buf[2], 0x4e)
  assert.equal(buf[3], 0x47)
})

test('OPTIONS /proxy 走 preflight：CORS 头全 + 204', async (t) => {
  const sv = await startServe({ ALLOW_PRIVATE_HOSTS: 'true' })
  t.after(() => stopServe(sv))
  const res = await fetch(`http://127.0.0.1:${sv.port}/proxy?url=foo`, {
    method: 'OPTIONS',
    headers: {
      'Access-Control-Request-Method': 'POST',
      'Access-Control-Request-Headers': 'content-type, x-custom',
    },
  })
  assert.equal(res.status, 204)
  assert.equal(res.headers.get('access-control-allow-origin'), '*')
  assert.match(res.headers.get('access-control-allow-headers') || '', /content-type/i)
})

test('SSRF 防护：禁止代理到 localhost / 127.0.0.1 / 私有 IP（无 ALLOW_PRIVATE_HOSTS）', async (t) => {
  const sv = await startServe({})
  t.after(() => stopServe(sv))
  const baseUrl = `http://127.0.0.1:${sv.port}/proxy`
  const cases = [
    'http://localhost:9999/x',
    'http://127.0.0.1:9999/x',
    'http://10.0.0.5/x',
    'http://192.168.1.5/x',
    'http://172.16.0.5/x',
  ]
  for (const target of cases) {
    const res = await fetch(`${baseUrl}?url=${encodeURIComponent(target)}`)
    assert.equal(res.status, 403, `expected 403 for ${target}, got ${res.status}`)
  }
})

test('缺失 url 查询参数返回 400', async (t) => {
  const sv = await startServe({ ALLOW_PRIVATE_HOSTS: 'true' })
  t.after(() => stopServe(sv))
  const res = await fetch(`http://127.0.0.1:${sv.port}/proxy`)
  assert.equal(res.status, 400)
})

test('non-http(s) 协议返回 400', async (t) => {
  const sv = await startServe({ ALLOW_PRIVATE_HOSTS: 'true' })
  t.after(() => stopServe(sv))
  const res = await fetch(`http://127.0.0.1:${sv.port}/proxy?url=file:///etc/passwd`)
  assert.equal(res.status, 400)
})

test('SPA fallback：非白名单路径仍然回退到 index.html（不破坏原有逻辑）', async (t) => {
  // 真用 web-runtime/dist，确保 fallback 路径不变
  const REAL_DIST = path.resolve(__dirname, '../../quicktvui/packages/web-runtime/dist')
  if (!fs.existsSync(REAL_DIST)) return // 跳过：当 web-runtime/dist 不存在时
  const proc = await new Promise((resolve, reject) => {
    const p = spawn('node', [SERVE_SCRIPT, '0', REAL_DIST], {
      stdio: ['ignore', 'pipe', 'pipe'],
      env: { ...process.env, ALLOW_PRIVATE_HOSTS: 'true' },
    })
    let out = ''
    p.stdout.on('data', (c) => {
      out += c.toString()
      if (out.includes('[serve]')) {
        const m = out.match(/localhost:(\d+)/)
        if (m) resolve({ proc: p, port: Number(m[1]) })
      }
    })
    setTimeout(() => reject(new Error('timeout')), 5000)
  })
  t.after(() => stopServe(proc))
  const res = await fetch(`http://127.0.0.1:${proc.port}/some-nonexistent-path`)
  assert.equal(res.status, 200)
  const text = await res.text()
  assert.match(text, /<!doctype html>|web-runtime/i)
})
