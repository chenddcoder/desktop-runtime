// 桌面端「默认加载快应用」链路验证（模拟 Tauri 环境，无头 Chromium）。
// 做法：serve.mjs 提供本地 web-runtime/dist 静态资源 + /proxy 端点；注入 ui_scale.js / proxy_fetch.js；
// 用 node fetch 模拟 Rust proxy_http（window.__TAURI__.core.invoke），从而让 es_pkg 解析、
// 加密 zip 下载、Open-Meteo 取数在桌面环境（release 无 /proxy 端点）下也能跑通。
//
// 本脚本验证的核心（对应 es-app.config.json 的 esPackage 配置化机制）：
//   ① 页面 URL 无 es_pkg 时，ui_scale.js 从 window.__ES_DEFAULT_PKG__（main.rs 注入，源于
//      src-tauri/es-app.config.json）用 history.replaceState 原地注入 es_pkg=<默认包>；
//   ② proxy_fetch 正确把 resolve XHR 经 invoke → proxy_http 转发并拿回响应。
import pw from '/Users/chendd/.trae/skills/playwright-skill/node_modules/playwright-core/index.js';
const { chromium } = pw;
import { readFileSync } from 'node:fs';

const EXEC = '/Users/chendd/Library/Caches/ms-playwright/chromium-1208/chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing';
const PORT = process.env.PORT || '1499';
const BASE = `http://localhost:${PORT}`;

const UI_SCALE = readFileSync('/Volumes/WD/Users/chendd/Documents/self/扩展屏快应用/desktop-runtime/src-tauri/ui_scale.js', 'utf8');
const PROXY_FETCH = readFileSync('/Volumes/WD/Users/chendd/Documents/self/扩展屏快应用/desktop-runtime/src-tauri/proxy_fetch.js', 'utf8');
const ES_CONFIG = JSON.parse(readFileSync('/Volumes/WD/Users/chendd/Documents/self/扩展屏快应用/desktop-runtime/src-tauri/es-app.config.json', 'utf8'));
const DEFAULT_PKG = ES_CONFIG.esPackage || 'cn.chenddcoder.tvcast';

// node 侧模拟 Rust proxy_http：解析 {req:{url,method,headers,body_base64}}，真实发请求，
// 回传 {status,headers,bodyBase64}（与 Rust ProxyResp 字段一致，proxy_fetch.js 会 JSON.parse）。
//
// resolve 桩（默认启用）：把 resolve 请求替换为直接回包，用于**离线/无网络**验证代理转发链路。
// 服务端 resolve 正常时（RuntimeController 已下线 vue-ai-resolver 中间层、nginx 直连 vue-ai，
// 加密协议实测 200），设 RESOLVE_STUB=0 走真实转发，可完整端到端验证
// （客户端加密 → 服务端解密 → 加密响应 → 客户端解密 → 下载 zip → 渲染应用）。
const RESOLVE_STUB = process.env.RESOLVE_STUB !== '0';
let nodeFetch = async function (payload) {
  const { cmd, args } = payload || {};
  if (cmd === 'proxy_http') {
    const url = (args.req && args.req.url) || '';
    if (RESOLVE_STUB && /app\/resolve/.test(url)) {
      const body = JSON.stringify({
        code: 200,
        data: {
          packageName: DEFAULT_PKG,
          version: '1.0.2',
          packageUrl: `https://www.chenddcoder.cn/api/runtime/package/${DEFAULT_PKG}-1.0.2.zip`,
          packageMd5: '',
          meta: { appName: ES_CONFIG.appName || '', icon: '', iconCircle: '' },
        },
      });
      return JSON.stringify({ status: 200, headers: { 'content-type': 'application/json' }, bodyBase64: Buffer.from(body).toString('base64') });
    }
    const { method, headers, body_base64 } = args.req;
    const init = { method: method || 'GET', headers: headers || {} };
    if (body_base64) init.body = Buffer.from(body_base64, 'base64');
    try {
      const resp = await fetch(url, init);
      const buf = Buffer.from(await resp.arrayBuffer());
      const h = {};
      resp.headers.forEach((v, k) => { h[k] = v; });
      return JSON.stringify({ status: resp.status, headers: h, bodyBase64: buf.toString('base64') });
    } catch (e) {
      return JSON.stringify({ status: 502, headers: {}, bodyBase64: Buffer.from(String(e.message || e)).toString('base64') });
    }
  }
  return JSON.stringify({ status: 200, headers: {}, bodyBase64: '' });
};

const browser = await chromium.launch({ headless: true, executablePath: EXEC, args: ['--no-sandbox', '--disable-gpu'] });
const page = await browser.newPage({ viewport: { width: 1920, height: 1080 } });

// invoke 直接透传 nodeFetch 的原始 ProxyResp JSON 字符串，由 proxy_fetch.js 自己重建 Response
// （不能在页面侧 new Response()，否则 proxy_fetch 拿到的对象没有 bodyBase64 → atob 报错）。
await page.exposeFunction('__nodeFetch', nodeFetch);
await page.addInitScript(() => {
  window.__TAURI__ = { core: { invoke: (cmd, args) => window.__nodeFetch({ cmd, args }) } };
});
// 模拟 main.rs 注入的默认包（对应 es-app.config.json 的 esPackage）
await page.addInitScript(`window.__ES_DEFAULT_PKG__ = ${JSON.stringify(DEFAULT_PKG)};`);
await page.addInitScript(UI_SCALE);
await page.addInitScript(PROXY_FETCH);

const logs = [];
page.on('console', (m) => { logs.push(m.type() + ': ' + m.text()); });
page.on('pageerror', (e) => { logs.push('pageerror: ' + e.message); });

// ===== 1) 打开 index.html → ui_scale 应注入 es_pkg=<默认包> =====
// 用 /home 路径（serve SPA fallback 到 index.html）：es3-router 把 /index.html 解析成 / 无匹配
// （routes 无 / 且 error 路由缺失）会白屏；/home 直接命中首屏路由。
await page.goto(`${BASE}/home`, { waitUntil: 'load', timeout: 30000 });
await page.waitForFunction(
  (pkg) => new RegExp('es_pkg=' + pkg.replace(/[.]/g, '\\.')).test(location.href),
  DEFAULT_PKG,
  { timeout: 10000 }
);
console.log('NAV_TO=' + (await page.evaluate(() => location.href)));
const injected = await page.evaluate((pkg) => new URL(location.href).searchParams.get('es_pkg') === pkg, DEFAULT_PKG);
console.log('ES_PKG_INJECTED=' + injected);
await page.screenshot({ path: '/tmp/verify-default-pkg.png' });

// ===== 2) 等 web-runtime 加载动作（resolve 桩返回 200 → 转发回包成功） =====
await page.waitForTimeout(4000);
const bodyText = await page.evaluate(() => (document.body ? document.body.innerText.slice(0, 300) : '(no body)'));
console.log('PAGE_TEXT=' + JSON.stringify(bodyText));

// ===== 3) 天气卡图标检查（launcher 渲染后：img 存在 + 加载成功） =====
// 注意：Hippy web-renderer 把 Vue 的 class 剥成内联 style，DOM 无 .app-card，
// 只能按 name="allApp" 属性 + 文本「天气」定位卡片。
await page.waitForTimeout(4000);
const iconCheck = await page.evaluate(() => {
  const cards = Array.from(document.querySelectorAll('[name="allApp"]'));
  const card = cards.find((c) => (c.textContent || '').indexOf('天气') >= 0) || null;
  if (!card) return { card: false, nameCards: cards.length };
  const img = card.querySelector('img');
  if (!img) return { card: true, img: false, text: (card.textContent || '').slice(0, 20) };
  return { card: true, img: true, src: (img.src || '').slice(0, 50), loaded: img.complete && img.naturalWidth > 0, w: img.naturalWidth, h: img.naturalHeight };
});
console.log('WEATHER_ICON_CHECK=' + JSON.stringify(iconCheck));
await page.screenshot({ path: '/tmp/verify-launcher-icons.png' });

// ===== 4) 关键日志 =====
const interesting = logs.filter((l) => /proxy_fetch|resolve|es_pkg|error|Error/i.test(l)).slice(-20);
console.log('--- interesting logs ---');
interesting.forEach((l) => console.log(l));

await browser.close();
console.log('DONE');
