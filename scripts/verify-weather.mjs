// 天气取数链路验证：模拟 Tauri 环境（serve.mjs + ui_scale.js + proxy_fetch.js + invoke 桩），
// RESOLVE_STUB=0 走真实服务端加载天气应用，然后：
//   ① 在页面环境直接测试 new Response(headers: Headers实例) vs (headers: 普通对象) 的 headers.keys；
//   ② 直接 fetch open-meteo，捕获 TypeError，确认根因；
//   ③ 打印 proxy_fetch 的 window.__proxy_logs。
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

let nodeFetch = async function (payload) {
  const { cmd, args } = payload || {};
  if (cmd === 'proxy_http') {
    const { method, headers, body_base64 } = args.req;
    const init = { method: method || 'GET', headers: headers || {} };
    if (body_base64) init.body = Buffer.from(body_base64, 'base64');
    try {
      const resp = await fetch(args.req.url, init);
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

await page.exposeFunction('__nodeFetch', nodeFetch);
await page.addInitScript(() => {
  window.__TAURI__ = { core: { invoke: (cmd, args) => window.__nodeFetch({ cmd, args }) } };
});
await page.addInitScript(`window.__ES_DEFAULT_PKG__ = ${JSON.stringify(DEFAULT_PKG)};`);
await page.addInitScript(UI_SCALE);
await page.addInitScript(PROXY_FETCH);
// 记录 open-meteo 响应（logger 在 autoProxy 之后执行，记录的即天气应用拿到的最终响应）
await page.addInitScript(() => {
  setTimeout(() => {
    const f = window.fetch;
    window.fetch = function (...args) {
      const url = typeof args[0] === 'string' ? args[0] : (args[0] && args[0].url) || '';
      if (url.indexOf('open-meteo.com') >= 0) {
        const p = f.apply(this, args);
        p.then((r) => {
          if (r && typeof r.text === 'function') {
            r.text().then((t) => {
              if (!window.__WEATHER_RESP__) window.__WEATHER_RESP__ = [];
              window.__WEATHER_RESP__.push({ url: url.slice(0, 90), body: t.slice(0, 1200) });
            }).catch(() => {});
          }
        }).catch(() => {});
        return p;
      }
      return f.apply(this, args);
    };
  }, 800);
});

const logs = [];
page.on('console', (m) => { logs.push(m.type() + ': ' + m.text()); });
page.on('pageerror', (e) => { logs.push('pageerror: ' + e.message); });

await page.goto(`${BASE}/`, { waitUntil: 'load', timeout: 30000 });
await page.waitForFunction(
  (pkg) => new RegExp('es_pkg=' + pkg.replace(/[.]/g, '\\.')).test(location.href),
  DEFAULT_PKG,
  { timeout: 10000 }
);
console.log('NAV_TO=' + (await page.evaluate(() => location.href)));

// 等天气应用渲染（"运行中" 字样出现）
try {
  await page.waitForFunction(() => (document.body.innerText || '').indexOf('运行中') >= 0, { timeout: 25000 });
  console.log('APP_LOADED=true');
} catch (e) {
  console.log('APP_LOADED=false (wait timeout): ' + (await page.evaluate(() => document.body.innerText.slice(0, 120))));
}

await page.waitForTimeout(3000);

// 等天气数据加载完成（"加载中…" 消失）
let settled = false;
try {
  await page.waitForFunction(() => (document.body.innerText || '').indexOf('加载中') < 0, { timeout: 15000 });
  settled = true;
} catch (e) { settled = false; }
console.log('WEATHER_SETTLED=' + settled);

// ① 测试 Response 构造：Headers 实例 vs 普通对象
const rTest = await page.evaluate(() => {
  try {
    const r1 = new Response(new Blob(['{"a":1}']), { status: 200, headers: new Headers({ 'content-type': 'application/json' }) });
    const r2 = new Response(new Blob(['{"a":1}']), { status: 200, headers: { 'content-type': 'application/json' } });
    return {
      r1_keys_type: typeof (r1 && r1.headers && r1.headers.keys),
      r2_keys_type: typeof (r2 && r2.headers && r2.headers.keys),
      r1_ctor: String(r1 && r1.constructor && r1.constructor.name),
    };
  } catch (e) {
    return { error: e.message };
  }
});
console.log('RESPONSE_CTOR_TEST=' + JSON.stringify(rTest));

// ② 直接 fetch open-meteo（走代理），捕获异常
const fetchTest = await page.evaluate(async () => {
  const out = { ok: false };
  try {
    const r = await fetch('https://geocoding-api.open-meteo.com/v1/search?name=%E5%8C%97%E4%BA%AC&count=1&language=zh&format=json');
    out.status = r.status;
    out.headers_keys_type = typeof r.headers.keys;
    out.headers_get = r.headers.get ? r.headers.get('content-type') : '(no get)';
    const j = await r.json();
    out.json_ok = true;
    out.results = (j.results || []).length;
  } catch (e) {
    out.error = e.message;
  }
  // forecast：检查页面拿到的 current 数据
  try {
    const r2 = await fetch('https://api.open-meteo.com/v1/forecast?latitude=39.9075&longitude=116.39723&current=temperature_2m,relative_humidity_2m,apparent_temperature,weather_code,wind_speed_10m&daily=weather_code,temperature_2m_max,temperature_2m_min&timezone=auto&forecast_days=7');
    const j2 = await r2.json();
    out.forecast_status = r2.status;
    out.current = j2.current || null;
    out.daily_days = (j2.daily && j2.daily.time || []).length;
  } catch (e) {
    out.forecast_error = e.message;
  }
  return out;
});
console.log('WEATHER_FETCH_TEST=' + JSON.stringify(fetchTest));

// ③.7 天气应用实际收到的响应（logger 记录）
const respLog = await page.evaluate(() => (window.__WEATHER_RESP__ || []).map((r) => ({ url: r.url, body: r.body })));
console.log('WEATHER_RESP=' + JSON.stringify(respLog));

// ③.9 完整复刻 weather.vue loadWeather 计算链路
const calcTest = await page.evaluate(async () => {
  const WMO = {
    0: ['clear','晴','☀️'], 1: ['partly','多云','🌤️'], 2: ['partly','多云','⛅'], 3: ['cloudy','阴','☁️'],
    45: ['fog','雾','🌫️'], 48: ['fog','雾','🌫️'],
    51: ['rain','小雨','🌦️'], 53: ['rain','小雨','🌦️'], 55: ['rain','小雨','🌦️'],
    56: ['rain','冻雨','🌧️'], 57: ['rain','冻雨','🌧️'],
    61: ['rain','中雨','🌧️'], 63: ['rain','中雨','🌧️'], 65: ['rain','大雨','🌧️'],
    66: ['rain','冻雨','🌧️'], 67: ['rain','冻雨','🌧️'],
    71: ['snow','小雪','🌨️'], 73: ['snow','中雪','🌨️'], 75: ['snow','大雪','🌨️'], 77: ['snow','雪','🌨️'],
    80: ['rain','阵雨','🌦️'], 81: ['rain','阵雨','🌧️'], 82: ['rain','强阵雨','⛈️'],
    85: ['snow','阵雪','🌨️'], 86: ['snow','阵雪','🌨️'],
    95: ['thunder','雷阵雨','⛈️'], 96: ['thunder','雷阵雨','⛈️'], 99: ['thunder','雷阵雨','⛈️'],
  };
  function describe(code) {
    const m = WMO[code] || ['cloudy','未知','🌥️'];
    return { cat: m[0], cond: m[1], emoji: m[2] };
  }
  const out = {};
  try {
    const geo = (await (await fetch('https://geocoding-api.open-meteo.com/v1/search?name=' + encodeURIComponent('北京') + '&count=1&language=zh&format=json')).json()).results[0];
    out.geo = { lat: geo.latitude, lon: geo.longitude };
    const u = 'https://api.open-meteo.com/v1/forecast?latitude=' + geo.latitude + '&longitude=' + geo.longitude +
      '&current=temperature_2m,relative_humidity_2m,apparent_temperature,weather_code,wind_speed_10m' +
      '&daily=weather_code,temperature_2m_max,temperature_2m_min' +
      '&timezone=auto&forecast_days=7';
    const r = await fetch(u);
    const data = await r.json();
    const cur = data.current || {};
    out.temperature_2m = cur.temperature_2m;
    out.weather_code = cur.weather_code;
    out.apparent_temperature = cur.apparent_temperature;
    out.temp_round = Math.round(cur.temperature_2m);
    out.feels_round = Math.round(cur.apparent_temperature);
    out.d0 = describe(cur.weather_code);
    out.keys = Object.keys(cur);
  } catch (e) {
    out.error = e.message;
  }
  return out;
});
console.log('CALC_TEST=' + JSON.stringify(calcTest));

// ③.10 直接查 Vue 组件实例（__vue__）
const vueCheck = await page.evaluate(() => {
  const res = {};
  const ids = ['6', '19', '23', '28'];
  for (const id of ids) {
    const el = document.querySelector('[data-component-name="View"][id="' + id + '"]');
    if (!el) { res['id' + id] = 'no-el'; continue; }
    const v = el.__vue__;
    res['id' + id] = v ? {
      name: v.$options && v.$options.name,
      isComp: !!(v.$options && v.$options.name),
    } : 'no-vue';
  }
  // 从任意 __vue__ 向上找 weather 组件
  let found = null;
  const walk = (el) => {
    if (found) return;
    if (el.__vue__ && el.__vue__.$data && el.__vue__.$data.cities) { found = el.__vue__; return; }
    for (const c of el.children || []) walk(c);
  };
  try { walk(document.body); } catch (e) {}
  if (found) {
    const d = found.$data;
    res.component = {
      name: found.$options.name,
      current: d.current,
      currentEmoji: d.currentEmoji,
      loading: d.loading,
      updateTime: d.updateTime,
      dailyLen: (d.daily || []).length,
    };
  }
  return res;
});
console.log('VUE_CHECK=' + JSON.stringify(vueCheck));

// ③.8 dump weather 应用渲染子树（Hippy 渲染后的实际节点）
const domDump = await page.evaluate(() => {
  function walk(el, depth) {
    if (depth > 22) return [];
    const out = [];
    const tag = el.tagName || el.nodeName;
    const txt = (el.childNodes && el.childNodes.length === 1 && el.childNodes[0].nodeType === 3)
      ? el.childNodes[0].textContent.slice(0, 40) : '';
    const attr = [];
    if (el.getAttribute) {
      ['name', 'data-component-name', 'id', 'class', 'data-row', 'data-position'].forEach((k) => {
        const v = el.getAttribute(k);
        if (v) attr.push(k + '=' + v.slice(0, 25));
      });
    }
    out.push('  '.repeat(depth) + tag + (txt ? ' TEXT[' + JSON.stringify(txt) + ']' : '') + (attr.length ? ' (' + attr.join(',') + ')' : ''));
    for (const c of el.children || []) out.push(...walk(c, depth + 1));
    return out;
  }
  // 从 weather 页面根 View（ESPageRootView 下的第一个 View）开始
  const rootView = document.querySelector('[data-component-name="ESPageRootView"]');
  if (!rootView) return '(no ESPageRootView)';
  const lines = walk(rootView, 0);
  return lines.slice(0, 90).join('\n');
});
console.log('DOM_DUMP_START\n' + domDump + '\nDOM_DUMP_END');

// 截图看实际渲染
await page.screenshot({ path: '/tmp/weather-page.png' });
console.log('SCREENSHOT=/tmp/weather-page.png');

// ③ 页面正文（天气数据是否渲染）
const bodyText = await page.evaluate(() => (document.body ? document.body.innerText.slice(0, 400) : '(no body)'));
console.log('PAGE_TEXT=' + JSON.stringify(bodyText));

// ③.5 找 weather 组件实例，检查 $data
const compCheck = await page.evaluate(() => {
  const vues = [];
  const walk = (el) => {
    if (el.__vue__) vues.push(el.__vue__);
    for (let i = 0; i < (el.children || []).length; i++) walk(el.children[i]);
  };
  try { walk(document.body); } catch (e) {}
  const comp = vues.find((v) => v.$options && v.$options.name === 'weather') || vues.find((v) => v.$data && v.$data.current && v.$data.cities) || null;
  if (!comp) return { found: false, vueCount: vues.length };
  const d = comp.$data;
  return {
    found: true,
    vueCount: vues.length,
    current: d.current,
    currentEmoji: d.currentEmoji,
    loading: d.loading,
    updateTime: d.updateTime,
    dailyLen: (d.daily || []).length,
    dailyFirst: d.daily && d.daily[0],
  };
});
console.log('COMP_CHECK=' + JSON.stringify(compCheck));

// ④ proxy_fetch 日志
const pfLogs = await page.evaluate(() => (window.__proxy_logs || []).slice(-25));
console.log('--- proxy_fetch logs ---');
pfLogs.forEach((e) => console.log('[' + e.type + '] ' + e.msg + (e.data ? ' | ' + JSON.stringify(e.data) : '')));

console.log('--- interesting console ---');
logs.filter((l) => /weather|proxy_fetch|AutoProxy|error|Error|TypeError/i.test(l)).slice(-25).forEach((l) => console.log(l));

await browser.close();
console.log('DONE');
