// 跨域代理注入脚本：把到 run.quicktvui.com 的 fetch / XMLHttpRequest 转交给 Rust 的
// proxy_http 命令发出（reqwest 不受浏览器同源策略约束），从而绕过 CORS。
//
// 关键约束：web-renderer 的 autoProxy 也会在页面加载后包裹 fetch / XHR。为避免两者
// 互相覆盖，本脚本在 initialization_script（document-start，早于页面脚本）阶段对
// 原生原型做「外层包裹」：autoProxy 后续包裹时，其 originalFetch/originalXHROpen
// 捕获到的就是本脚本的包裹层；非目标请求回落到真正的原生实现，不会双重代理也不会死循环。
//
// 仅在 URL host === run.quicktvui.com 时走代理，其它请求（含 autoProxy 改写后的
// localhost /proxy）原样放行。

(function () {
  var LOG_LIMIT = 200;

  // ===== 日志通道（同时 console + window.__proxy_logs，dlna_overlay 浮窗可读） =====
  function ensureLog() {
    if (!Array.isArray(window.__proxy_logs)) {
      Object.defineProperty(window, '__proxy_logs', {
        value: [],
        writable: false,
        configurable: false,
        enumerable: true,
      });
    }
    return window.__proxy_logs;
  }
  function log(type, msg, data) {
    var line = '[proxy_fetch] ' + type + ' | ' + msg;
    if (data !== undefined) {
      try {
        line += ' | ' + JSON.stringify(data);
      } catch (e) {
        line += ' | [unserializable: ' + (typeof data) + ']';
      }
    }
    // console
    if (type === 'error') {
      console.error(line);
    } else if (type === 'warn') {
      console.warn(line);
    } else {
      console.log(line);
    }
    // window.__proxy_logs（环形）
    var arr = ensureLog();
    arr.push({ ts: Date.now(), type: type, msg: msg, data: data });
    if (arr.length > LOG_LIMIT) arr.shift();
  }

  // 代理目标：huan 公共后台（*.quicktvui.com）+ 自建后台（*.chenddcoder.cn）。
  // 自建后台必须代理：resolve 返回的 packageUrl 指向 www.chenddcoder.cn（跨域），
  // zip 下载 XHR 不代理会撞 CORS（www.chenddcoder.cn 响应头 Access-Control-Allow-Origin 重复）。
  function isProxyTarget(host) {
    return host.endsWith('quicktvui.com') || host.endsWith('chenddcoder.cn');
  }

  // 命中规则（顺序敏感，返回最早命中即停）：
  //   ① URL host 是代理目标（quicktvui.com / chenddcoder.cn）—— 直接代理
  //   ② URL 是 `<current-host>/proxy?url=<encoded>` 形式 —— dev 模式下 web-runtime 的
  //      autoProxy 把跨域请求改写到 `<devUrl-host>/proxy?url=<encoded>`，目标指向 SPA fallback
  //      （serve.mjs / web-cli dev 都返回 HTML），不解开会返回整页 HTML 当响应。
  //      这里解开 `url` 参数直走 Rust 代理，避免这趟「假中转」。
  // 其它全部走原生实现，不二次代理。
  function resolveTarget(rawUrl) {
    var noop = { proxied: false, url: rawUrl, isDevProxy: false };
    try {
      var u = new URL(rawUrl, location.href);
      var host = u.hostname;
      if (isProxyTarget(host)) {
        return { proxied: true, url: u.toString(), isDevProxy: false };
      }
      if (u.host === location.host && u.pathname === '/proxy') {
        var orig = u.searchParams.get('url');
        if (orig) {
          try {
            var o = new URL(orig, location.href);
            if (isProxyTarget(o.hostname)) {
              return { proxied: true, url: o.toString(), isDevProxy: true };
            }
          } catch (e) { /* ignore */ }
        }
      }
      return noop;
    } catch (e) {
      return noop;
    }
  }

  function b64ToBytes(b64) {
    var bin = atob(b64);
    var a = new Uint8Array(bin.length);
    for (var i = 0; i < bin.length; i++) a[i] = bin.charCodeAt(i);
    return a;
  }

  function bytesToB64(bytes) {
    var s = '';
    for (var i = 0; i < bytes.length; i++) s += String.fromCharCode(bytes[i]);
    return btoa(s);
  }

  // 兼容获取 Tauri 的 invoke。
  // ⚠️ 关键修复：Tauri v2 全局 API（withGlobalTauri）把 invoke 挂在
  // `window.__TAURI__.core.invoke`，**顶层 `window.__TAURI__.invoke` 并不存在**（与 v1 不同）。
  function getInvoke() {
    var t = window.__TAURI__;
    if (!t) return null;
    if (t.core && typeof t.core.invoke === 'function') return t.core.invoke;
    if (typeof t.invoke === 'function') return t.invoke; // v1 兼容
    return null;
  }
  // 暴露到全局，供同页面注入的 dlna_overlay.js 复用（避免各脚本各写一份）。
  window.__get_invoke = getInvoke;

  // Tauri invoke 可能尚未就绪（initialization_script 早于 withGlobalTauri 注入）。
  // proxy_fetch.js 注入顺序：document-start → 立刻包裹原型；全局脚本在 DOMContentLoaded 前后挂 __TAURI__。
  // 必须轮询等待 invoke 可用后，记录就绪事件，否则拦截到的请求会拿到 null。
  var invokeReady = false;
  function waitForInvoke() {
    var tries = 0;
    var max = 200; // 200 * 100ms = 20s
    function tick() {
      var inv = getInvoke();
      if (inv) {
        invokeReady = true;
        var t = window.__TAURI__;
        var via = t && t.core && t.core.invoke ? 'core' : (t && t.invoke ? 'top-level' : 'unknown');
        log('invoke-ready', 'Tauri invoke available (path=' + via + ')');
        return;
      }
      if (++tries >= max) {
        log('error', 'Tauri invoke never became available after 20s; cross-origin will fail');
        return;
      }
      setTimeout(tick, 100);
    }
    tick();
  }

  function invokeProxy(method, url, headers, bodyBytes) {
    var invoke = getInvoke();
    if (!invoke) {
      log('error', 'invoke unavailable; request will fail', { method: method, url: url });
      return Promise.reject(
        new Error('[proxy_fetch] Tauri invoke unavailable (window.__TAURI__.core.invoke missing)')
      );
    }
    var body_base64 = bodyBytes ? bytesToB64(bodyBytes) : null;
    var reqSummary = {
      method: method,
      url: url,
      headerKeys: Object.keys(headers || {}),
      bodyBytes: bodyBytes ? bodyBytes.length : 0,
    };
    log('invoke-call', 'proxy_http → Rust', reqSummary);
    // Tauri v2 命令参数名必须严格匹配 Rust 签名。
    // 这里 Rust 是 `proxy_http(req: ProxyReq)`，所以前端必须整体包成 `{ req: {...} }`，
    // 不能把字段散开在 invoke 第二参的顶层（那样会报 "missing required key req."）。
    return invoke('proxy_http', {
      req: {
        url: url,
        method: method,
        headers: headers,
        body_base64: body_base64,
      },
    })
      .then(function (res) {
        var parsed = typeof res === 'string' ? JSON.parse(res) : res;
        log('invoke-ok', 'Rust responded', {
          status: parsed.status,
          respHeaderKeys: Object.keys(parsed.headers || {}),
          bodyBytes: parsed.bodyBase64 ? Math.floor((parsed.bodyBase64.length * 3) / 4) : 0,
        });
        return parsed;
      })
      .catch(function (err) {
        log('invoke-fail', 'proxy_http rejected: ' + (err && err.message ? err.message : err), reqSummary);
        throw err;
      });
  }

  function collectHeaders(h) {
    var headers = {};
    if (!h) return headers;
    if (typeof h.forEach === 'function') {
      h.forEach(function (v, k) { headers[k] = v; });
    } else if (h instanceof Headers) {
      h.forEach(function (v, k) { headers[k] = v; });
    } else {
      Object.keys(h).forEach(function (k) { headers[k] = h[k]; });
    }
    return headers;
  }

  function bodyToBytes(body) {
    if (body == null) return null;
    if (typeof body === 'string') return new TextEncoder().encode(body);
    if (body instanceof Uint8Array) return body;
    if (body instanceof ArrayBuffer) return new Uint8Array(body);
    return null; // Blob / FormData 在 resolve 场景不出现
  }

  // ===== fetch =====
  var nativeFetch = window.fetch;
  window.fetch = function (input, init) {
    init = init || {};
    var url = typeof input === 'string' ? input : (input && input.url) || '';
    var t = resolveTarget(url);
    if (!t.proxied) {
      return nativeFetch.call(window, input, init);
    }
    var method = (init.method || (input && input.method) || 'GET').toUpperCase();
    var headers = collectHeaders(init.headers || (input && input.headers));
    var bodyBytes = bodyToBytes(init.body != null ? init.body : input && input.body);
    log('intercept-fetch', method + ' ' + t.url, {
      bodyBytes: bodyBytes ? bodyBytes.length : 0,
      viaDevProxy: t.isDevProxy,
    });
    return invokeProxy(method, t.url, headers, bodyBytes).then(function (p) {
      var buf = b64ToBytes(p.bodyBase64);
      var rh = new Headers();
      Object.keys(p.headers || {}).forEach(function (k) { rh.set(k, p.headers[k]); });
      var blob = new Blob([buf], { type: rh.get('content-type') || '' });
      return new Response(blob, { status: p.status, headers: rh });
    });
  };

  // ===== XMLHttpRequest =====
  var NativeXHROpen = XMLHttpRequest.prototype.open;
  var NativeXHRSend = XMLHttpRequest.prototype.send;
  var NativeXHRSetHeader = XMLHttpRequest.prototype.setRequestHeader;

  XMLHttpRequest.prototype.setRequestHeader = function (name, value) {
    this.__ph = this.__ph || {};
    this.__ph[name] = value;
    return NativeXHRSetHeader.call(this, name, value);
  };

  XMLHttpRequest.prototype.open = function (method, url, async, user, pass) {
    this.__pm = (method || 'GET').toUpperCase();
    this.__pu = url;
    this.__pa = async !== false;
    return NativeXHROpen.call(this, method, url, async, user, pass);
  };

  XMLHttpRequest.prototype.send = function (body) {
    var t = resolveTarget(this.__pu);
    if (!t.proxied) {
      return NativeXHRSend.call(this, body);
    }
    var self = this;
    var method = this.__pm;
    var absUrl = t.url;
    var headers = this.__ph || {};
    var bodyBytes = bodyToBytes(body);
    log('intercept-xhr', method + ' ' + absUrl, {
      headerKeys: Object.keys(headers || {}),
      bodyBytes: bodyBytes ? bodyBytes.length : 0,
      responseType: self.responseType || '',
      viaDevProxy: t.isDevProxy,
    });

    invokeProxy(method, absUrl, headers, bodyBytes)
      .then(function (p) {
        var buf = b64ToBytes(p.bodyBase64);
        var rt = self.responseType || '';
        Object.defineProperty(self, 'status', { value: p.status, configurable: true, writable: true });
        Object.defineProperty(self, 'statusText', { value: 'OK', configurable: true, writable: true });
        Object.defineProperty(self, 'readyState', { value: 4, configurable: true, writable: true });

        if (rt === 'arraybuffer') {
          var ab = buf.buffer.slice(buf.byteOffset, buf.byteOffset + buf.byteLength);
          Object.defineProperty(self, 'response', { value: ab, configurable: true, writable: true });
        } else if (rt === 'json') {
          var txt = new TextDecoder().decode(buf);
          Object.defineProperty(self, 'responseText', { value: txt, configurable: true, writable: true });
          try {
            Object.defineProperty(self, 'response', { value: JSON.parse(txt), configurable: true, writable: true });
          } catch (e) {}
        } else if (rt === 'blob') {
          Object.defineProperty(self, 'response', {
            value: new Blob([buf], { type: (p.headers || {})['content-type'] || '' }),
            configurable: true,
            writable: true,
          });
          Object.defineProperty(self, 'responseText', { value: new TextDecoder().decode(buf), configurable: true, writable: true });
        } else {
          var text = new TextDecoder().decode(buf);
          Object.defineProperty(self, 'responseText', { value: text, configurable: true, writable: true });
          Object.defineProperty(self, 'response', { value: text, configurable: true, writable: true });
        }

        if (self.onreadystatechange) self.onreadystatechange();
        if (self.onload) self.onload();
      })
      .catch(function (err) {
        Object.defineProperty(self, 'readyState', { value: 4, configurable: true, writable: true });
        log('error', 'XHR proxy failed', { message: err && err.message, url: absUrl });
        if (self.onerror) self.onerror(new Event('error'));
      });
  };

  // 注册一个 debug helper：window.__proxy_stats() 在 console 显示最近 N 条
  window.__proxy_stats = function (n) {
    n = n || 20;
    var arr = window.__proxy_logs || [];
    var last = arr.slice(-n);
    console.log('[proxy_fetch] total=' + arr.length + ', last ' + last.length + ':');
    last.forEach(function (e, i) {
      console.log('  [' + (arr.length - last.length + i) + ']', e.type, e.msg, e.data || '');
    });
    return last;
  };

  log('install', 'proxy_fetch installed; targets=quicktvui.com/chenddcoder.cn');
  waitForInvoke();
})();
