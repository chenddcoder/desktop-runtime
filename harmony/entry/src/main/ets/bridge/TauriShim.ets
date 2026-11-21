/**
 * document-start 注入脚本：在 web-runtime 主 JS 之前，把 Tauri 的全局 API 补出来。
 *
 * 为什么是 document-start：
 *   web-runtime 的 index.html 会先写 __webpack_public_path__ / __CROSS_APP_RESOLVER_BASE__，
 *   随后 bundle 才读 __TAURI__。若 __TAURI__ 晚于 bundle 出现，首次 resolve 就会走
 *   fetch 分支并被 CORS 拦死（桌面端为此专门做过 waitForInvoke 轮询）。这里在文档开始
 *   就挂好，前端无需改造即可认为「自己在 Tauri 宿主里」。
 *
 * 通信模型（不使用 javaScriptProxy 的 Promise 返回值，避免序列化歧义）：
 *   JS  invoke(cmd,args) → __ohosBridge.call(reqId, cmd, argsJson)
 *   ArkTS 处理完      → controller.runJavaScript("__tauriBridgeResolve(reqId, ok, payload)")
 *   ArkTS 下行事件    → controller.runJavaScript("__tauriHostEmit(name, payloadJson)")
 */
export const TAURI_SHIM_JS: string = `
(function () {
  if (window.__TAURI_HOST_HARMONY__) return;
  window.__TAURI_HOST_HARMONY__ = true;

  var seq = 0;
  var pending = {};
  var listeners = {};
  var pendingEarly = [];

  function bridgeReady() {
    return window.__ohosBridge && typeof window.__ohosBridge.call === 'function';
  }

  function flushEarly() {
    while (pendingEarly.length) {
      var job = pendingEarly.shift();
      job();
    }
  }

  function invoke(cmd, args) {
    return new Promise(function (resolve, reject) {
      var id = 'h' + (++seq);
      pending[id] = { resolve: resolve, reject: reject };
      var run = function () {
        try {
          window.__ohosBridge.call(id, String(cmd), JSON.stringify(args === undefined ? null : args));
        } catch (e) {
          delete pending[id];
          reject(e);
        }
      };
      if (bridgeReady()) run();
      else pendingEarly.push(run);
    });
  }

  window.__tauriBridgeResolve = function (id, ok, payload) {
    var p = pending[id];
    if (!p) return;
    delete pending[id];
    var value;
    try { value = payload ? JSON.parse(payload) : null; } catch (e) { value = payload; }
    if (ok) p.resolve(value);
    else p.reject(new Error(String(value || 'invoke failed')));
  };

  window.__tauriHostEmit = function (name, payloadJson) {
    var arr = listeners[name] || [];
    var payload;
    try { payload = payloadJson ? JSON.parse(payloadJson) : null; } catch (e) { payload = payloadJson; }
    for (var i = 0; i < arr.length; i++) {
      try { arr[i]({ event: name, id: 0, payload: payload }); } catch (e) {}
    }
  };

  var tauri = {
    core: { invoke: invoke },
    invoke: invoke,
    event: {
      listen: function (name, cb) {
        (listeners[name] = listeners[name] || []).push(cb);
        return Promise.resolve(function () {
          var arr = listeners[name] || [];
          var i = arr.indexOf(cb);
          if (i >= 0) arr.splice(i, 1);
        });
      },
      once: function (name, cb) {
        var un = null;
        var wrap = function (e) { if (un) un(); cb(e); };
        return tauri.event.listen(name, wrap).then(function (u) { un = u; return u; });
      },
      emit: function (name, payload) {
        window.__tauriHostEmit(name, JSON.stringify(payload === undefined ? null : payload));
        return Promise.resolve();
      }
    },
    window: { getCurrentWindow: function () { return { label: 'main' }; } },
    app: { getVersion: function () { return Promise.resolve('1.0.0'); } }
  };

  window.__TAURI__ = tauri;
  window.isTauri = true;

  /*
   * Tauri v2 的内核形状。@kit 的 desktopListen() 在拿不到 __TAURI__.event.listen
   * 时，会退回 transformCallback + invoke('plugin:event|listen') 这套原生协议；
   * 两条路都补出来，前端就不必关心自己走的是哪条。
   * ⛔ 这段注释写在模板字符串里，不许出现反引号，否则模板字面量会被提前闭合。
   */
  var callbacks = {};
  var cbSeq = 100000;

  window.__TAURI_INTERNALS__ = {
    invoke: function (cmd, args) {
      // 事件协议：只在本层处理，不下发到 ArkTS（Rust 侧没有 tauri 的事件总线）
      if (cmd === 'plugin:event|listen') {
        var a = args || {};
        var name = String(a.event || '');
        var handler = a.handler;
        var cb = callbacks[handler];
        if (name && typeof cb === 'function') {
          // 这条路径的 cb 期望「裸 payload」（desktopListen 里是 (p) => cb(p)），
          // 而 __tauriHostEmit 统一发 {event,id,payload} —— 在这里拆一层，两条路径同形。
          var wrap = function (ev) { cb(ev && ev.payload !== undefined ? ev.payload : ev); };
          wrap.__raw = cb;
          (listeners[name] = listeners[name] || []).push(wrap);
        }
        return Promise.resolve(handler);
      }
      if (cmd === 'plugin:event|unlisten') {
        var u = args || {};
        var un = String(u.event || '');
        var id = u.eventId;
        var fn = callbacks[id];
        var arr = listeners[un] || [];
        for (var k = 0; k < arr.length; k++) {
          if (arr[k] === fn || arr[k].__raw === fn) { arr.splice(k, 1); break; }
        }
        return Promise.resolve();
      }
      return invoke(cmd, args);
    },
    transformCallback: function (cb) {
      var id = ++cbSeq;
      callbacks[id] = cb;
      return id;
    },
    metadata: { currentWindow: { label: 'main' }, currentWebview: { windowLabel: 'main', label: 'main' } }
  };

  // 与桌面端 proxy_fetch.js 的约定保持一致：它是「兼容获取 invoke」的唯一入口。
  window.__get_invoke = function () { return invoke; };

  // javaScriptProxy 在个别时序下晚于 document-start 可用，轮询兜底 3s。
  var tries = 0;
  var timer = setInterval(function () {
    if (bridgeReady()) {
      clearInterval(timer);
      flushEarly();
      return;
    }
    if (++tries > 30) { clearInterval(timer); flushEarly(); }
  }, 100);
})();
`;
