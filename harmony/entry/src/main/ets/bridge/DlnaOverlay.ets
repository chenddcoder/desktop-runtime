/**
 * document-start 注入脚本：DLNA 下行事件 → 快应用（EventDispatcher）桥。
 *
 * 为什么必须有这段：
 *   Rust 侧 DMR 收到手机投屏后只做两件事 —— 回 SOAP 200、`emit("dlna://play", {...})`。
 *   真正的「换源播放」是页面里跑的快应用（cn.chenddcoder.tvcast）做的，它只认原生契约：
 *   EventDispatcher 广播 `ACTION_DLNA` / `ACTION_DLNA_STATUS`。
 *   桌面端靠 `src-tauri/dlna_overlay.js` 做这层翻译（见 main.rs 的 initialization_script）。
 *   鸿蒙侧一开始漏了这段 —— 现象就是：手机能发现、能建立起投屏会话、电视端却一直不播，
 *   因为 `dlna://play` 事件到了页面没有任何监听者，等于石沉大海。
 *
 * 契约与桌面端逐字一致（同一份快应用代码，改一处就漂移）：
 *   es3-vue EventDispatcher.receiveNativeEvent([eventName, eventData])
 *     ACTION_DLNA        { actionType:'playerUrl', url, title, mediaType }
 *     ACTION_DLNA_STATUS { actionType:'playerStatus', action:'seekTo'|'stop'|'pause', ... }
 *
 * ⛔ 这段是模板字符串：内部不许出现反引号，否则字面量提前闭合。
 */
export const DLNA_OVERLAY_JS: string = `
(function () {
  if (window.__dlnaOverlayInstalled) return;
  window.__dlnaOverlayInstalled = true;

  function dlog(tag, msg, data) {
    var line = '[dlna_overlay] ' + tag + ' | ' + msg;
    if (data !== undefined) {
      try { line += ' | ' + JSON.stringify(data); } catch (e) { line += ' | [unserializable]'; }
    }
    if (tag === 'error') console.error(line);
    else if (tag === 'warn') console.warn(line);
    else console.log(line);
  }

  dlog('install', 'dlna overlay installed', { url: location.href });

  // 快应用 EventDispatcher 未就绪时缓存的最新投屏请求（只留最后一条，同桌面端）
  var pendingDlnaPlay = null;

  // 诊断上报：把 overlay 侧关键链路打回 Rust（dlna.log / hilog），
  // 看不到 webview 控制台时靠这几行判断「事件到底走到哪一步」。
  function reportDlnaState(msg, data) {
    try {
      var invoke = window.__get_invoke ? window.__get_invoke() : null;
      if (!invoke) return;
      invoke('dlna_debug_log', { msg: String(msg || ''), data: data || null }).catch(function () {});
    } catch (e) {}
  }

  function getEventDispatcher() {
    var g = null;
    try {
      if (typeof global !== 'undefined' && global.__GLOBAL__) g = global.__GLOBAL__;
    } catch (e) {}
    if (!g) {
      // 不是所有运行时都会 shim 出 global（webpack 会，裸浏览器不会）
      try { g = window.__GLOBAL__; } catch (e) {}
    }
    try {
      if (g && g.jsModuleList && g.jsModuleList.EventDispatcher) {
        return g.jsModuleList.EventDispatcher;
      }
    } catch (e) {}
    return null;
  }

  // 与真机 EsEngine.sendNativeEvent 同一契约：注入后 es3-vue 会自动 se.$emit 到业务 EventBus
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

  // mediaType 必须透传：华为图库推的是 JPEG，落在 <video> 上必然 error
  // → 业务层自 stop → 手机端看到「投屏失败」（桌面端 2026-09-13 实测）。
  function broadcastPlay(url, title, mediaType) {
    if (!url) return false;
    var payload = {
      actionType: 'playerUrl',
      url: url,
      title: title || '',
      mediaType: mediaType || ''
    };
    if (broadcastToApp('ACTION_DLNA', payload)) {
      pendingDlnaPlay = null;
      reportDlnaState('forward-play', { ok: true, url: url, mediaType: payload.mediaType });
      return true;
    }
    pendingDlnaPlay = { url: url, title: payload.title, mediaType: payload.mediaType };
    reportDlnaState('forward-play', { ok: false, url: url, cached: true });
    return false;
  }

  function broadcastControl(action, extra) {
    var payload = { actionType: 'playerStatus', action: action };
    if (extra) {
      for (var k in extra) {
        if (Object.prototype.hasOwnProperty.call(extra, k)) payload[k] = extra[k];
      }
    }
    broadcastToApp('ACTION_DLNA_STATUS', payload);
  }

  // 兜底轮询：快应用启动晚于首次投屏时，等 EventDispatcher 出现再补发
  var dispatcherSeen = false;
  function flushPendingLoop() {
    if (!dispatcherSeen) {
      var ed = getEventDispatcher();
      if (ed && typeof ed.receiveNativeEvent === 'function') {
        dispatcherSeen = true;
        // 这行出现在日志里，就说明桥接通了（投屏不播时才需要往下查业务层）
        dlog('ready', 'EventDispatcher 已就绪，投屏桥可用');
        reportDlnaState('overlay-dispatch-ready', {});
      }
    }
    if (pendingDlnaPlay) {
      var p = pendingDlnaPlay;
      pendingDlnaPlay = null;
      if (!broadcastPlay(p.url, p.title, p.mediaType)) {
        pendingDlnaPlay = p;
      }
    }
    setTimeout(flushPendingLoop, 500);
  }

  // 快应用主动 sendRemoteEvent('tvcast_ready') → Rust emit dlna://app-ready（加速补发）
  function listenAppReady() {
    if (!window.__TAURI__ || !window.__TAURI__.event) {
      setTimeout(listenAppReady, 200);
      return;
    }
    window.__TAURI__.event.listen('dlna://app-ready', function () {
      dlog('bridge', 'app-ready，补发缓存投屏请求', { hasPending: !!pendingDlnaPlay });
      if (pendingDlnaPlay) {
        var p = pendingDlnaPlay;
        pendingDlnaPlay = null;
        broadcastPlay(p.url, p.title, p.mediaType);
      }
    });
  }

  function listenDlna() {
    if (!window.__TAURI__ || !window.__TAURI__.event) {
      setTimeout(listenDlna, 200);
      return;
    }
    window.__TAURI__.event.listen('dlna://play', function (e) {
      var p = e && e.payload;
      var url = p && p.url;
      if (!url) return;
      dlog('cast-play', '收到投屏请求', { url: url, title: p.title, mediaType: p.mediaType });
      reportDlnaState('received-play', { url: url, mediaType: p.mediaType });
      broadcastPlay(url, p.title, p.mediaType);
    });
    window.__TAURI__.event.listen('dlna://seek', function (e) {
      var pos = e && e.payload && e.payload.position;
      if (pos == null) return;
      dlog('cast-seek', '进度跳转', { position: pos });
      broadcastControl('seekTo', { position: Number(pos) });
    });
    window.__TAURI__.event.listen('dlna://stop', function () {
      dlog('cast-stop', '停止');
      broadcastControl('stop');
    });
    window.__TAURI__.event.listen('dlna://pause', function () {
      dlog('cast-pause', '暂停');
      broadcastControl('pause');
    });

    dlog('ready', 'listen dlna://play/seek/stop/pause/app-ready');
    flushPendingLoop();
    listenAppReady();
  }

  listenDlna();
})();
`;
