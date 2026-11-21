/**
 * 大屏便签（`esapp-livingroom`）桌面适配验收 —— CDP 起真 web-runtime + `__TAURI__` 桩。
 *
 * ## 为什么需要一个"假 Rust"
 *
 * 桌面端跟浏览器的差别**不在渲染**，在四条 Tauri 命令：`webdav_start` / `webdav_info` /
 * `es_list_files` / `es_read_file_base64`。而原先的代码是靠本机 WebDAV 的 HTTP 拿数据的，
 * 在桌面壳里那条路被 CORS 拦死（详见 `core/platform.ts` 顶部）。
 *
 * 这个脚本把「Rust 那一侧」用 Node 实现出来（一个临时目录当落盘根），
 * 页面里注入 `__TAURI__` 桩把 invoke 转回 Node —— 于是：
 *   · 走的是**真的** `web-renderer`（EsWebdavServerModule 的真实现）、真的应用包；
 *   · 只有最外面那一层 Rust 被替换，接口字段与真实 `webdav.rs` 一一对齐。
 *
 * ## 验的是哪七件事
 *   ① 服务起来（`phase === 'ready'`、地址/端口/落盘根都对）
 *   ② 开机把落盘目录里的历史便签读回对话流（`es_list_files`）
 *   ③ 收到 `webdav://upload` → 便签进对话流（真模块 → `OnWebdavEvent` → 接收层）
 *   ④ 一句话便签的正文能读出来（`es_read_file_base64` + UTF-8）
 *   ⑤ CSV 便签打开成表格
 *   ⑥ 图片便签打开成 `<img src="data:image/png;base64,…">`
 *   ⑦ 桌面返回键：阅读页回首页、首页回启动器（`es_pkg=cn.chenddcoder.tvcast`）
 *
 * ## 前置
 *   # 1) 把 web-runtime 产物服务起来（另开一个进程）
 *   node desktop-runtime/scripts/serve.mjs 1489
 *   # 2) 把**当前构建**打成一个 zip 放进 web-runtime/dist（产物变了要重打）
 *   python3 -c "import zipfile,os;src='EsApp/esapp-livingroom/dist/android';z=zipfile.ZipFile('quicktvui/packages/web-runtime/dist/lr.zip','w',zipfile.ZIP_DEFLATED);[z.write(os.path.join(src,f),f) for f in os.listdir(src) if f.endswith('.js')];z.close()"
 *
 * ## 跑
 *   node desktop-runtime/scripts/verify-livingroom-desktop.mjs
 *   SHOT=/tmp/lr.png node …            # 另外存一张截图
 */
import { spawn } from 'node:child_process'
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, readdirSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

const CHROME =
  process.env.CHROME ||
  '/Users/chendd/Library/Caches/ms-playwright/chromium_headless_shell-1217/chrome-headless-shell-mac-arm64/chrome-headless-shell'
const CDP_PORT = Number(process.env.CDP_PORT || 9336)
const PAGE_PORT = Number(process.env.PAGE_PORT || 1489)
const SHOT_PATH = process.env.SHOT || ''
/** 版面（思维导图）那一屏的截图：单独一个路径，避免被最后的首页态覆盖 */
const BOARD_SHOT = process.env.BOARD_SHOT || ''
const PAGE_URL = `http://127.0.0.1:${PAGE_PORT}/?zipUrl=/lr.zip`

const sleep = (ms) => new Promise((r) => setTimeout(r, ms))

/* ------------------------------------------------------------ 假落盘根 */
const TMP = mkdtempSync(join(tmpdir(), 'lr-desktop-'))
/** 与 Rust `default_root()` 对齐：`<appData>/es_files`，PUT 路径原样映射在它下面 */
const ROOT = join(TMP, 'es_files')
/** 真实落盘位置：`<root>/es_files/files/livingroom`（多一层 es_files 是 Rust 的既定行为） */
const DATA = join(ROOT, 'es_files', 'files', 'livingroom')
mkdirSync(DATA, { recursive: true })

/** 等价于 Rust `es_list_files`：落盘根下所有文件的绝对路径 */
function listAll(dir) {
  const out = []
  const rec = (d, depth) => {
    if (depth > 4) return
    let ents = []
    try {
      ents = readdirSync(d, { withFileTypes: true })
    } catch (_e) {
      return
    }
    for (const e of ents) {
      const p = join(d, e.name)
      if (e.isDirectory()) rec(p, depth + 1)
      else out.push(p)
    }
  }
  rec(dir, 0)
  return out
}

/* ------------------------------------------------------------ CDP */
const prof = mkdtempSync(join(tmpdir(), 'lr-cdp-'))
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
  for (let i = 0; i < 80; i++) {
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
  if (r.exceptionDetails) return { __error: r.exceptionDetails.text || r.exceptionDetails.exception?.description }
  return r.result?.value
}

/** 按一个遥控器键（走页面 keydown，与桌面壳里真按键盘同一条路） */
const KEYS = {
  left: { key: 'ArrowLeft', code: 'ArrowLeft', kc: 37 },
  right: { key: 'ArrowRight', code: 'ArrowRight', kc: 39 },
  up: { key: 'ArrowUp', code: 'ArrowUp', kc: 38 },
  down: { key: 'ArrowDown', code: 'ArrowDown', kc: 40 },
  ok: { key: 'Enter', code: 'Enter', kc: 13 },
  back: { key: 'Escape', code: 'Escape', kc: 27 },
}
async function press(name, times = 1) {
  const k = KEYS[name]
  for (let i = 0; i < times; i++) {
    await send('Input.dispatchKeyEvent', {
      type: 'keyDown',
      key: k.key,
      code: k.code,
      windowsVirtualKeyCode: k.kc,
      nativeVirtualKeyCode: k.kc,
    })
    await send('Input.dispatchKeyEvent', {
      type: 'keyUp',
      key: k.key,
      code: k.code,
      windowsVirtualKeyCode: k.kc,
      nativeVirtualKeyCode: k.kc,
    })
    await sleep(220)
  }
}

await send('Runtime.enable')
await send('Page.enable')
await send('Emulation.setDeviceMetricsOverride', { width: 1920, height: 1080, deviceScaleFactor: 1, mobile: false })
await send('Runtime.addBinding', { name: '__TAURI_MSG__' })

/* ------------------------------------------------------------ 假 Rust */
let webdavStarted = false
onBinding = async (raw) => {
  let m
  try {
    m = JSON.parse(raw)
  } catch {
    return
  }
  const { id, cmd, args } = m
  let result = null
  if (cmd === 'webdav_start') {
    webdavStarted = true
    result = { running: true, url: `http://127.0.0.1:5990`, ip: '127.0.0.1', port: 5990, root: ROOT }
    console.log('[e2e] invoke webdav_start ->', JSON.stringify(result))
  } else if (cmd === 'webdav_info') {
    result = {
      running: webdavStarted,
      url: webdavStarted ? 'http://127.0.0.1:5990' : '',
      ip: webdavStarted ? '127.0.0.1' : '',
      port: webdavStarted ? 5990 : 0,
      root: ROOT,
    }
  } else if (cmd === 'webdav_stop') {
    webdavStarted = false
    result = true
  } else if (cmd === 'es_list_files') {
    result = listAll(ROOT)
  } else if (cmd === 'es_read_file_base64') {
    const p = String((args && args.path) || '')
    try {
      result = readFileSync(p).toString('base64')
    } catch (e) {
      result = null
    }
  } else if (cmd === 'endpoint_start' || cmd === 'endpoint_stop') {
    result = null
  } else {
    result = null
  }
  const lit = JSON.stringify(result)
  await evalJs(
    `(() => { const f = window.__TAURI_PENDING__[${id}]; delete window.__TAURI_PENDING__[${id}]; if (f) f(${lit}); return 1 })()`
  )
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
        try { window.__TAURI_MSG__(JSON.stringify({ id, cmd, args: args || {} })) } catch (e) { reject(e) }
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

// zip 下载 + 解压 + initServer（startServer → 300ms → 落根 → 定地址）
await sleep(9000)

const boot = await evalJs(`JSON.stringify({
  tauri: !!window.__TAURI__,
  listeners: Object.keys(window.__LISTENERS__ || {}),
  body: ((document.getElementById('root') || document.body).innerText || '').replace(/\\s+/g, ' ').slice(0, 200)
})`)
console.log('[e2e] BOOT ->', boot)

/** 等一个页面谓词为真 */
async function until(expr, ms = 15000, gap = 400) {
  const t0 = Date.now()
  while (Date.now() - t0 < ms) {
    const v = await evalJs(expr)
    if (v && !v.__error) return v
    await sleep(gap)
  }
  return null
}

/* ------------------------------------------------------------ 拿令牌 */
const token = await until(
  `(async () => { try { const v = await window.localStorage.getItem('livingroom.token'); return typeof v === 'string' && /^[A-HJ-NP-Z2-9]{6}$/.test(v) ? v : '' } catch (e) { return '' } })()`,
  8000
)
console.log('[e2e] token ->', token)
if (!token) {
  console.log('❌ 拿不到配对令牌，后面的文件名协议无从构造')
}

/* ------------------------------------------------------------ 造七张便签 */
const SAMPLES = join(process.cwd(), 'EsApp', 'esapp-livingroom', 'verify', 'samples')
const now = Date.now()
const files = [
  // 顺序 = 时间从早到晚（对话流按文件名里的时间戳升序，最后一张是最新）
  { name: `cast_${token}-${now - 7000}-陈兄~合同.pdf`, body: readFileSync(join(SAMPLES, 'pdf-test-plusminus-pdf.pdf')) },
  { name: `cast_${token}-${now - 6500}-陈妹~清单.docx`, body: readFileSync(join(SAMPLES, 'docx-tables.docx')) },
  /*
   * 思维导图版 docx：正文全在文本框里、结构靠连线 —— 走的是**版面渲染**这条路。
   * 就是 2026-11-09 陈兄传上来的那份真文件（收进 `verify/samples/` 当回归样本）。
   */
  { name: `cast_${token}-${now - 6000}-陈妹~三下思维导图.docx`, body: readFileSync(join(SAMPLES, 'docx-textbox-mindmap.docx')) },
  { name: `cast_${token}-${now - 4000}-陈兄~__say.txt`, body: '今天降温，出门穿外套' },
  { name: `cast_${token}-${now - 3000}-陈妹~周报.md`, body: '# 周报\n\n- 完成直播链路\n- 修好投屏\n' },
  {
    name: `cast_${token}-${now - 2000}-陈兄~销售明细.csv`,
    body: '月份,金额\n1月,1200\n2月,1500\n3月,900\n',
  },
  {
    name: `cast_${token}-${now - 1000}-陈妹~截图.png`,
    body: Buffer.from(
      'iVBORw0KGgoAAAANSUhEUgAAAAgAAAAICAYAAADED76LAAAAJ0lEQVR42mNk+M9Qz0AEYBxVSF+FjP8ZGBgYGEcV0lchY6hCGgIAxx0H+2iKTGkAAAAASUVORK5CYII=',
      'base64'
    ),
  },
]
for (const f of files) writeFileSync(join(DATA, f.name), f.body)

/* ② 开机对齐：先看它能不能把落盘目录里的历史读回来 */
// 应用在 initServer 之前不会列目录，等它启动完成再主动打一次同步（真实路径是"收到事件后补一刀"）
const phaseAfterBoot = await evalJs(`JSON.stringify({
  bubbles: document.querySelectorAll('[data-testid]').length,
  text: ((document.getElementById('root') || document.body).innerText || '').replace(/\\s+/g, ' ')
})`)
console.log('[e2e] 落盘后 DOM ->', String(phaseAfterBoot).slice(0, 400))

/* ③ 逐张投上来（走真模块的 webdav://upload → OnWebdavEvent） */
for (const f of files) {
  const abs = join(DATA, f.name)
  const payload = { path: abs, name: f.name, size: Buffer.byteLength(f.body) }
  const r = await evalJs(`(() => {
    const cb = window.__LISTENERS__ && window.__LISTENERS__['webdav://upload']
    if (typeof cb !== 'function') return 'no-listener'
    cb({ payload: ${JSON.stringify(payload)} })
    return 'ok'
  })()`)
  console.log(`[e2e] upload ${f.name} -> ${r}`)
  await sleep(900)
}

const afterUpload = await evalJs(
  `((document.getElementById('root') || document.body).innerText || '').replace(/\\s+/g, ' ')`
)
console.log('[e2e] 七张投上来后 DOM ->', String(afterUpload).slice(0, 260))

/** 当前首页光标停在第几条（从最新往前数），用于断言"翻到哪了" */
const cursorAt = () =>
  evalJs(`(function () {
    var t = ((document.getElementById('root') || document.body).innerText || '');
    var m = /第 (\\d+)\\/6 张/.exec(t);
    return m ? Number(m[1]) : -1;
  })()`)

/* ④ 一句话的正文：光标默认在最新那张（png），往前翻三张到 txt */
await press('left', 3)
await sleep(600)
const sayChecked = await until(
  `(() => { const t = ((document.getElementById('root') || document.body).innerText || ''); return t.indexOf('今天降温') >= 0 ? JSON.stringify({ ok: true }) : '' })()`,
  15000
)
console.log('[e2e] 一句话正文 ->', sayChecked ? '读到了 ✅' : '没读到 ❌')

/* ⑥ 图片：翻回最新那张（png），按 OK 打开 */
await press('right', 3)
await sleep(500)
await press('ok')
await sleep(3500)
const img = await evalJs(`JSON.stringify({
  kind: (typeof window.__reader === 'function' ? window.__reader() : {}).kind || '',
  phase: (typeof window.__reader === 'function' ? window.__reader() : {}).phase || '',
  srcs: Array.prototype.map.call(document.querySelectorAll('img'), function (el) { return String(el.getAttribute('src') || el.src || '').slice(0, 22) }),
  size: (/\\d+ × \\d+/.exec(((document.getElementById('root') || document.body).innerText || '')) || [''])[0]
})`)
console.log('[e2e] 打开图片 ->', img)
await press('back')
await sleep(900)

/* ⑤ → ⑨ 逐个打开：CSV / Markdown / 普通 DOCX / 思维导图 DOCX / PDF
 * ⛔ 光标位置按**便签排列**数（pdf0 · docx1 · mindmap2 · say3 · md4 · csv5 · png6），
 *    「一句话」那张按 OK 不开东西，跨过它要多按一次 ←。 */
let cur = 6

async function openAt(idx, tag, expect) {
  const delta = idx - cur
  if (delta !== 0) await press(delta < 0 ? 'left' : 'right', Math.abs(delta))
  cur = idx
  await sleep(500)
  await press('ok')
  await sleep(tag === 'PDF' ? 6000 : 3000)
  const r = await evalJs(`JSON.stringify({
    kind: (typeof window.__reader === 'function' ? window.__reader() : {}).kind || '',
    phase: (typeof window.__reader === 'function' ? window.__reader() : {}).phase || '',
    pdfPages: (typeof window.__reader === 'function' ? window.__reader() : {}).pdfPages || 0,
    draws: (typeof window.__reader === 'function' ? window.__reader() : {}).draws || 0,
    boardBoxes: (typeof window.__reader === 'function' ? window.__reader() : {}).boardBoxes || 0,
    boardLines: (typeof window.__reader === 'function' ? window.__reader() : {}).boardLines || 0,
    boardScroll: (typeof window.__reader === 'function' ? window.__reader() : {}).boardScroll || 0,
    boardMaxScroll: (typeof window.__reader === 'function' ? window.__reader() : {}).boardMaxScroll || 0,
    hit: ${JSON.stringify(expect)} ? ((document.getElementById('root') || document.body).innerText || '').indexOf(${JSON.stringify(expect)}) >= 0 : false,
    rects: document.querySelectorAll('div').length
  })`)
  console.log(`[e2e] 打开 ${tag} ->`, r)
  return r
}

await openAt(5, 'CSV', '1月')
await press('back')
await sleep(900)
await openAt(4, 'Markdown', '完成直播链路')
await press('back')
await sleep(900)
await openAt(1, 'DOCX(表格文档)', '清单')
await press('back')
await sleep(900)

/* ⑥ 思维导图：必须走版面（文本框 + 连线），不是一行行文字 */
await openAt(2, 'DOCX(思维导图/版面)', '')
const boardShot = await evalJs(`JSON.stringify({
  kind: (typeof window.__reader === 'function' ? window.__reader() : {}).kind || '',
  boardBoxes: (typeof window.__reader === 'function' ? window.__reader() : {}).boardBoxes || 0,
  boardLines: (typeof window.__reader === 'function' ? window.__reader() : {}).boardLines || 0,
  maxScroll: (typeof window.__reader === 'function' ? window.__reader() : {}).boardMaxScroll || 0,
  texts: Array.prototype.slice.call(document.querySelectorAll('.qt-text-inner')).map(function (s) { return s.textContent }).filter(function (s) { return s && s.trim().length > 2 }).slice(0, 6)
})`)
console.log('[e2e] 版面详情 ->', boardShot)
if (BOARD_SHOT) {
  // ⛔ 必须单独一个路径：最后那张 `SHOT_PATH` 会被首页/启动器态覆盖掉
  const shot = await send('Page.captureScreenshot', { format: 'png' })
  writeFileSync(BOARD_SHOT, Buffer.from(shot.data, 'base64'))
  console.log('[e2e] 版面截图 ->', BOARD_SHOT)
}

/*
 * 层叠顺序断言（2026-11-09 补）。
 *
 * ⛔ 只数 DOM 里的文字是**假绿**：`PdfDraw` 按类型分组渲染，文字和历史排查中
 *    白底矩形同处一棵树时，文字在 DOM 里、屏幕上却被整片盖住 —— 上一版就是这么
 *    漏掉的（24 个白框遮掉 30 行字，探针全对、屏幕上没字）。
 *    判据：所有垫底矩形必须排在文字**之前**（先画在下，后画在上）。
 */
const paint = await evalJs(`(function () {
  /*
   * ⚠️ 不能查 \`p\`：web-renderer 把模板里的 \`<p>\` 编译成
   *    \`<div><span class="qt-text-inner">…</span></div>\`，\`querySelectorAll('p')\` 恒空。
   */
  var spans = Array.prototype.slice.call(document.querySelectorAll('.qt-text-inner'))
  var target = null
  for (var i = 0; i < spans.length; i += 1) {
    if ((spans[i].textContent || '').indexOf('杞人忧天') >= 0) { target = spans[i]; break }
  }
  if (!target) return JSON.stringify({ ok: false, why: '屏幕上找不到版面文字（杞人忧天）' })
  var textBox = target.parentElement
  var root = textBox && textBox.parentElement
  if (!root) return JSON.stringify({ ok: false, why: '文字没有版面父容器' })
  var kids = Array.prototype.slice.call(root.children)
  var lastBg = -1
  var bgCount = 0
  for (var k = 0; k < kids.length; k += 1) {
    var el = kids[k]
    var bg = el.style && el.style.backgroundColor
    if (!bg || bg === 'transparent' || bg === 'rgba(0, 0, 0, 0)') continue
    /*
     * ⚠️ 只算**能遮字的大色块**：连线是采样成 9×9 小方块的，它们也有底色，
     *    但排在文字后面也不影响可读性。按面积筛掉，否则断言恒红。
     */
    if (el.offsetWidth * el.offsetHeight < 4000) continue
    lastBg = k
    bgCount += 1
  }
  var textIndex = kids.indexOf(textBox)
  var r = target.getBoundingClientRect()
  return JSON.stringify({
    ok: textIndex > lastBg,
    textIndex: textIndex,
    lastBgIndex: lastBg,
    bgCount: bgCount,
    siblings: kids.length,
    rect: [r.left, r.top, r.width, r.height].map(Math.round)
  })
})()`)
console.log('[e2e] 层叠顺序 ->', paint)

/* DOM 结构诊断：文字到底以什么标签存在（`<p>` / `<div>` / shadow root？） */
const domShape = await evalJs(`(function () {
  var all = document.querySelectorAll('*')
  var hist = {}
  for (var i = 0; i < all.length; i += 1) {
    var tag = all[i].tagName
    hist[tag] = (hist[tag] || 0) + 1
  }
  var hit = null
  for (var j = 0; j < all.length; j += 1) {
    var txt = all[j].textContent || ''
    if (txt.indexOf('杞人忧天') >= 0 && all[j].children.length === 0) { hit = all[j]; break }
  }
  var shells = document.querySelectorAll('*')
  var shadow = 0
  for (var k = 0; k < shells.length; k += 1) { if (shells[k].shadowRoot) shadow += 1 }
  return JSON.stringify({
    hist: hist,
    shadowHosts: shadow,
    hitTag: hit ? hit.tagName : null,
    hitParent: hit && hit.parentElement ? hit.parentElement.tagName : null,
    hitHtml: hit ? String(hit.outerHTML).slice(0, 240) : '',
    hitRect: hit ? (function (r) { return [r.left, r.top, r.width, r.height].map(Math.round) })(hit.getBoundingClientRect()) : null
  })
})()`)
console.log('[e2e] DOM 结构 ->', domShape)
/* 版面滚动：↓ 之后 boardScroll 必须变大 */
await press('down', 2)
await sleep(500)
const scrolled = await evalJs(
  `JSON.stringify({ scroll: (typeof window.__reader === 'function' ? window.__reader() : {}).boardScroll || 0 })`
)
console.log('[e2e] 版面滚动 ->', scrolled)
await press('back')
await sleep(900)

await openAt(0, 'PDF', '')
await sleep(600)

/* ⑩ 重载页面 → 只能靠 `es_list_files` 把历史读回来（不投任何事件） */
await send('Page.reload')
await sleep(11000)
const restored = await evalJs(`JSON.stringify({
  count: (function () {
    var t = ((document.getElementById('root') || document.body).innerText || '');
    var m = /(\\d+) 张便签/.exec(t);
    return m ? Number(m[1]) : -1;
  })(),
  state: ((document.getElementById('root') || document.body).innerText || '').replace(/\\s+/g, ' ').slice(0, 180)
})`)
console.log('[e2e] 重载后（纯靠目录恢复）->', restored)

/* ⑪ 首页按返回 → 回启动器 */
await press('back')
await sleep(1500)
const leave = await evalJs(`String(location.href)`)
console.log('[e2e] 首页按返回 ->', leave)

if (SHOT_PATH) {
  const shot = await send('Page.captureScreenshot', { format: 'png' })
  writeFileSync(SHOT_PATH, Buffer.from(shot.data, 'base64'))
  console.log('[e2e] 截图 ->', SHOT_PATH)
}

console.log('=== 页面日志（尾部 25 条）===')
console.log(logs.slice(-25).join('\n'))
console.log('=== 页面异常 (' + errors.length + ') ===')
console.log(errors.slice(0, 5).join('\n'))
child.kill()
process.exit(0)
