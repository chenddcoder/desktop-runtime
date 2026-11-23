/**
 * document-start 注入脚本：让 1920x1080 的 web-runtime 舞台跟随窗口尺寸等比缩放。
 *
 * 为什么鸿蒙侧必须自己注入：
 *   web-runtime 的 index.html 内置 scaleApp() 用 `_originalInnerWidth / _originalInnerHeight`
 *   （页面加载那一刻的视口尺寸，const 缓存）算缩放。窗口拉大后 resize 回调虽然会跑，
 *   但读的仍是旧值 —— 且脚本已经用 defineProperty 把 window.innerWidth 覆盖成 1920，
 *   再也拿不到真实视口。结果就是窗口放大后画面原地不动，右侧/下方一片黑。
 *   桌面端用 src-tauri/ui_scale.js 修过同一个问题，这里是它在 ArkWeb 侧的对应实现。
 *
 * 取值只用 documentElement.clientWidth/Height（未被覆盖）。
 * 缩放用 contain（Math.min）：等比放到最大且完整可见，绝不裁掉内容 —— 鸿蒙侧窗口
 * 已在 EntryAbility 里锁成 16:9，contain 的结果就是刚好铺满（无黑边、无裁切）。
 * 故意不用桌面端的 cover：窗口一旦被拖成非 16:9，cover 会把左侧导航栏直接裁掉。
 * 竖屏（手机）保留旋转 90° 的处理。
 *
 * 与内置 scaleApp() 的对抗：
 *   内置脚本在 DOMContentLoaded / load / resize 里都会用 contain 重算，把画面改回
 *   letterbox。这里三管齐下 —— 替换 window.scaleApp（后续内置 resize 回调直接走我们的）、
 *   监听 resize、再用 MutationObserver 兜住内置的每一次 style 改写。
 */
export const WEB_SCALE_JS: string = `
(function () {
  if (window.__HARMONY_SCALE__) return;
  window.__HARMONY_SCALE__ = true;

  var TV_W = 1920;
  var TV_H = 1080;
  var RAF = window.requestAnimationFrame ? window.requestAnimationFrame.bind(window) : function (fn) {
    return setTimeout(fn, 16);
  };

  /** 真实视口。window.innerWidth 已被 index.html 改写成 1920，只能走 documentElement。 */
  function viewport() {
    var el = document.documentElement;
    var w = el ? el.clientWidth : 0;
    var h = el ? el.clientHeight : 0;
    if (!w || !h) return null;
    return { w: w, h: h };
  }

  function fitScale(vp) {
    // 竖屏时舞台旋转 90°，视觉尺寸是 TV_H x TV_W
    if (vp.h > vp.w) return Math.min(vp.w / TV_H, vp.h / TV_W);
    return Math.min(vp.w / TV_W, vp.h / TV_H);
  }

  function applyScale() {
    var c = document.getElementById('main-container');
    if (!c) return;
    var vp = viewport();
    if (!vp) return;
    var rotate = vp.h > vp.w ? 'rotate(90deg) ' : '';
    var scale = fitScale(vp);
    c.style.transformOrigin = 'center center';
    c.style.transform = rotate + 'scale(' + scale + ')';
    // 用 left/top 50% + 负 margin 让容器中心对齐视口中心，旋转/缩放都不会偏移
    c.style.left = '50%';
    c.style.top = '50%';
    c.style.marginLeft = (-TV_W / 2) + 'px';
    c.style.marginTop = (-TV_H / 2) + 'px';
    c.style.overflow = 'hidden';
    c.style.visibility = 'visible';
    window.__HARMONY_SCALE_VALUE__ = scale;
    ensureGuard();
  }

  function schedule() {
    RAF(function () { RAF(applyScale); });
  }

  /** 内置 scaleApp 被内置脚本 hoist 定义后仍会覆盖我们的赋值，所以每次就绪都再抢一次。 */
  function takeOverScaleApp() {
    window.scaleApp = applyScale;
  }

  /** 幂等：快应用切换/刷新时容器可能被重建，重新挂到新节点上 */
  function ensureGuard() {
    var c = document.getElementById('main-container');
    if (!c) return;
    if (window.__HARMONY_SCALE_MO__ && window.__HARMONY_SCALE_TARGET__ === c) return;
    if (window.__HARMONY_SCALE_MO__) {
      try { window.__HARMONY_SCALE_MO__.disconnect(); } catch (e) {}
    }
    window.__HARMONY_SCALE_TARGET__ = c;
    var mo = new MutationObserver(function () {
      var vp = viewport();
      if (!vp) return;
      var expected = fitScale(vp);
      var current = window.__HARMONY_SCALE_VALUE__;
      var t = c.style.transform || '';
      var m = t.match(/scale\\(([0-9.]+)\\)/);
      var applied = m ? parseFloat(m[1]) : null;
      // 被内置 scaleApp 改成别的比例或 transform 缺失 → 重新按当前视口重算
      if (applied === null || current === undefined || Math.abs(applied - expected) > 0.001) {
        applyScale();
      }
    });
    mo.observe(c, { attributes: true, attributeFilter: ['style'] });
    window.__HARMONY_SCALE_MO__ = mo;
  }

  function boot() {
    takeOverScaleApp();
    applyScale();
    schedule();
    // 内置脚本的 DOMContentLoaded 回调注册得比我们晚、执行得比我们晚，
    // 用 0ms 定时器把接管压到它之后，之后内置的 resize 回调就直接走我们的实现了
    setTimeout(takeOverScaleApp, 0);
  }

  window.addEventListener('resize', function () {
    takeOverScaleApp();
    schedule();
  });
  window.addEventListener('orientationchange', function () {
    takeOverScaleApp();
    schedule();
  });

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', boot);
  } else {
    boot();
  }
  // 内置脚本在 load 时还会再跑一次 contain，补两刀
  window.addEventListener('load', function () {
    takeOverScaleApp();
    schedule();
  });
  RAF(applyScale);
  setTimeout(function () {
    takeOverScaleApp();
    applyScale();
  }, 2000);
})();
`;
