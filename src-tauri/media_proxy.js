// media_proxy.js — 把远程 http(s) 视频 src 改写为本地回环媒体代理
// 由 desktop-runtime (Tauri v2) initialization_script 注入（document-start 执行），
// 不改 web-runtime / 快应用业务代码。
//
// 目的：抖音等 CDN 投屏 URL（ott_cast，jump_ttl=1，302 一次性签名调度）在 <video>
// 直连时，每次补 Range / 重连 / seek / 切集都重新 302 到「新 host + 新签名」节点，
// 连续性断裂 → 播放卡进度、切集后新集起不来。改写为
//   http://127.0.0.1:<PORT>/media?url=<encoded-原URL>
// 后由 Rust 侧 media_proxy（见 src/media_proxy.rs）手动跟随 302 并缓存最终节点、
// 按 Range 流式回传，video 只见回环地址的稳定 206 响应。
//
// 安全/兼容：
//   - 跳过 data:/blob:/相对路径/回环地址/私有网段；
//   - 已改写的地址不重复包裹；
//   - 代理不可用（127.0.0.1:PORT 连不上触发 error）→ 回退直连原 URL（最多一次）；
//   - 全程 try/catch，hook 失败不影响业务。
(function () {
  if (window.__MEDIA_PROXY_HOOKED__) return;
  window.__MEDIA_PROXY_HOOKED__ = true;

  var PORT = Number(window.__MEDIA_PROXY_PORT__) || 5200;
  var PREFIX = 'http://127.0.0.1:' + PORT + '/media?url=';

  function isLoopbackOrPrivate(url) {
    try {
      var u = new URL(url);
      var h = u.hostname;
      if (h === '127.0.0.1' || h === 'localhost' || h === '::1') return true;
      if (/^(10\.|192\.168\.|172\.(1[6-9]|2\d|3[01])\.)/.test(h)) return true;
      return false;
    } catch (e) {
      return true;
    }
  }

  function wrap(url) {
    if (typeof url !== 'string') return url;
    if (!/^https?:\/\//.test(url)) return url; // 相对路径 / data: / blob: 不动
    if (isLoopbackOrPrivate(url)) return url; // 本地 / 内网不动
    if (url.indexOf(PREFIX) === 0) return url; // 已改写不重复包
    return PREFIX + encodeURIComponent(url);
  }

  function fallback(video) {
    // 代理不可用（127.0.0.1:PORT 连不上）时回退直连原 URL，最多一次
    if (!video || !video.dataset) return;
    var orig = video.dataset.mediaOriginal;
    if (!orig || video.__mediaFallbackUsed) return;
    video.__mediaFallbackUsed = true;
    try {
      video.src = orig;
    } catch (e) {}
  }

  function isProxiedUrl(u) {
    return typeof u === 'string' && u.indexOf(PREFIX) === 0;
  }

  try {
    // src 的 getter/setter 定义在 HTMLMediaElement.prototype（video/audio 公共父类）上，
    // HTMLVideoElement.prototype 自身没有 src 描述符，直接查 video 原型会拿到 undefined。
    var mediaProto = HTMLMediaElement.prototype;

    // 1) hook src property setter（video 与 audio 统一走媒体代理，均无副作用）
    var desc = Object.getOwnPropertyDescriptor(mediaProto, 'src');
    if (desc && typeof desc.set === 'function') {
      Object.defineProperty(mediaProto, 'src', {
        get: desc.get,
        set: function (v) {
          try {
            if (this.dataset) {
              this.dataset.mediaOriginal = typeof v === 'string' ? v : String(v);
            }
          } catch (e) {}
          desc.set.call(this, wrap(v));
        },
        configurable: true,
      });
    }

    // 2) setAttribute('src', ...) 兜底（hook 在 video 原型，实例方法查找路径能命中）
    var videoProto = HTMLVideoElement.prototype;
    var origSetAttr = videoProto.setAttribute;
    videoProto.setAttribute = function (name, value) {
      if (name === 'src') {
        try {
          this.dataset.mediaOriginal = String(value);
        } catch (e) {}
        value = wrap(value);
      }
      return origSetAttr.call(this, name, value);
    };

    // 3) error 兜底：代理不可用 → 回退直连原 URL（一次）。
    //    连接被拒时 currentSrc 可能为空，需同时检查 src 属性。
    document.addEventListener(
      'error',
      function (e) {
        var t = e.target;
        if (t && t.tagName === 'VIDEO' && isProxiedUrl(t.currentSrc || t.src)) {
          fallback(t);
        }
      },
      true
    );
  } catch (e) {
    try {
      console.warn('[media_proxy.js] hook failed:', e);
    } catch (_) {}
  }
})();
