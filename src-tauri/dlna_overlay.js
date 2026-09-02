// 投屏播放叠层 + 屏幕日志面板（由 desktop-runtime 注入到 web-runtime 页面，不改 web-runtime 本体）。
//
// 职责：
//   ① 监听 Rust 端 emit 的 "dlna://play" 事件，用全屏 <video> 播放被投的视频/音频。
//   ② 调试日志面板 / DLNA 状态条已拆到 debug_hud.js，仅 DEBUG 构建注入；release 不再包含。
//
// __TAURI__ 可能未就绪（initialization_script 早于 withGlobalTauri），轮询等待。

(function () {
  if (window.__dlnaOverlayInstalled) return;
  window.__dlnaOverlayInstalled = true;

  window.__LOG__ = window.__LOG__ || [];
  var LOG = window.__LOG__; // dlna 自有日志（release 无面板，仅内部诊断）
  function dlog(tag, msg, data) {
    var line = '[dlna_overlay] ' + tag + ' | ' + msg;
    if (data !== undefined) {
      try { line += ' | ' + JSON.stringify(data); } catch (e) { line += ' | [unserializable]'; }
    }
    if (tag === 'error') console.error(line); else if (tag === 'warn') console.warn(line); else console.log(line);
    // 必须存 data，否则面板渲染时 e.data 永远 undefined，诊断全丢。
    LOG.push({ ts: Date.now(), tag: tag, msg: msg, data: data });
    if (LOG.length > 200) LOG.shift();
  }

  dlog('install', 'dlna_overlay installed', { url: location.href });

  // ========== 投屏事件桥接：Rust dlna://* → 快应用 EventBus（ACTION_DLNA） ==========
  // 快应用真机契约：下行控制由原生 DLNA 层广播 EventBus 事件（ACTION_DLNA /
  // ACTION_DLNA_STATUS，payload 为 JSON 字符串）。desktop 场景由本脚本拿到
  // es3-vue 的 EventDispatcher 后模拟原生广播，快应用与真机走同一套契约，
  // 不再使用 window CustomEvent（真机没有浏览器 DOM）。
  var pendingDlnaPlay = null; // 快应用 EventDispatcher 未就绪时缓存的最新投屏请求

  // 诊断上报：把 overlay 侧关键链路状态经 Tauri invoke 打回 Rust 终端（eprintln），
  // 便于在看不到 webview 控制台时确认真实环境里"事件有没有走到快应用"。
  function reportDlnaState(msg, data) {
    try {
      var invoke = window.__get_invoke ? window.__get_invoke() : null;
      if (!invoke) { dlog('debug-report', 'no invoke (skip): ' + msg, data); return; }
      invoke('dlna_debug_log', { msg: String(msg || ''), data: data || null })
        .then(function () { dlog('debug-report', 'reported: ' + msg, data); })
        .catch(function (e) { dlog('warn', 'debug-report failed: ' + msg + ' ' + (e && e.message || e)); });
    } catch (e) {
      dlog('warn', 'debug-report throw: ' + msg + ' ' + (e && e.message || e));
    }
  }

  function getEventDispatcher() {
    try {
      var g = (typeof global !== 'undefined' && global.__GLOBAL__) ? global.__GLOBAL__ : null;
      if (g && g.jsModuleList && g.jsModuleList.EventDispatcher) {
        return g.jsModuleList.EventDispatcher;
      }
    } catch (e) {}
    return null;
  }

  // 经 es3-vue EventDispatcher 广播原生事件（与真机 EsApp 原生广播同路径）
  // 调用格式与 web-renderer 已验证的 sendNativeEvent 完全一致：
  //   ed.receiveNativeEvent([eventName, eventData])  （eventData 为对象）
  // 快应用侧 parsePayload 同时兼容「对象」与「真机 JSON 字符串」两种形态。
  //
  // es3-vue 3.x 的 EventDispatcher(Un) 收到注入后自动 se.$emit 到业务 EventBus
  // （与 Android 真机 EsEngine.sendNativeEvent → EventDispatcher.receiveNativeEvent
  //  同一契约），业务层 EventBus.$on('ACTION_DLNA'/'ACTION_DLNA_STATUS') 即可收到。
  // 事件未就绪时由调用方缓存、就绪后补发（broadcastPlay/flushPendingLoop）。
  function broadcastToApp(eventName, eventData) {
    var ed = getEventDispatcher();
    if (!ed || typeof ed.receiveNativeEvent !== 'function') {
      dlog('warn', 'EventDispatcher 未就绪，等待重试', { eventName: eventName });
      return false;
    }
    try {
      ed.receiveNativeEvent([eventName, eventData]);
      dlog('bridge', 'broadcast ' + eventName, eventData);
      return true;
    } catch (e) {
      dlog('error', 'broadcast ' + eventName + ' failed: ' + (e && e.message || e));
      return false;
    }
  }

  // 投屏请求 → ACTION_DLNA playerUrl（与 sendNativeEvent 同格式，对象形态）。
  // title 可选：普通投屏 Rust 不带；播放列表切集时带（esapp-tvcast casting 页用于更新标题）。
  function broadcastPlay(url, title) {
    if (!url) return false;
    var payload = { actionType: 'playerUrl', url: url, title: title || '' };
    if (broadcastToApp('ACTION_DLNA', payload)) {
      reportDlnaState('forward-play', { ok: true, url: url });
      pendingDlnaPlay = null;
      return true;
    }
    // 快应用 EventDispatcher 尚未就绪：缓存（覆盖旧的），轮询/就绪后补发
    pendingDlnaPlay = { url: url, title: payload.title };
    dlog('warn', '快应用未就绪，缓存投屏请求待补发', { url: url });
    reportDlnaState('forward-play', { ok: false, url: url, cached: true });
    return false;
  }

  // 播放控制 → ACTION_DLNA_STATUS playerStatus
  function broadcastControl(action, extra) {
    var payload = Object.assign({ actionType: 'playerStatus', action: action }, extra || {});
    broadcastToApp('ACTION_DLNA_STATUS', payload);
  }

  // 轮询 EventDispatcher 就绪后补发缓存投屏请求（兜底，不依赖 app-ready 事件）
  function flushPendingLoop() {
    if (pendingDlnaPlay) {
      var p = pendingDlnaPlay;
      pendingDlnaPlay = null;
      if (!broadcastPlay(p.url, p.title)) {
        pendingDlnaPlay = p; // 仍不可用，退回缓存
      }
    }
    setTimeout(flushPendingLoop, 500);
  }

  // Rust 侧收到快应用 sendRemoteEvent('tvcast_ready') 后 emit 的事件（加速补发）
  function listenAppReady() {
    if (!window.__TAURI__ || !window.__TAURI__.event) {
      setTimeout(listenAppReady, 200);
      return;
    }
    window.__TAURI__.event.listen('dlna://app-ready', function () {
      dlog('bridge', '快应用就绪（app-ready），补发缓存投屏请求', { hasPending: !!pendingDlnaPlay });
      reportDlnaState('app-ready', { hasPending: !!pendingDlnaPlay });
      if (pendingDlnaPlay) {
        var p = pendingDlnaPlay;
        pendingDlnaPlay = null;
        broadcastPlay(p.url, p.title);
      }
    });
  }

  function listenDlna() {
    if (!window.__TAURI__ || !window.__TAURI__.event) {
      setTimeout(listenDlna, 200);
      return;
    }
    window.__TAURI__.event.listen('dlna://play', function (e) {
      var url = e && e.payload && e.payload.url;
      if (url) {
        dlog('cast-play', '收到投屏请求（转发快应用）', { url: url, title: e.payload.title });
        reportDlnaState('received-play', { url: url });
        broadcastPlay(url, e.payload.title);
      }
    });
    window.__TAURI__.event.listen('dlna://status', function (e) {
      var p = e && e.payload;
      if (!p) return;
      if (p.ok === true) {
        dlog('status', 'DLNA UP', { port: p.port, uuid: p.uuid });
        showDlnaStatus('up', p);
      } else if (p.ok === null) {
        // Rust 侧刚启动时给的占位事件
        dlog('status', 'DLNA STARTING…', { msg: p.msg });
        showDlnaStatus('starting', p);
      } else {
        dlog('error', 'DLNA DOWN', { error: p.error });
        showDlnaStatus('down', p);
      }
    });
    // SSDP 收到 M-SEARCH 时也会被 Rust 推一个事件，便于排查"搜不到"问题
    window.__TAURI__.event.listen('dlna://msearch', function (e) {
      var p = e && e.payload;
      if (p) dlog('msearch', p.from || '?', { st: p.st, nt: p.nt });
    });
    // —— 投屏控制指令转发：手机端暂停/停止/拖动进度 → 快应用播放器跟随 ——
    window.__TAURI__.event.listen('dlna://seek', function (e) {
      var pos = e && e.payload && e.payload.position;
      dlog('cast-seek', '进度跳转指令（转发快应用）', { position: pos });
      if (pos != null) broadcastControl('seekTo', { position: Number(pos) });
    });
    window.__TAURI__.event.listen('dlna://stop', function () {
      dlog('cast-stop', '停止指令（转发快应用）');
      broadcastControl('stop');
    });
    window.__TAURI__.event.listen('dlna://pause', function () {
      dlog('cast-pause', '暂停指令（转发快应用）');
      broadcastControl('pause');
    });
    dlog('tauri-ready', 'event.listen("dlna://play/seek/stop/pause/status") registered → ACTION_DLNA');
    reportDlnaState('overlay-listen-registered', {});

    // 快应用 EventDispatcher 就绪后补发缓存投屏请求（兜底轮询 + app-ready 加速）
    flushPendingLoop();
    listenAppReady();

    // ========== P0 状态同步：轮询 dlna_status，不依赖一次性事件 ==========
    // 上一轮根因：auto-start 的 dlna://status(ok:true) 在 webview 监听者注册前就已发出，
    // 事件被吞 → 前端误判 DLNA 没起来 → 5s 兜底 invoke 又被 "已在运行" 拒 → 假红。
    // 改为：注册监听后立即轮询 dlna_status（权威状态），running 即绿；
    // 若窗口内仍未启动（auto-start 真失败），再显式 invoke dlna_start 拿真实错误。
    function dumpErr(e) {
      var msg = '<no message>', props = {};
      try {
        if (e == null) msg = 'null';
        else if (typeof e === 'string') msg = e;
        else if (typeof e === 'object') {
          if (e.message) msg = String(e.message);
          else if (e.msg) msg = String(e.msg);
          else if (e.toString) msg = String(e);
          Object.getOwnPropertyNames(e).forEach(function (k) {
            try { props[k] = String(e[k]); } catch (_) { props[k] = '<unreadable>'; }
          });
        } else msg = String(e);
      } catch (x) { msg = 'dump-failed: ' + String(x); }
      return { msg: msg, props: props };
    }
    function showDown(e) {
      var d = dumpErr(e);
      dlog('error', 'DLNA 启动失败: ' + d.msg, { error: d.msg, errorProps: d.props });
      showDlnaStatus('down', { error: d.msg });
      window.__dlna_status = 'down';
      if (body) {
        body.style.background = 'rgba(244,67,54,0.18)';
        setTimeout(function () { body.style.background = ''; }, 1500);
      }
    }
    function checkDlnaStatus(attempt) {
      if (window.__dlna_status === 'up' || window.__dlna_status === 'down') return;
      var invoke = window.__get_invoke ? window.__get_invoke() : null;
      if (!invoke) {
        // Tauri 还没就绪，等一会重试（最多 ~6s）
        if (attempt < 12) setTimeout(function () { checkDlnaStatus(attempt + 1); }, 500);
        return;
      }
      invoke('dlna_status', {}).then(function (s) {
        if (s && s.running) {
          dlog('status', 'DLNA UP (polled)', { port: s.port, uuid: s.uuid });
          showDlnaStatus('up', { port: s.port, uuid: s.uuid });
          window.__dlna_status = 'up';
        } else if (attempt < 10) {
          // auto-start 可能仍在跑（get_local_ip 等），继续等
          setTimeout(function () { checkDlnaStatus(attempt + 1); }, 500);
        } else {
          // 轮询窗口内仍未启动 → auto-start 大概率真失败，显式拉一次拿真实错误
          dlog('warn', 'dlna 轮询窗口内未启动，主动 invoke 拉取');
          invoke('dlna_start', { port: 5001 }).then(function (info) {
            dlog('status', 'dlna_start 主动拉取成功', info);
            showDlnaStatus('up', { port: info.port, uuid: info.uuid });
            window.__dlna_status = 'up';
          }).catch(showDown);
        }
      }).catch(function (e) {
        if (attempt < 10) setTimeout(function () { checkDlnaStatus(attempt + 1); }, 500);
        else showDown(e);
      });
    }
    // 先给一个"启动中"反馈，随后轮询权威状态
    showDlnaStatus('starting', { msg: 'DLNA 初始化中…' });
    checkDlnaStatus(0);
  }

  // ========== QR 诊断：扫描页面里疑似二维码的元素，报告状态 ==========
  // 上一轮 canvasHasPixels 只看左上 4x4，QR 的 quiet zone（白边）正好占那位置 → 误报 'empty'。
  // 这里：① 9 点网格采样 + 全图非白像素统计；② dump dataURL 截断前缀（一眼看出画没画）；
  // ③ 同时扫 SVG；④ dump CSS 样式（display/visibility/opacity/zIndex）—— 区分「没画」「被遮」「CSS 隐藏」。
  function scanQrElements() {
    var sels = [
      'canvas.qrcode', '.qrcode canvas', '.qr canvas',
      'canvas[class*="qr"]', 'img.qrcode', 'img.qr', '.qr img',
      '[class*="qr-code"]', '[class*="qrcode"]', '[class*="QRCode"]',
      'svg.qrcode', 'svg[class*="qr"]', '.qr svg',
    ];
    var found = [];
    for (var i = 0; i < sels.length; i++) {
      var nodes = document.querySelectorAll(sels[i]);
      for (var j = 0; j < nodes.length; j++) found.push(nodes[j]);
    }
    // 同时扫「投屏码」页面区域的 canvas/img/svg（QR 库可能没设标准 class）
    var codeRegion = findCodeRegion();
    if (codeRegion) {
      var regional = codeRegion.querySelectorAll('canvas, img, svg');
      for (var k = 0; k < regional.length; k++) {
        if (found.indexOf(regional[k]) === -1) found.push(regional[k]);
      }
    }
    if (found.length === 0) {
      dlog('qr-scan', '无 QR 候选元素', {
        totalCanvases: document.querySelectorAll('canvas').length,
        totalImgs: document.querySelectorAll('img').length,
        totalSvgs: document.querySelectorAll('svg').length,
        codeRegionFound: !!codeRegion,
      });
      return;
    }
    found.forEach(function (el, idx) {
      var rect = el.getBoundingClientRect();
      var cs = window.getComputedStyle(el);
      var info = {
        idx: idx,
        tag: el.tagName.toLowerCase(),
        cls: (el.className && el.className.toString ? el.className.toString() : '').slice(0, 80),
        id: el.id || null,
        bbox: Math.round(rect.width) + 'x' + Math.round(rect.height) + '@' +
              Math.round(rect.left) + ',' + Math.round(rect.top),
        display: cs.display,
        visibility: cs.visibility,
        opacity: cs.opacity,
        zIndex: cs.zIndex,
        position: cs.position,
        src: el.src || null,
      };
      if (el.tagName === 'CANVAS') {
        info.canvasW = el.width;
        info.canvasH = el.height;
        info.pixelStat = canvasPixelStat(el);
        try {
          var url = el.toDataURL('image/png');
          info.dataUrlLen = url.length;
          // 空白 canvas 的 dataURL 长度很短（~几百字节），画过的通常 >1KB
          info.dataUrlHead = url.slice(0, 80);
        } catch (e) {
          info.dataUrlErr = String(e.message || e);
        }
      } else if (el.tagName === 'IMG') {
        info.complete = el.complete;
        info.naturalSize = el.naturalWidth + 'x' + el.naturalHeight;
      } else if (el.tagName === 'SVG' || el.tagName === 'svg') {
        info.svgInnerLen = (el.innerHTML || '').length;
      }
      dlog('qr-scan', info.tag + ' #' + idx + ' ' + info.pixelStat || '', info);
    });
  }
  // 找到含「投屏码」文本的容器（向上 5 层作为 QR 区域）
  function findCodeRegion() {
    var all = document.querySelectorAll('*');
    for (var i = 0; i < all.length; i++) {
      var el = all[i];
      // 叶子节点且文本包含「投屏码」+ 不太长
      if (el.childElementCount === 0 && /投屏码/.test(el.textContent || '') && el.textContent.length < 30) {
        var p = el;
        for (var k = 0; k < 5 && p.parentElement; k++) p = p.parentElement;
        return p;
      }
    }
    return null;
  }
  // canvas 全图扫描：9 点网格 + 全图非白像素统计（避开 quiet zone 误判）
  function canvasPixelStat(canvas) {
    try {
      var ctx = canvas.getContext('2d');
      if (!ctx) return 'no-ctx';
      var w = canvas.width, h = canvas.height;
      if (w === 0 || h === 0) return 'uninit:0x0';
      // 9 点网格（角 + 中 + 边中）
      var pts = [
        [0, 0], [(w / 2) | 0, 0], [w - 1, 0],
        [0, (h / 2) | 0], [(w / 2) | 0, (h / 2) | 0], [w - 1, (h / 2) | 0],
        [0, h - 1], [(w / 2) | 0, h - 1], [w - 1, h - 1],
      ];
      for (var i = 0; i < pts.length; i++) {
        var d = ctx.getImageData(pts[i][0], pts[i][1], 1, 1).data;
        if (d[0] < 250 || d[1] < 250 || d[2] < 250) return 'drawn';
      }
      // 全图扫非白像素（阈值 <250 视为非白）
      var data = ctx.getImageData(0, 0, w, h).data;
      var nonWhite = 0;
      for (var k = 0; k < data.length; k += 4) {
        if (data[k] < 250 || data[k + 1] < 250 || data[k + 2] < 250) {
          nonWhite++;
          if (nonWhite > 200) return 'drawn:200+px';
        }
      }
      return nonWhite > 0 ? ('drawn:' + nonWhite + 'px') : 'empty';
    } catch (e) {
      return 'unreadable:' + (e.message || e);
    }
  }

  // release 构建：DLNA 状态条 / 日志面板由 debug_hud.js 覆写（仅 DEBUG 构建注入）。
  // 这里给 window 上的默认空实现：debug 构建注入 debug_hud 后会被真实实现覆盖，
  // release 构建没有 debug_hud，则保持空实现，保证启动链路（listenDlna 等）不报错。
  window.showDlnaStatus = window.showDlnaStatus || function () {};
  window.ensurePanelLoop = window.ensurePanelLoop || function () {};

  // ========== 版本角标：右下角常驻显示应用版本（Rust 侧替换 __APP_VERSION__ 占位符）==========
  function installVersionBadge() {
    if (!document.body) {
      setTimeout(installVersionBadge, 100);
      return;
    }
    if (document.getElementById('__es_desktop_version_badge')) return;
    var badge = document.createElement('div');
    badge.id = '__es_desktop_version_badge';
    badge.textContent = 'v' + '__APP_VERSION__';
    // 低调常驻：不拦截鼠标事件，避免挡住快应用右下角的按钮/二维码
    badge.style.cssText =
      'position:fixed;right:10px;bottom:8px;' +
      'font:11px/1.4 -apple-system,BlinkMacSystemFont,Helvetica,sans-serif;' +
      'color:rgba(255,255,255,0.85);background:rgba(0,0,0,0.35);' +
      'padding:2px 8px;border-radius:8px;z-index:2147483000;' +
      'pointer-events:none;user-select:none;letter-spacing:0.3px;';
    document.body.appendChild(badge);
    dlog('install', '版本角标已挂载', { version: badge.textContent });
  }

  // 启动
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', function () { installVersionBadge(); ensurePanelLoop(); listenDlna(); });
  } else {
    installVersionBadge();
    ensurePanelLoop();
    listenDlna();
  }
})();
