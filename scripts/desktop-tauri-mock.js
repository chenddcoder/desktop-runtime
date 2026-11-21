/**
 * 桌面端验收注入脚本（CDP addScriptToEvaluateOnNewDocument，document-start）。
 *
 * 目的：在**无 GUI 的 headless Chrome** 里还原 Tauri WebView 的关键环境——
 *   1. `window.__TAURI__.core.invoke`（走 CDP binding 到 Node，Node 侧实现真 WebDAV / 真代理）
 *   2. `window.__TAURI__.event.listen`（用于接收 `webdav://upload`）
 *   3. `window.__ES_RESOLVER_BASE__`（钉住自建后端，桌面端真实做法）
 *   4. `window.__ES_DEFAULT_PKG__`
 *
 * ⛔ 为什么不能只 `window.__TAURI__ = {core:{invoke: async()=>({})}}`：
 *    web-runtime 的 es_pkg 分支要真下载加密包、真解密、真加载 bundle；
 *    WebDAV 要真起端口、真落盘、真读回。这些必须在 Node 侧做真事，
 *    否则验的只是「桩返回 200」而不是「这条链路能跑」。
 *
 * 只用于验收，不随 App 打包（不在 tauri bundle resources 里）。
 */
(function () {
  var pending = {};
  var seq = 0;

  window.__nodeReply = function (id, ok, data) {
    var p = pending[id];
    if (!p) return;
    delete pending[id];
    if (ok) {
      p.res(data);
    } else {
      p.rej(new Error(typeof data === 'string' ? data : JSON.stringify(data)));
    }
  };

  function invoke(cmd, args) {
    return new Promise(function (res, rej) {
      var id = ++seq;
      pending[id] = { res: res, rej: rej };
      try {
        window.__nodeInvoke(JSON.stringify({ id: id, cmd: cmd, args: args || {} }));
      } catch (e) {
        delete pending[id];
        rej(e);
      }
    });
  }

  var listeners = {};
  window.__TAURI__ = {
    core: { invoke: invoke },
    invoke: invoke,
    event: {
      listen: function (name, cb) {
        (listeners[name] = listeners[name] || []).push(cb);
        return Promise.resolve(function () {});
      },
      emit: function () {
        return Promise.resolve();
      },
    },
    __emit: function (name, payload) {
      (listeners[name] || []).forEach(function (cb) {
        try {
          cb({ event: name, payload: payload });
        } catch (e) {
          console.error('[tauri-mock] listener error', e);
        }
      });
    },
  };

  /*
   * resolve 基址：用 defineProperty **锁死**。
   *
   * ⛔ 不能只 `window.__ES_RESOLVER_BASE__ = x` 就完事：web-runtime 的 index.html
   *    内联脚本会无条件执行 `window.__CROSS_APP_RESOLVER_BASE__ = location.origin`，
   *    而桌面/无头环境的 origin 是 `tauri://localhost` 或 `http://127.0.0.1:1451`，
   *    拼出来的 resolve 地址没有后端。锁定 setter（no-op）+ getter 恒返回自建后端，
   *    才能保证 loader 读到的是真能解析包的地址。
   *    真实 desktop-runtime 在 main.rs 里注入同一段（document-start，早于 index.html）。
   */
  try {
    Object.defineProperty(window, '__CROSS_APP_RESOLVER_BASE__', {
      configurable: true,
      get: function () {
        return 'https://www.chenddcoder.cn'
      },
      set: function () {
        /* index.html 的赋值忽略掉 */
      },
    })
  } catch (e) {
    window.__CROSS_APP_RESOLVER_BASE__ = 'https://www.chenddcoder.cn'
  }

  if (!window.__ES_DEFAULT_PKG__) window.__ES_DEFAULT_PKG__ = 'cn.chenddcoder.tvcast';
})();
