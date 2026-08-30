// 投屏诊断注入脚本（desktop-runtime 可选注入，devtools.json 的 "diagnostics": true 开启）。
// 排查 release 下「播放失败」等 webview 侧问题时使用：捕获 video error / console /
// 未处理异常 / video 事件时间线，经 dlna_debug_log 打回 Rust（写 app_log_dir/desktop-runtime.log），
// 并渲染常驻诊断面板（右上角绿色，点击折叠）+ 错误红条，截图即可取回现场。
// 默认不注入；需要时在 devtools.json 加 { "diagnostics": true } 并重新打包。
(function () {
  if (window.__diagHooked) return;
  window.__diagHooked = true;

  function reportDiag(tag, msg, data) {
    try {
      var invoke = window.__get_invoke ? window.__get_invoke() : null;
      if (!invoke) return;
      invoke('dlna_debug_log', { msg: '[diag] ' + tag + ': ' + String(msg || '').slice(0, 800), data: data || null })
        .catch(function () {});
    } catch (e) {}
  }
  function jsonish(v) {
    if (v === null || v === undefined) return String(v);
    if (typeof v === 'object') { try { return JSON.stringify(v); } catch (e) { return String(v); } }
    return String(v);
  }

  // ===== video 事件时间线 + 主动快照 =====
  window.__VIDEO_EVENTS__ = window.__VIDEO_EVENTS__ || [];
  function vlog(ev, v) {
    try {
      var e = window.__VIDEO_EVENTS__;
      e.push({
        t: Date.now() % 100000, ev: ev,
        rs: v.readyState, ns: v.networkState,
        err: (v.error && v.error.code) || null,
        src: String(v.currentSrc || v.src || '').slice(0, 90),
      });
      if (e.length > 60) e.shift();
      reportDiag('video-evt', ev + ' rs=' + v.readyState + ' ns=' + v.networkState +
        ' err=' + ((v.error && v.error.code) || '-') + ' src=' + String(v.currentSrc || v.src || '').slice(0, 120));
    } catch (e2) {}
  }
  var _MEDIA_EVENTS = ['loadstart','loadedmetadata','loadeddata','canplay','canplaythrough','playing','pause','ended','error','abort','stalled','suspend','waiting','progress','seeked'];
  function hookVideo(v) {
    if (v.__diagHooked) return;
    v.__diagHooked = true;
    for (var i = 0; i < _MEDIA_EVENTS.length; i++) {
      (function (ev) {
        v.addEventListener(ev, function () { vlog(ev, v); });
      })(_MEDIA_EVENTS[i]);
    }
  }
  function watchVideos() {
    try {
      var all = document.querySelectorAll('video');
      for (var i = 0; i < all.length; i++) hookVideo(all[i]);
    } catch (e) {}
  }
  watchVideos();
  try {
    if (window.MutationObserver) {
      var mo = new MutationObserver(function () { watchVideos(); });
      mo.observe(document.documentElement, { childList: true, subtree: true });
    }
  } catch (e) {}
  // 每 1.5s 主动快照（事件没触发也能看到 video 卡在哪个状态）
  setInterval(function () {
    try {
      var all = document.querySelectorAll('video');
      for (var i = 0; i < all.length; i++) {
        var v = all[i];
        // 快照携带 hls 实例状态 + buffered（排查「waiting/stalled 但无 error」的静默卡住）
        var extra = '';
        try {
          var hls = v.__hlsInstance;
          if (hls) {
            extra += ' hlsUrl=' + String(hls.url || '').slice(0, 80) +
              ' levels=' + (hls.levels ? hls.levels.length : -1) +
              ' curLevel=' + hls.currentLevel +
              ' media=' + !!hls.media;
          }
          var bEnd = 0;
          if (v.buffered && v.buffered.length > 0) bEnd = v.buffered.end(v.buffered.length - 1);
          extra += ' bufEnd=' + bEnd.toFixed(2) + ' ct=' + (v.currentTime || 0).toFixed(2);
        } catch (e5) {}
        if (v.__diagSeen === v.currentSrc + extra) continue;
        v.__diagSeen = v.currentSrc + extra;
        vlog('snap rs=' + v.readyState + ' ns=' + v.networkState + ' err=' + ((v.error && v.error.code) || '-') + extra, v);
      }
    } catch (e) {}
  }, 1500);

  // ===== video 元素 error（error 事件不冒泡，须捕获阶段监听）+ 错误红条 =====
  document.addEventListener('error', function (e) {
    var t = e.target;
    if (t && t.tagName === 'VIDEO') {
      var vids = [];
      try {
        var all = document.querySelectorAll('video');
        for (var i = 0; i < all.length; i++) {
          vids.push({
            idx: i,
            src: String(all[i].src || '').slice(0, 200),
            currentSrc: String(all[i].currentSrc || '').slice(0, 200),
            paused: all[i].paused,
            networkState: all[i].networkState,
            readyState: all[i].readyState,
          });
        }
      } catch (e2) {}
      var recentLog = [];
      try {
        var L = window.__LOG__ || [];
        recentLog = L.slice(-15).map(function (x) { return x.tag + ':' + String(x.msg).slice(0, 120); });
      } catch (e3) {}
      reportDiag('video-error',
        'code=' + (t.error && t.error.code) + ' msg=' + (t.error && t.error.message),
        {
          src: String(t.src || '').slice(0, 400),
          currentSrc: String(t.currentSrc || '').slice(0, 400),
          networkState: t.networkState,
          readyState: t.readyState,
          url: location.href,
          videos: vids,
          recentLog: recentLog,
        });

      // 错误红条（页面顶部，点击关闭）
      try {
        if (document.getElementById('__diag_bar__')) return;
        var bar = document.createElement('div');
        bar.id = '__diag_bar__';
        bar.style.cssText = 'position:fixed;top:0;left:0;right:0;z-index:999999;' +
          'background:#c62828;color:#fff;font:13px/1.5 -apple-system,monospace;' +
          'padding:10px 14px;white-space:pre-wrap;word-break:break-all;' +
          'box-shadow:0 2px 12px rgba(0,0,0,.5);cursor:pointer;';
        var code = (t.error && t.error.code) || -1;
        var msg = (t.error && t.error.message) || '(no message)';
        var srcShort = String(t.src || '').slice(0, 180);
        var curShort = String(t.currentSrc || '').slice(0, 180);
        bar.textContent =
          'VIDEO ERROR code=' + code + ' (' + msg + ')\n' +
          'networkState=' + t.networkState + ' readyState=' + t.readyState + '\n' +
          'src: ' + srcShort + '\n' +
          'currentSrc: ' + curShort + '\n' +
          'count=' + vids.length + ' (点击关闭)';
        bar.addEventListener('click', function () {
          if (bar.parentNode) bar.parentNode.removeChild(bar);
        });
        (document.body || document.documentElement).appendChild(bar);
      } catch (e4) {}
    }
  }, true);

  // ===== console.error / console.warn 转发 =====
  var _origError = console.error;
  console.error = function () {
    try {
      var args = Array.prototype.slice.call(arguments);
      reportDiag('console.error', args.map(jsonish).join(' ').slice(0, 1200));
    } catch (e) {}
    return _origError.apply(console, arguments);
  };

  var _origWarn = console.warn;
  console.warn = function () {
    try {
      var args = Array.prototype.slice.call(arguments);
      var line = args.map(jsonish).join(' ').slice(0, 1200);
      if (/casting|player|播放|投屏|error|video|dlna/i.test(line)) {
        reportDiag('console.warn', line);
      }
    } catch (e) {}
    return _origWarn.apply(console, arguments);
  };

  // console.log 定向转发（排查 m3u8/HLS 链路：IJKPlayerComponent / hls.js / autoProxy /
  // tauriEnv 关键日志平时走 console.log 不落盘，release 下完全不可见 → 定向前缀过滤）
  var _origLog = console.log;
  console.log = function () {
    try {
      var args = Array.prototype.slice.call(arguments);
      var line = args.map(jsonish).join(' ').slice(0, 1200);
      if (/IJKPlayer|HLS|hls\.js|m3u8|casting|AutoProxy|tauriEnv|es3-video-player/i.test(line)) {
        reportDiag('console.log', line);
      }
    } catch (e) {}
    return _origLog.apply(console, arguments);
  };

  // ===== hls.js 实例错误捕获（含非 fatal —— 静默重试中的错误也要可见） =====
  // 快照轮询里发现 video.__hlsInstance（es3-video-player.js 挂的调试引用）后挂钩：
  // hls.js 的事件总线事件名常量 Hls.Events.ERROR === 'hlsError'，直接用字符串监听。
  setInterval(function () {
    try {
      var all = document.querySelectorAll('video');
      for (var i = 0; i < all.length; i++) {
        var hls = all[i].__hlsInstance;
        if (!hls || hls.__diagHooked) continue;
        hls.__diagHooked = true;
        hls.on('hlsError', function (evt, data) {
          try {
            reportDiag('hls-error',
              'fatal=' + (data && data.fatal) + ' type=' + (data && data.type) +
              ' details=' + (data && data.details) +
              ' reason=' + jsonish(data && (data.reason || (data.err && data.err.message))).slice(0, 200) +
              ' frag=' + jsonish(data && data.frag ? data.frag.url : null).slice(0, 120));
          } catch (e) {}
        });
        reportDiag('hls-hook', 'hls instance hooked, url=' + String(hls.url || '').slice(0, 150));
      }
    } catch (e) {}
  }, 1000);

  // ===== JS 运行时错误 + 资源加载错误 + 未处理 Promise 拒绝 =====
  window.addEventListener('error', function (e) {
    var t = e.target || e.srcElement;
    if (t && t.tagName) {
      reportDiag('resource-error', jsonish(t.src || t.href || t.currentSrc || t.tagName).slice(0, 400));
    } else {
      reportDiag('window-onerror', (e.message || '') + ' @' + (e.filename || '') + ':' + (e.lineno || 0));
    }
  });
  window.addEventListener('unhandledrejection', function (e) {
    var r = e && e.reason;
    reportDiag('unhandledrejection', jsonish(r && (r.message || r)).slice(0, 500));
  });

  // ===== 常驻诊断面板（右上角可折叠） =====
  (function () {
    function panelHtml() {
      try {
        var vids = document.querySelectorAll('video');
        var vTxt = [];
        for (var i = 0; i < vids.length; i++) {
          var v = vids[i];
          vTxt.push('v' + i + ' rs=' + v.readyState + ' ns=' + v.networkState +
            ' err=' + ((v.error && v.error.code) || '-') + ' paused=' + v.paused +
            ' src=' + String(v.currentSrc || v.src || '').slice(0, 70));
        }
        var evs = (window.__VIDEO_EVENTS__ || []).slice(-8).map(function (x) {
          return x.t + ' ' + x.ev + ' rs' + x.rs + ' ns' + x.ns + (x.err != null ? ' ERR' + x.err : '');
        });
        var logs = (window.__LOG__ || []).slice(-4).map(function (x) { return x.tag + ' ' + String(x.msg).slice(0, 60); });
        var env = 'invoke=' + (window.__get_invoke ? 'Y' : 'N') + ' tauri=' + (window.__TAURI__ ? 'Y' : 'N') +
          ' esPkg=' + (window.__ES_DEFAULT_PKG__ ? 'Y' : 'N');
        return env + '\nVIDEO(' + vids.length + '):\n' + (vTxt.join('\n') || '(none)') +
          '\nEVTS:\n' + (evs.join('\n') || '(none)') + '\nLOG:\n' + (logs.join('\n') || '(none)');
      } catch (e) { return 'panel err: ' + e; }
    }
    function ensurePanel() {
      try {
        var p = document.getElementById('__diag_panel__');
        if (!p) {
          p = document.createElement('div');
          p.id = '__diag_panel__';
          p.style.cssText = 'position:fixed;top:44px;right:0;z-index:999998;background:rgba(0,0,0,.85);' +
            'color:#0f0;font:11px/1.45 -apple-system,monospace;padding:8px 10px;max-width:560px;' +
            'white-space:pre-wrap;word-break:break-all;cursor:pointer;border:1px solid #333;';
          p.addEventListener('click', function () {
            p.style.display = p.style.display === 'none' ? '' : 'none';
          });
          (document.body || document.documentElement).appendChild(p);
        }
        p.textContent = '[投屏诊断 ' + new Date().toTimeString().slice(0, 8) + ' 点击折叠]\n' + panelHtml();
      } catch (e) {}
    }
    setInterval(ensurePanel, 1000);
  })();
})();
