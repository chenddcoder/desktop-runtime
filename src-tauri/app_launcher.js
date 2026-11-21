/**
 * 桌面端应用切换器（注入脚本）。
 *
 * ## 为什么需要它
 * desktop-runtime 是「单应用壳」：窗口加载 web-runtime，再由 `ui_scale.js` 把
 * `es-app.config.json` 里的 esPackage 注入成 `?es_pkg=`，一个进程只跑一个快应用。
 * 于是「在投屏显象里看留言板」就没有入口了 —— 除非重新打包一个 exe。
 *
 * 这里在**不改 web-runtime 首屏**的前提下补一个切换器：
 * 读后台的公开目录 `/api/runtime/catalog`（已发布应用），点一个就换 `?es_pkg=` 重载。
 *
 * ## 为什么不改 web-runtime/index.html
 * 那是网页端（runtime.chenddcoder.cn）也在用的产物。把桌面入口写进去，
 * 等于让网页端也长出一个「切换应用」的按钮，反而更割裂。
 * 这个脚本只在 Tauri 窗口里注入，网页端一行不受影响 —— 端差异留在端里。
 *
 * ## 位置约定（⛔ 不许和已有浮层叠一起）
 * · 左上 `web-back-btn`：20,20 起，48×48
 * · 右上 `controls-panel`（开发者面板）：right 10 / top 10
 * · 右下 DLNA 版本角标（dlna_overlay.js）
 * 所以切换器放 **left:20 / top:84**（back 正下方，竖排成一组），面板向下展开 340px 宽。
 * 面板展开时会盖住应用内容 —— 这是**有意**的临时遮挡（点了就关），不是常驻叠放。
 */
;(function () {
  if (window.__APP_LAUNCHER__) return
  window.__APP_LAUNCHER__ = true

  var CATALOG_PATH = '/api/runtime/catalog'

  /** 后台基址：main.rs 里锁死过 __CROSS_APP_RESOLVER_BASE__，这里保持一致 */
  function base() {
    try {
      var b = window.__CROSS_APP_RESOLVER_BASE__
      if (typeof b === 'string' && /^https?:\/\//i.test(b)) return b.replace(/\/+$/, '')
    } catch (e) {}
    return location.origin
  }

  function currentPkg() {
    try {
      return new URLSearchParams(location.search).get('es_pkg') || ''
    } catch (e) {
      return ''
    }
  }

  var btn = null
  var panel = null
  var listEl = null
  var loaded = false
  var loading = false

  function el(tag, css, html) {
    var n = document.createElement(tag)
    if (css) n.setAttribute('style', css)
    if (html != null) n.innerHTML = html
    return n
  }

  function build() {
    btn = el(
      'button',
      [
        'position:fixed',
        'left:20px',
        'top:84px',
        'z-index:99998',
        'width:48px',
        'height:48px',
        'background:rgba(255,255,255,0.10)',
        'border:2px solid rgba(255,255,255,0.30)',
        'border-radius:50%',
        'cursor:pointer',
        'display:flex',
        'align-items:center',
        'justify-content:center',
        'backdrop-filter:blur(10px)',
        'transition:background .2s ease,border-color .2s ease',
        'padding:0',
        'outline:none',
      ].join(';'),
      // 四宫格
      '<svg viewBox="0 0 24 24" width="22" height="22" fill="none" stroke="rgba(255,255,255,0.92)" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">' +
        '<rect x="3" y="3" width="7" height="7" rx="1.5"/><rect x="14" y="3" width="7" height="7" rx="1.5"/>' +
        '<rect x="3" y="14" width="7" height="7" rx="1.5"/><rect x="14" y="14" width="7" height="7" rx="1.5"/></svg>'
    )
    btn.title = '切换应用'
    btn.onmouseenter = function () {
      btn.style.background = 'rgba(255,255,255,0.18)'
      btn.style.borderColor = 'rgba(255,255,255,0.45)'
    }
    btn.onmouseleave = function () {
      btn.style.background = 'rgba(255,255,255,0.10)'
      btn.style.borderColor = 'rgba(255,255,255,0.30)'
    }
    btn.onclick = function (e) {
      e.stopPropagation()
      toggle()
    }

    panel = el(
      'div',
      [
        'position:fixed',
        'left:20px',
        'top:142px',
        'z-index:99998',
        'width:340px',
        'max-height:62vh',
        'overflow-y:auto',
        'background:rgba(22,19,17,0.94)',
        'border:1px solid rgba(255,255,255,0.14)',
        'border-radius:14px',
        'box-shadow:0 12px 40px rgba(0,0,0,0.45)',
        'backdrop-filter:blur(16px)',
        'padding:14px',
        'display:none',
        'font-family:-apple-system,BlinkMacSystemFont,"PingFang SC","Microsoft YaHei",sans-serif',
        'color:#f2ece4',
        'box-sizing:border-box',
      ].join(';')
    )

    var head = el(
      'div',
      'display:flex;align-items:center;justify-content:space-between;margin-bottom:10px;',
      '<span style="font-size:13px;letter-spacing:.08em;color:rgba(242,236,228,0.62);">应用</span>'
    )
    var close = el(
      'button',
      'background:none;border:none;color:rgba(242,236,228,0.55);font-size:18px;line-height:1;cursor:pointer;padding:2px 6px;',
      '×'
    )
    close.onclick = function () {
      hide()
    }
    head.appendChild(close)
    panel.appendChild(head)

    listEl = el('div', 'display:flex;flex-direction:column;gap:6px;')
    panel.appendChild(listEl)
    panel.onclick = function (e) {
      e.stopPropagation()
    }

    document.body.appendChild(btn)
    document.body.appendChild(panel)

    document.addEventListener('click', hide, true)
    document.addEventListener('keydown', function (e) {
      if (e.key === 'Escape') hide()
    })
  }

  function show() {
    panel.style.display = 'block'
    if (!loaded && !loading) load()
  }

  function hide() {
    if (panel) panel.style.display = 'none'
  }

  function toggle() {
    if (!panel) return
    if (panel.style.display === 'none') show()
    else hide()
  }

  function render(items) {
    listEl.innerHTML = ''
    var cur = currentPkg()
    if (!items.length) {
      listEl.appendChild(
        el('div', 'font-size:13px;color:rgba(242,236,228,0.5);padding:10px 4px;', '后台暂无已发布的应用')
      )
      return
    }
    items.forEach(function (app) {
      var pkg = app.packageName || ''
      var isCur = pkg && pkg === cur
      var row = el(
        'div',
        [
          'display:flex',
          'align-items:center',
          'gap:10px',
          'padding:8px 10px',
          'border-radius:10px',
          'cursor:pointer',
          'background:' + (isCur ? 'rgba(228,99,63,0.16)' : 'rgba(255,255,255,0.04)'),
          'border:1px solid ' + (isCur ? 'rgba(228,99,63,0.55)' : 'transparent'),
          'transition:background .15s ease',
        ].join(';')
      )

      var iconBox = el(
        'div',
        'width:36px;height:36px;flex:0 0 36px;border-radius:9px;overflow:hidden;background:rgba(255,255,255,0.08);display:flex;align-items:center;justify-content:center;'
      )
      if (app.icon) {
        var img = el('img', 'width:100%;height:100%;object-fit:contain;')
        img.src = app.icon
        img.onerror = function () {
          iconBox.textContent = '·'
        }
        iconBox.appendChild(img)
      } else {
        iconBox.textContent = '·'
      }

      var text = el(
        'div',
        'min-width:0;flex:1;',
        '<div style="font-size:14px;color:#f2ece4;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;">' +
          escapeHtml(app.appName || pkg) +
          '</div>' +
          '<div style="font-size:11px;color:rgba(242,236,228,0.45);margin-top:2px;">' +
          escapeHtml(pkg) +
          (app.version ? ' · v' + escapeHtml(app.version) : '') +
          '</div>'
      )

      row.appendChild(iconBox)
      row.appendChild(text)
      if (isCur) {
        row.appendChild(
          el('span', 'font-size:11px;color:#e4633f;flex:0 0 auto;', '当前')
        )
      }

      row.onmouseenter = function () {
        if (!isCur) row.style.background = 'rgba(255,255,255,0.09)'
      }
      row.onmouseleave = function () {
        if (!isCur) row.style.background = 'rgba(255,255,255,0.04)'
      }
      row.onclick = function () {
        if (!pkg || isCur) {
          hide()
          return
        }
        // 切包 = 换 es_pkg 重载。web-runtime 的 loader 原生支持这个参数，
        // 会重新走 resolve → 下载 → 解压 → 挂载，不需要我们干预加载细节。
        location.href = location.pathname + '?es_pkg=' + encodeURIComponent(pkg)
      }

      listEl.appendChild(row)
    })
  }

  function escapeHtml(s) {
    return String(s == null ? '' : s).replace(/[&<>"']/g, function (c) {
      return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]
    })
  }

  function load() {
    loading = true
    listEl.innerHTML = '<div style="font-size:13px;color:rgba(242,236,228,0.5);padding:10px 4px;">加载中…</div>'
    fetch(base() + CATALOG_PATH)
      .then(function (r) {
        // ⛔ web-runtime 里的 fetch 不是标准 Response：r.json() 通常可用，
        //    但 r.ok / headers 不可靠（2026-11-07 实测）。只认 body。
        return r.text()
      })
      .then(function (t) {
        var json = JSON.parse(t)
        var items = (json && json.data) || []
        // 只留快应用包（有 packageName 的），并按名字排一下，避免后台顺序变化带来跳动
        items = items
          .filter(function (x) {
            return x && x.packageName
          })
          .sort(function (a, b) {
            return String(a.appName || a.packageName).localeCompare(String(b.appName || b.packageName), 'zh')
          })
        loaded = true
        render(items)
      })
      .catch(function (e) {
        listEl.innerHTML =
          '<div style="font-size:13px;color:rgba(242,236,228,0.55);padding:10px 4px;line-height:1.6;">' +
          '拉取应用列表失败<br><span style="font-size:11px;opacity:.7;">' +
          escapeHtml((e && e.message) || e) +
          '</span></div>'
      })
      .then(function () {
        loading = false
      })
  }

  function init() {
    if (!document.body) {
      setTimeout(init, 50)
      return
    }
    if (document.getElementById('web-back-btn') === null && document.readyState !== 'complete') {
      // web-runtime 的宿主 UI 是 index.html 里就有的，正常不会缺；
      // 只在 DOM 还没就绪时稍等，避免把按钮插到 <body> 之外。
      setTimeout(init, 50)
      return
    }
    build()
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init)
  } else {
    init()
  }
})()
