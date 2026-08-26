// 投屏「扫码看广告解锁」叠层（由 desktop-runtime 注入到 web-runtime 页面，不改 web-runtime 本体）。
//
// 职责：
//   ① 暴露 window.VueAiUnlock.show(featureKey, onUnlocked) 给投屏业务页面调用。
//   ② 调用 vue-ai-server 生成解锁会话，弹出二维码（扫码打开小程序 pages/unlock）。
//   ③ 轮询解锁状态，done 后回调 onUnlocked 并关闭弹层。
//
// 与 dlna_overlay.js 同机制：initialization_script 注入，框架级不改业务代码。
// 后端地址走绝对 https（webview 本地页无法用相对路径），后端已 @CrossOrigin 放行。

(function () {
  if (window.__vueAiUnlockInstalled) return;
  window.__vueAiUnlockInstalled = true;

  var API_BASE = 'https://www.chenddcoder.cn';
  var QR_IMG = 'https://api.qrserver.com/v1/create-qr-code/?size=260x260&data=';

  var overlayEl = null;
  var pollTimer = null;
  var currentSession = '';
  var currentCb = null;

  function log(tag, msg) {
    console.log('[ad_unlock_overlay] ' + tag + ' | ' + msg);
  }

  function ensureOverlay() {
    if (overlayEl) return overlayEl;
    var mask = document.createElement('div');
    mask.id = '__ad_unlock_mask';
    mask.style.cssText =
      'position:fixed;inset:0;background:rgba(0,0,0,0.6);z-index:2147483647;' +
      'display:none;align-items:center;justify-content:center;' +
      'font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,sans-serif;';
    mask.innerHTML =
      '<div style="background:#fff;color:#222;border-radius:14px;padding:24px 28px;width:320px;max-width:86vw;' +
      'box-shadow:0 12px 40px rgba(0,0,0,0.35);text-align:center;">' +
      '<div style="font-size:16px;font-weight:600;margin-bottom:6px;">解锁高级功能</div>' +
      '<div id="__ad_unlock_tip" style="font-size:12px;color:#888;margin-bottom:14px;">请使用微信小程序扫码看广告解锁</div>' +
      '<img id="__ad_unlock_qr" style="width:240px;height:240px;border:1px solid #eee;border-radius:8px;" />' +
      '<div style="margin-top:12px;font-size:12px;color:#aaa;">打开「VUE编辑小站」小程序扫一扫</div>' +
      '<button id="__ad_unlock_close" style="margin-top:14px;width:100%;padding:8px 0;border:none;border-radius:8px;' +
      'background:#eee;color:#555;font-size:13px;cursor:pointer;">取消</button>' +
      '</div>';
    document.body.appendChild(mask);
    overlayEl = mask;
    mask.querySelector('#__ad_unlock_close').onclick = hide;
    return mask;
  }

  function show(featureKey, onUnlocked) {
    var api = ensureOverlay();
    currentCb = typeof onUnlocked === 'function' ? onUnlocked : null;
    api.querySelector('#__ad_unlock_tip').textContent = '正在生成二维码…';
    api.style.display = 'flex';

    fetch(API_BASE + '/api/ad/session', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ featureKey: featureKey || '' })
    })
      .then(function (r) { return r.json(); })
      .then(function (data) {
        if (!data || !data.session) {
          api.querySelector('#__ad_unlock_tip').textContent = '生成失败，请重试';
          return;
        }
        currentSession = data.session;
        api.querySelector('#__ad_unlock_qr').src = QR_IMG + encodeURIComponent(data.qrContent);
        api.querySelector('#__ad_unlock_tip').textContent = '请使用微信小程序扫码看广告解锁';
        startPoll();
      })
      .catch(function () {
        api.querySelector('#__ad_unlock_tip').textContent = '网络错误，请重试';
      });
    log('show', 'featureKey=' + (featureKey || ''));
  }

  function startPoll() {
    stopPoll();
    pollTimer = setInterval(function () {
      fetch(API_BASE + '/api/ad/status?session=' + encodeURIComponent(currentSession))
        .then(function (r) { return r.json(); })
        .then(function (st) {
          if (st.status === 'done') {
            stopPoll();
            var cb = currentCb;
            hide();
            log('unlocked', 'session=' + currentSession);
            if (cb) cb();
          } else if (st.status === 'not_found') {
            stopPoll();
            if (overlayEl) overlayEl.querySelector('#__ad_unlock_tip').textContent = '二维码已失效，请重新获取';
          }
        })
        .catch(function () {});
    }, 2000);
  }

  function stopPoll() {
    if (pollTimer) { clearInterval(pollTimer); pollTimer = null; }
  }

  function hide() {
    stopPoll();
    if (overlayEl) overlayEl.style.display = 'none';
  }

  window.VueAiUnlock = { show: show, hide: hide };
  log('install', 'VueAiUnlock ready (call VueAiUnlock.show(featureKey, cb))');
})();
