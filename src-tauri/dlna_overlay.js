// 投屏播放叠层 + 屏幕日志面板（由 desktop-runtime 注入到 web-runtime 页面，不改 web-runtime 本体）。
//
// 职责：
//   ① 监听 Rust 端 emit 的 "dlna://play" 事件，用全屏 <video> 播放被投的视频/音频。
//   ② 在 webview 右下角浮一个日志面板，实时显示 window.__proxy_logs（与 console 同步）。
//      释放你在 Tauri dev 里找 DevTools 的摩擦 —— 直接看屏幕就知道 resolve 走了/没走。
//   ③ dev 模式（loaded by initialization_script）默认展开浮窗；发布模式默认折叠为小圆点。
//
// __TAURI__ 可能未就绪（initialization_script 早于 withGlobalTauri），轮询等待。

(function () {
  if (window.__dlnaOverlayInstalled) return;
  window.__dlnaOverlayInstalled = true;

  var LOG = []; // dlna 自有日志
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

  // ========== 日志浮窗 ==========
  var panel, body, toggleBtn, header, dragHandle;
  var collapsed = false;
  var pos = null; // 由用户拖动决定；null = 用默认右下

  function buildPanel() {
    panel = document.createElement('div');
    panel.id = '__dlna_log_panel';
    panel.style.cssText =
      'position:fixed;right:16px;bottom:16px;width:480px;max-width:70vw;height:280px;' +
      'background:rgba(20,20,28,0.92);color:#eaeaea;font:12px/1.45 Menlo,Consolas,monospace;' +
      'border:1px solid rgba(255,255,255,0.12);border-radius:10px;z-index:2147483646;' +
      'display:flex;flex-direction:column;box-shadow:0 8px 28px rgba(0,0,0,0.4);' +
      'backdrop-filter:blur(8px);overflow:hidden;';

    header = document.createElement('div');
    header.style.cssText =
      'padding:8px 12px;background:rgba(255,255,255,0.06);display:flex;align-items:center;gap:8px;' +
      'cursor:move;user-select:none;font-weight:600;font-size:12px;';
    var title = document.createElement('span');
    title.textContent = '📋 dev log (proxy_fetch + dlna)';
    title.style.flex = '1';
    header.appendChild(title);

    var clearBtn = document.createElement('button');
    clearBtn.textContent = '清';
    clearBtn.style.cssText = btnStyle();
    clearBtn.onclick = function () { LOG.length = 0; if (window.__proxy_logs) window.__proxy_logs.length = 0; render(); };
    header.appendChild(clearBtn);

    var qrBtn = document.createElement('button');
    qrBtn.textContent = 'QR';
    qrBtn.title = '手动扫一次 QR 元素';
    qrBtn.style.display = 'none'; // 调试用完先隐藏，避免误触
    qrBtn.style.cssText = btnStyle();
    qrBtn.onclick = function () { scanQrElements(); };
    header.appendChild(qrBtn);

    var copyBtn = document.createElement('button');
    copyBtn.textContent = '拷';
    copyBtn.style.cssText = btnStyle();
    copyBtn.onclick = function () {
      try {
        var snap = LOG.concat(window.__proxy_logs || []).map(function (e) {
          return '[' + new Date(e.ts).toISOString().slice(11, 23) + '][' + (e.tag || e.type) + '] ' + e.msg + (e.data ? ' ' + JSON.stringify(e.data) : '');
        }).join('\n');
        navigator.clipboard.writeText(snap);
        dlog('info', '日志已复制到剪贴板');
      } catch (e) {}
    };
    header.appendChild(copyBtn);

    toggleBtn = document.createElement('button');
    toggleBtn.textContent = '−';
    toggleBtn.style.cssText = btnStyle();
    toggleBtn.onclick = function () { setCollapsed(!collapsed); };
    header.appendChild(toggleBtn);

    panel.appendChild(header);

    body = document.createElement('div');
    body.id = '__dlna_log_body';
    body.style.cssText = 'flex:1;overflow:auto;padding:8px 12px;white-space:pre-wrap;word-break:break-all;';
    panel.appendChild(body);

    document.body.appendChild(panel);

    // 拖动
    header.addEventListener('mousedown', function (ev) {
      if (ev.target.tagName === 'BUTTON') return;
      ev.preventDefault();
      var sx = ev.clientX, sy = ev.clientY;
      var r = panel.getBoundingClientRect();
      var ox = sx - r.left, oy = sy - r.top;
      function move(e) {
        var nx = Math.max(0, e.clientX - ox);
        var ny = Math.max(0, e.clientY - oy);
        panel.style.left = nx + 'px';
        panel.style.top = ny + 'px';
        panel.style.right = 'auto';
        panel.style.bottom = 'auto';
      }
      function up() {
        document.removeEventListener('mousemove', move);
        document.removeEventListener('mouseup', up);
      }
      document.addEventListener('mousemove', move);
      document.addEventListener('mouseup', up);
    });

    // 折叠状态
    var saved = '';
    try { saved = localStorage.getItem('__dlna_log_collapsed') || ''; } catch (e) {}
    setCollapsed(saved === '1', true);
  }

  function btnStyle() {
    return 'background:rgba(255,255,255,0.08);color:#fff;border:1px solid rgba(255,255,255,0.18);' +
      'border-radius:6px;padding:2px 8px;font-size:11px;cursor:pointer;font-family:inherit;';
  }

  function setCollapsed(c, skipSave) {
    collapsed = c;
    if (!panel) return;
    if (collapsed) {
      body.style.display = 'none';
      panel.style.height = 'auto';
      panel.style.width = 'auto';
      panel.style.minWidth = '180px';
      toggleBtn.textContent = '+';
    } else {
      body.style.display = 'block';
      panel.style.height = '280px';
      panel.style.width = '480px';
      panel.style.maxWidth = '70vw';
      toggleBtn.textContent = '−';
      render();
    }
    if (!skipSave) {
      try { localStorage.setItem('__dlna_log_collapsed', c ? '1' : '0'); } catch (e) {}
    }
  }

  function render() {
    if (!body || collapsed) return;
    var arr = LOG.concat(window.__proxy_logs || []).sort(function (a, b) { return a.ts - b.ts; });
    var html = '';
    for (var i = 0; i < arr.length; i++) {
      var e = arr[i];
      var t = new Date(e.ts).toISOString().slice(11, 23);
      var color = (e.tag === 'error' || e.type === 'error') ? '#ff6b6b'
        : (e.tag === 'warn' || e.type === 'warn') ? '#ffb454'
          : (e.tag === 'invoke-ok' || e.type === 'invoke-ok') ? '#6bcf7f'
            : '#eaeaea';
      var dataStr = '';
      if (e.data) {
        try { dataStr = ' ' + JSON.stringify(e.data); } catch (err) {}
      }
      html += '<div style="color:' + color + '">[' + t + '][' + (e.tag || e.type) + '] '
        + escapeHtml(e.msg) + escapeHtml(dataStr) + '</div>';
    }
    body.innerHTML = html;
    // 滚到底
    body.scrollTop = body.scrollHeight;
  }

  function escapeHtml(s) {
    return String(s)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;');
  }

  function ensurePanelLoop() {
    buildPanel();
    setInterval(render, 500);
    // QR 诊断已暂时关闭（用户切到 DevTools 排查 encrypt-proxy 问题）。
    // 需要重新扫描时取消下面这行注释即可：
    // setInterval(scanQrElements, 2000);
  }

  // ========== 投屏播放叠层（原职责保留） ==========
  var overlay, video, hint, posTimer = null;
  function buildCast() {
    if (overlay) return;
    overlay = document.createElement('div');
    overlay.id = 'dlna-cast-overlay';
    overlay.style.cssText =
      'position:fixed;left:0;top:0;width:100%;height:100%;background:#000;' +
      'z-index:2147483647;display:none;align-items:center;justify-content:center;';
    video = document.createElement('video');
    video.style.cssText = 'width:100%;height:100%;object-fit:contain;background:#000;';
    video.setAttribute('playsinline', '');
    video.setAttribute('webkit-playsinline', '');
    video.controls = true;
    overlay.appendChild(video);
    hint = document.createElement('div');
    hint.style.cssText =
      'position:absolute;left:0;right:0;bottom:24px;text-align:center;color:#fff;' +
      'font:16px/1.5 sans-serif;text-shadow:0 1px 2px #000;pointer-events:none;';
    hint.textContent = '投屏播放中 · 按 Esc 退出';
    overlay.appendChild(hint);
    document.body.appendChild(overlay);
    document.addEventListener('keydown', function (e) {
      if (e.key === 'Escape' && overlay.style.display !== 'none') hideCast();
    });
  }
  function showCast(url) {
    buildCast();
    video.src = url;
    overlay.style.display = 'flex';
    var p = video.play();
    if (p && p.catch) p.catch(function () { hint.textContent = '点击画面开始播放 · 按 Esc 退出'; });
    dlog('cast-show', url);
    startPositionReporter();
  }
  function hideCast() {
    if (!overlay) return;
    try { video.pause(); } catch (e) {}
    video.removeAttribute('src');
    try { video.load(); } catch (e) {}
    overlay.style.display = 'none';
    stopPositionReporter();
    reportStopped();
  }

  // —— 进度回传：DLNA 客户端进度条靠周期性 GetPositionInfo 驱动，必须让 Rust 拿到真实进度 ——
  // 之前 GetPositionInfo 永远返回 0:00:00，导致客户端进度条不动、拖动后读回仍是 0。
  function reportPosition() {
    if (!video || overlay.style.display === 'none') return;
    var dur = isFinite(video.duration) ? Math.floor(video.duration) : 0;
    var pos = isFinite(video.currentTime) ? Math.floor(video.currentTime) : 0;
    var playing = !video.paused && !video.ended;
    var paused = video.paused;
    var invoke = window.__get_invoke ? window.__get_invoke() : null;
    if (!invoke) return;
    invoke('dlna_report_position', { position: pos, duration: dur, playing: playing, paused: paused })
      .catch(function () {});
  }
  function reportStopped() {
    var invoke = window.__get_invoke ? window.__get_invoke() : null;
    if (!invoke) return;
    // 投屏结束：position/duration 归零、playing/paused 皆否 → 后端状态机置 Stopped
    invoke('dlna_report_position', { position: 0, duration: 0, playing: false, paused: false })
      .catch(function () {});
  }
  function startPositionReporter() {
    stopPositionReporter();
    // 每秒回传一次真实进度；客户端 GetPositionInfo 轮询（通常 1-2s）即可平滑跟随。
    posTimer = setInterval(reportPosition, 1000);
  }
  function stopPositionReporter() {
    if (posTimer) { clearInterval(posTimer); posTimer = null; }
  }

  function listenDlna() {
    if (!window.__TAURI__ || !window.__TAURI__.event) {
      setTimeout(listenDlna, 200);
      return;
    }
    window.__TAURI__.event.listen('dlna://play', function (e) {
      var url = e && e.payload && e.payload.url;
      if (url) showCast(url);
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
    // —— 投屏控制指令：之前 Rust 只 emit 了 dlna://play，Seek/Stop/Pause 被静默吞掉，
    //    导致手机端拖动进度 / 退出投屏时桌面端 <video> 完全没反应。这里补齐监听。
    window.__TAURI__.event.listen('dlna://seek', function (e) {
      var pos = e && e.payload && e.payload.position;
      if (video && pos != null) {
        try {
          video.currentTime = Number(pos);
          dlog('cast-seek', '进度跳转到 ' + pos + 's');
          reportPosition(); // 立即把跳转后的位置回传，让客户端进度条同步
        } catch (x) { dlog('error', 'seek 失败: ' + x.message); }
      }
    });
    window.__TAURI__.event.listen('dlna://stop', function () {
      dlog('cast-stop', '收到停止指令，收起播放层');
      hideCast();
    });
    window.__TAURI__.event.listen('dlna://pause', function () {
      if (video) {
        try { video.pause(); dlog('cast-pause', '暂停'); } catch (x) {}
      }
    });
    dlog('tauri-ready', 'event.listen("dlna://play/seek/stop/pause/status") registered');

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

  // 在页面顶部贴一条 DLNA 状态条（不依赖 web-runtime 注入）
  var dlnaBadge;
  function showDlnaStatus(state, info) {
    if (!dlnaBadge) {
      dlnaBadge = document.createElement('div');
      dlnaBadge.style.cssText =
        'position:fixed;left:16px;top:16px;padding:6px 12px;border-radius:8px;' +
        'font:12px/1.4 Menlo,Consolas,monospace;color:#fff;z-index:2147483646;' +
        'box-shadow:0 4px 14px rgba(0,0,0,0.4);cursor:default;user-select:none;';
      document.body.appendChild(dlnaBadge);
    }
    if (state === 'up') {
      dlnaBadge.style.background = 'rgba(76,175,80,0.92)';
      dlnaBadge.textContent = '📡 DLNA 在线 · port ' + info.port;
      dlnaBadge.title = 'uuid: ' + info.uuid + '\nhttp://*:' + info.port + '/device-desc.xml';
    } else if (state === 'starting') {
      dlnaBadge.style.background = 'rgba(255,180,0,0.92)';
      dlnaBadge.textContent = '⏳ DLNA 启动中…';
      dlnaBadge.title = info.msg || '';
    } else {
      dlnaBadge.style.background = 'rgba(244,67,54,0.92)';
      dlnaBadge.textContent = '⚠️ DLNA 启动失败';
      dlnaBadge.title = info.error || '';
    }
    window.__dlna_status = state;
  }

  // 启动
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', function () { ensurePanelLoop(); listenDlna(); });
  } else {
    ensurePanelLoop();
    listenDlna();
  }
})();
