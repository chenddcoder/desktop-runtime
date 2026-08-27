# web-runtime Tauri 环境适配 + invoke(reqwest) 取代桌面注入代理

> 日期：2026-08-27
> 范围：`quicktvui/packages/web-renderer`（web-runtime 核心源码）+ `desktop-runtime/src-tauri`（桌面外壳）

## 1. 背景与目标

当前 desktop-runtime 通过**注入脚本**让 web-runtime 在桌面可用：

- `proxy_fetch.js`（document-start 注入）全局包裹 fetch/XHR，命中 `quicktvui.com / chenddcoder.cn / open-meteo.com` 的请求 invoke Rust `proxy_http`（reqwest）发出，绕过 CORS。覆盖 es_pkg 的 resolve / zip 下载与包内业务 API。
- `media_proxy.js`（document-start 注入）hook video/audio src 改写为 `127.0.0.1:5200`，由 Rust `media_proxy.rs`（reqwest 流式）处理抖音 302 一次性签名调度链。

目标：**移除注入脚本式的代理方式**，把"判断 Tauri 环境 → invoke 调 Rust（reqwest）→ 非 Tauri 走原逻辑"内建到 web-runtime 源码，并收拢已多处使用的环境适配代码（ProcessBridgeModule / EsNativeModule 中的 `isRustEnv` / `invokeTauri`）。

## 2. 关键决策

| 决策点 | 结论 |
|--------|------|
| Rust `proxy_http` 命令 | 保留不变（本身已基于 reqwest） |
| Rust 媒体服务 `media_proxy.rs`(127.0.0.1:5200) | 保留不变（本身已基于 reqwest stream）。`<video>` 是持续 Range 流式消费，Tauri invoke 是一次性返回值，无法流式喂给 video；直连原 URL 会复现抖音 302 链卡进度 bug |
| 业务 API 跨域请求 | 统一走 invoke 通道（autoProxy 拦截器中 Tauri 分支），彻底移除 `proxy_fetch.js` |
| 媒体 src 改写 | 从 `media_proxy.js` 内置到 web-renderer 源码（仅 Tauri 环境挂载），移除注入脚本 |
| `window.__get_invoke` | 由新模块 `initTauriBridge()` 挂载（原由 proxy_fetch.js 提供，保持向后兼容） |
| 非 Tauri 环境（浏览器预览 / 线上 runtime / TV） | 所有新分支以 `isTauriEnv()` 短路，行为完全不变 |

## 3. 模块设计

### 3.1 新增 `web-renderer/src/core/tauriEnv.js`（统一环境适配 + HTTP 请求封装）

导出：

- `getTauri()`：返回 `window.__TAURI__ || null`
- `isTauriEnv()`：`__TAURI__.core` 或顶层 `invoke` 存在（v1/v2 兼容）
- `getInvoke()`：懒获取 invoke 函数（v2 `core.invoke`，v1 `invoke`）
- `initTauriBridge()`：把 `getInvoke` 挂到 `window.__get_invoke`（兼容既有使用点）
- `waitTauriReady(timeout)`：轮询等待 Tauri invoke 就绪的 Promise（withGlobalTauri 注入晚于 document-start）
- `isPrivateHost(host)`：私有/本地网段判断（防 SSRF，从 proxy_fetch.js 迁移）
- `isProxyTargetHost(host)`：代理目标域名判断（quicktvui.com / chenddcoder.cn / open-meteo.com，迁移）
- `requestByEnv({method, url, headers, bodyBytes, timeout})`：统一 HTTP 请求
  - Tauri 环境 → `invoke('proxy_http', {req:{url, method, headers, body_base64}})` → base64 解码 → 返回 `{status, statusText, headers, data: ArrayBuffer}`（对齐 CrossAppResolver.xhrRequest 契约）
  - 非 Tauri 环境 → 原生 XHR 实现（保留 `__skipAutoProxy` 语义，供 CrossAppResolver 使用）

### 3.2 新增 `web-renderer/src/adapters/mediaProxyAdapter.js`

- `installMediaProxyAdapter()`：仅 Tauri 环境由 loader.js 调用。逻辑迁自 desktop-runtime `media_proxy.js`：
  - hook `HTMLMediaElement.prototype.src` setter + `setAttribute('src')` 兜底
  - 远程 http(s) 且非私有/回环的 src → 改写为 `http://127.0.0.1:<port>/media?url=<encoded>`（port 读 `window.__MEDIA_PROXY_PORT__ || 5200`）
  - 已改写不重复包裹；`error` 事件回退直连一次
- 安全兜底 `isMediaProxyUrl(u)` 辅助导出

### 3.3 `CrossAppResolver.js`（es_pkg resolve / zip 下载）

`xhrRequest` 改为调用 `requestByEnv`：Tauri 环境自动走 Rust reqwest；非 Tauri 保持原 XHR（`__skipAutoProxy` 行为不变）。resolve POST 加密 payload / zip GET 均在 Tauri 环境免 CORS。

### 3.4 `autoProxy.js`（包内业务 API）

- fetch 拦截：计算 url/method/headers/body 后，优先 `if (isTauriEnv() && shouldProxy(url))` → `requestByEnv` → 包装为兼容 Response（复用 createProxyResponse）
- XHR 拦截：open() 中对 Tauri 环境且 shouldProxy 的请求记录 `__rustMeta`，send() 中走 `requestByEnv` 并模拟响应属性/事件（复用 encrypt 分支的模拟逻辑骨架）
- 非 Tauri 分支不变（DevServer `/proxy`、加密通道、直连）

### 3.5 `loader.js`（web-runtime 入口）

- 从 web-renderer src 导入 `initTauriBridge / isTauriEnv / waitTauriReady / installMediaProxyAdapter`
- 主体初始化：`initTauriBridge()`；`if (isTauriEnv()) { waitTauriReady(); installMediaProxyAdapter(); }`
- 现有 es_pkg / zip / bundle 加载链路不动

### 3.6 既有使用点收拢

- `ProcessBridgeModule.js`：`isRustEnv()/invokeTauri` 改从 `../../core/tauriEnv` 导入（行为不变）
- `EsNativeModule.js` `sendRemoteEvent`：改从 `../core/tauriEnv` 导入 getInvoke

### 3.7 `desktop-runtime/src-tauri/src/main.rs`

- 删除 `PROXY_FETCH_JS`、`MEDIA_PROXY_JS` 常量与对应 `initialization_script` 注入
- 保留：`media_proxy.rs` 服务 spawn、`window.__MEDIA_PROXY_PORT__` 注入、`ui_scale.js`、dlna、`proxy::proxy_http` 命令注册

## 4. 数据流（改造后，Tauri 环境）

```
es_pkg 加载:  getPackageInfo(POST resolve) ─┐
             downloadAndDecryptPackage(GET zip) ─┤ requestByEnv → invoke('proxy_http') → Rust reqwest
包内业务 API: fetch/XHR → autoProxy tauri 分支 ──┘
video/audio:  src(远程) → installMediaProxyAdapter 改写 → http://127.0.0.1:5200/media?url= → media_proxy.rs(reqwest 流式)
```

非 Tauri 环境全部走原路径，无任何变化。

## 5. 风险与兼容

- **时序**：`requestByEnv` 在 invoke 未就绪时应返回失败/降级（走原生将撞 CORS，故 Tauri 分支在 invoke 不可用时 reject 并给出明确错误），`waitTauriReady` 兜底。
- **大文件**：zip 下载经 base64 膨胀约 33%（与 proxy_fetch.js 现状一致，不扩大范围）。
- **向后兼容**：`window.__get_invoke` 仍由 initTauriBridge 挂载，旧注入脚本/模块不破坏。
- **非 Tauri 回归**：全部新逻辑短路，浏览器 39002 / 线上 runtime / TV 行为不变。

## 6. 验证

1. `web-renderer` 相关单测（CrossAppResolver 既有测试，新增 __TAURI__ 模拟用例）
2. `pnpm --filter web-runtime build` 构建通过
3. `desktop-runtime` `cargo check` 零错误
4. 真机（用户本机 GUI）实测：es_pkg 加载（resolve + zip）、投放抖音播放、天气应用