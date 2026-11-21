/**
 * 内存版 IndexedDB —— **只用于无头验收**。
 *
 * 为什么需要：本机 Security Center 会拦 Chrome 的所有临时文件删除，IndexedDB 的
 * LevelDB 后端因此打不开，web-runtime 直接报
 * `Internal error opening backing store for indexedDB.open`，整条 es_pkg 加载链断掉。
 * 这个 mock 用内存 Map 顶替 IDB，行为覆盖 loader 真正用到的 5 个 API
 * （open / createObjectStore / transaction / objectStore / put / get / delete）。
 *
 * ⛔ 不要打进产品：真实桌面（WKWebView / WebView2）的 IndexedDB 是好的，
 *    掩掉它反而丢掉「离线缓存包」这个能力。
 */
(function () {
  if (window.__ES_FAKE_IDB__) return
  window.__ES_FAKE_IDB__ = true

  var dbs = Object.create(null)

  function makeRequest() {
    return { result: undefined, error: null, onsuccess: null, onerror: null, onupgradeneeded: null }
  }

  function later(fn) {
    setTimeout(fn, 0)
  }

  function makeDB(name) {
    var stores = Object.create(null)
    var db = {
      name: name,
      stores: stores,
      createObjectStore: function (n) {
        if (!stores[n]) stores[n] = new Map()
        return { name: n }
      },
      transaction: function (storeName) {
        var tx = { mode: 'readwrite', oncomplete: null, onerror: null, error: null }
        tx.objectStore = function () {
          if (!stores[storeName]) stores[storeName] = new Map()
          var m = stores[storeName]
          return {
            put: function (v, k) {
              m.set(k, v)
              later(function () {
                if (tx.oncomplete) tx.oncomplete({ target: tx })
              })
              return makeRequest()
            },
            get: function (k) {
              var q = makeRequest()
              later(function () {
                q.result = m.get(k)
                if (q.onsuccess) q.onsuccess({ target: q })
              })
              return q
            },
            delete: function (k) {
              m.delete(k)
              later(function () {
                if (tx.oncomplete) tx.oncomplete({ target: tx })
              })
              return makeRequest()
            },
            clear: function () {
              m.clear()
              later(function () {
                if (tx.oncomplete) tx.oncomplete({ target: tx })
              })
              return makeRequest()
            },
          }
        }
        return tx
      },
      close: function () {},
    }
    // loader 用 db.objectStoreNames.contains(...)
    db.objectStoreNames = {
      contains: function (n) {
        return !!stores[n]
      },
    }
    return db
  }

  /*
   * ⛔ 必须 defineProperty，不能直接赋值：
   *    `window.indexedDB` 在 Chrome 里是**只有 getter 没有 setter** 的访问器属性，
   *    非严格模式下 `window.indexedDB = fake` 静默失败，页面照旧用原生 IDB
   *    （表现：注入"成功"，但报的仍是 Internal error opening backing store）。
   */
  var fake = {
    open: function (name) {
      var r = makeRequest()
      var fresh = !dbs[name]
      if (fresh) dbs[name] = makeDB(name)
      var db = dbs[name]
      later(function () {
        if (fresh && r.onupgradeneeded) {
          r.result = db
          r.onupgradeneeded({ target: r })
        }
      })
      setTimeout(function () {
        r.result = db
        if (r.onsuccess) r.onsuccess({ target: r })
      }, 5)
      return r
    },
    deleteDatabase: function (name) {
      delete dbs[name]
      var r = makeRequest()
      later(function () {
        if (r.onsuccess) r.onsuccess({ target: r })
      })
      return r
    },
  }

  try {
    Object.defineProperty(window, 'indexedDB', {
      configurable: true,
      get: function () {
        return fake
      },
    })
  } catch (e) {
    try {
      Object.defineProperty(window, 'indexedDB', { configurable: true, value: fake })
    } catch (e2) {
      console.error('[verify] 无法替换 indexedDB：', e2)
    }
  }
  console.log('[verify] 已启用内存版 IndexedDB（无头验收专用）')
})();
