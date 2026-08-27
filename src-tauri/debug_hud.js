// 调试 HUD（仅 desktop-runtime 的 DEBUG 构建经 initialization_script 注入；
// release 构建不编译此文件，故发布版不包含任何调试浮层）。
// 内容：① 右下角 dev log 面板 ② 左上角 DLNA 状态条。
(function () {
  if (window.__debugHudInstalled) return;
  window.__debugHudInstalled = true;
  // 与 dlna_overlay 共享同一份日志数组（dlog 推入 window.__LOG__）
  var LOG = window.__LOG__ || (window.__LOG__ = []);
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

  // 挂到 window：debug 构建注入后才生效；release 构建不注入本文件，状态条保持空实现。
  window.ensurePanelLoop = function () {
    buildPanel();
    setInterval(render, 500);
    // QR 诊断已暂时关闭（用户切到 DevTools 排查 encrypt-proxy 问题）。
    // 需要重新扫描时取消下面这行注释即可：
    // setInterval(scanQrElements, 2000);
  };

  // 在页面顶部贴一条 DLNA 状态条（不依赖 web-runtime 注入）
  var dlnaBadge;
  window.showDlnaStatus = function (state, info) {
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
  };
  // 启动面板（release 里 dlna_overlay 的 ensurePanelLoop 是空实现，这里才是真的）
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', function () { ensurePanelLoop(); });
  } else { ensurePanelLoop(); }
})();
