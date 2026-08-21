// desktop-runtime 注入脚本（不改 web-runtime 本体）
// 职责：
//   ① 预加载 cn.chenddcoder.tvcast（投屏显象）——利用 web-runtime loader 原生支持的 ?es_pkg= 参数
//      加载源走自建后台 runtime.chenddcoder.cn（web-runtime 产物 + 加密 resolver + vue-ai 后端）
//   ② 窗口缩放自适应——修复 web-runtime 内置 scaleApp() 在 resize 时不重新计算导致 UI 不缩放的问题
//
// 注意：devUrl 已带 ?es_pkg=，ensureEsPkg 主要兜底（离线/手工启动时仍生效）；
//       若 URL 已有显式参数，绝不重置 location（避免 devUrl 参数被反复 reload）。
(function () {
  'use strict';

  function log(tag, msg, data) {
    var line = '[ui_scale] ' + tag + ' | ' + msg;
    if (data !== undefined) {
      try { line += ' | ' + JSON.stringify(data); } catch (e) {}
    }
    if (tag === 'error') console.error(line); else if (tag === 'warn') console.warn(line); else console.log(line);
    if (window.__proxy_logs) window.__proxy_logs.push({ ts: Date.now(), type: tag, msg: msg, data: data });
  }

  log('install', 'ui_scale installed', { url: location.href });

  // ===================== ⓪ resolver 指向自建后台 =====================
  // web-runtime 的 resolver base 默认是 run.quicktvui.com（huan 公共后台，没有
  // cn.chenddcoder.tvcast 这个包 → 桌面端 resolve 必然失败）。desktop 走自建后台：
  // runtime.chenddcoder.cn（web-runtime 静态入口 + /api/ 代理到 vue-ai 后端 RuntimeController）。
  // 必须在 web-runtime 主 JS 执行前设置（本脚本为 initialization_script / document-start 注入，
  // 早于主 JS 读 __CROSS_APP_RESOLVER_BASE__）。
  if (!window.__CROSS_APP_RESOLVER_BASE__) {
    window.__CROSS_APP_RESOLVER_BASE__ = 'https://runtime.chenddcoder.cn';
    log('resolver', 'set __CROSS_APP_RESOLVER_BASE__', window.__CROSS_APP_RESOLVER_BASE__);
  }

  // ===================== ① 预加载 cn.chenddcoder.tvcast =====================
  // web-runtime 的 loader 原生支持 ?es_pkg=<pkg> 加载快应用：
  //   - 优先读本地 IndexedDB 缓存（pkg:<pkg>）
  //   - 再 getPackageInfo 在线 resolve 下载 URL/MD5/版本，下载并加载 zip bundle
  // 仅当用户未显式指定加载方式（es_pkg/bundle/zipUrl/zip）时默认追加。
  //
  // 2026-08-21 修复：从 location.replace 改为 history.replaceState。
  // 原因：replace 会触发页面导航，在 tauri://localhost（release 内置 asset）等自定义
  // scheme 下 query 可能被丢 → 每次导航都发现无 es_pkg → 无限 replace 循环 → 内存爆炸。
  // replaceState 只改地址栏 URL 不导航，web-runtime 主 JS 读 location.search 即可拿到 es_pkg，
  // 彻底消除循环。另加 window.name 标记做双保险（跨导航保留，防任何残余循环路径）。
  (function ensureEsPkg() {
    try {
      var u = new URL(window.location.href);
      var explicit =
        u.searchParams.has('es_pkg') ||
        u.searchParams.has('bundle') ||
        u.searchParams.has('zipUrl') ||
        u.searchParams.has('zip');
      if (explicit) {
        log('es_pkg', 'URL already has explicit pkg param; skip redirect', {
          es_pkg: u.searchParams.get('es_pkg') || u.searchParams.get('bundle'),
        });
        return;
      }
      // 防循环：window.name 跨导航保留；若已尝试过一次仍无参数（说明该 scheme 不支持），不再追加
      if (window.name === 'es_pkg_redirected') {
        log('warn', 'es_pkg already attempted once but query not visible; skip to avoid loop', {
          href: location.href,
        });
        return;
      }
      window.name = 'es_pkg_redirected';
      log('es_pkg', 'injecting es_pkg=cn.chenddcoder.tvcast via replaceState (no navigation)');
      u.searchParams.set('es_pkg', 'cn.chenddcoder.tvcast');
      window.history.replaceState(null, '', u.toString());
    } catch (e) {
      log('error', 'ensureEsPkg failed', { message: e && e.message });
    }
  })();

  // ===================== ② 缩放自适应 =====================
  // 用 cover（取 max）填满视口，避免非 16:9 窗口（如 macOS 含标题栏导致 inner_size=1280×693）
  // 出现左右黑边；超出的部分由 main-container overflow:hidden 裁剪。
  // 这是 desktop-runtime 与 web-runtime 内置 scaleApp() 不一致的根因修复。
  var TV_W = 1920,
    TV_H = 1080;

  function realViewport() {
    var w = document.documentElement.clientWidth;
    var h = document.documentElement.clientHeight;
    if (!w || !h) return null;
    return { w: w, h: h };
  }

  function applyScale() {
    var c = document.getElementById('main-container');
    if (!c) return;
    var vp = realViewport();
    if (!vp) return;
    // cover：取 max，铺满整个视口；多出的内容由 overflow:hidden 裁掉
    var scale = Math.max(vp.w / TV_W, vp.h / TV_H);
    c.style.transformOrigin = 'center center';
    c.style.transform = 'scale(' + scale + ')';
    c.style.left = '50%';
    c.style.top = '50%';
    c.style.marginLeft = (-TV_W / 2) + 'px';
    c.style.marginTop = (-TV_H / 2) + 'px';
    // 确保溢出内容不露出（cover 模式必要）
    c.style.overflow = 'hidden';
    c.style.visibility = 'visible';
    log('scale-cover', 'scaled to fill', { vp: vp, scale: scale });
  }

  function onResize() { setTimeout(applyScale, 0); }

  // ========== 守护进程：阻止 web-runtime 内置 scaleApp() 把缩放改回 letterbox ==========
  // index.html 自带的 scaleApp() 也在 DOMContentLoaded / resize / orientationchange 时跑，
  // 它用 Math.min（contain）会再次把容器缩到 1232.6×693 居中 → 左右黑边重新出现。
  // 我们的 ui_scale 跑在 document-start 早于内置，但内置后跑会覆盖我们。
  // 这里加个 MutationObserver 持续观察 main-container 的 transform，一旦被改回 letterbox
  // （scale < max）就立即重新应用 cover。视觉上看不到任何闪烁。
  function guardCoverScale() {
    var c = document.getElementById('main-container');
    if (!c) return;
    var expected = Math.max(window.innerWidth / TV_W, window.innerHeight / TV_H);
    var mo = new MutationObserver(function () {
      var t = c.style.transform || '';
      var m = t.match(/scale\(([0-9.]+)\)/);
      var current = m ? parseFloat(m[1]) : null;
      // 当前 scale 比我们期望的小（被内置 scaleApp 改回 letterbox）→ 重新覆盖
      if (current === null || current < expected - 0.001) {
        applyScale();
      }
    });
    mo.observe(c, { attributes: true, attributeFilter: ['style'] });
    // 30s 后停止（页面基本稳定，不需要一直监听）
    setTimeout(function () {
      mo.disconnect();
    }, 30000);
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', function () {
      applyScale();
      window.addEventListener('resize', onResize);
      guardCoverScale();
      log('scale', 'applyScale mounted', realViewport());
    });
  } else {
    applyScale();
    window.addEventListener('resize', onResize);
    guardCoverScale();
    log('scale', 'applyScale mounted (sync)', realViewport());
  }

  requestAnimationFrame(applyScale);
  // 兜底：2s 后再强制覆盖一次，捕获 load 之后内置 scaleApp 的最后一次执行
  setTimeout(applyScale, 2000);
})();
