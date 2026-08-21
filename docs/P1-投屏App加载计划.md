# 第一阶段（P1）实施计划（决策版）：desktop-runtime 默认加载 xiaoyoucast + 真实 DLNA 投屏

> 已确认两项决策：① 默认加载走 **`es_pkg` 机制（路径 B）**；② 第一阶段就要做**真实 DLNA 投屏**（非仅模块可用）。
> 为此把第一阶段拆为 **P1a（默认加载 + UI 跑通）** 与 **P1b（Rust DLNA 真实投屏）**。前端零改动复用 `web-runtime/dist`。

---

## 1. 目标边界（决策后）

| 属于本阶段 | 备注 |
|-----------|------|
| P1a：内置 xiaoyoucast zip，启动即默认加载 `es.tv.huan.escast` | 走 `es_pkg`（路径 B） |
| P1a：xiaoyoucast UI 完整渲染（TV 16:9 画布） | 含 DLNA 设备名显示 |
| P1a：DLNA 相关 web 模块（ProcessBridgeModule 等）加载可用 | 设备名读写 |
| P1b：Rust 实现 SSDP/UPnP，**真实局域网投屏到 DLNA 设备** | 端到端验证 |
| 不属于本阶段 | 备注 |
| ❌ Windows 构建 / 签名公证 | P3 / P4 |
| ❌ 服务端投屏码体系完整打通 | 依赖线上服务，桌面端先走本地 DLNA 直投 |

---

## 2. 现状盘点（已核实）

1. **资源**：`esapp-xiaoyoucast/dist/android.zip`，主入口 `index.android.js`，pkg 标识 `es.tv.huan.escast`（`src/config/other-config.ts:28` 明确 `packageName: 'es.tv.huan.escast'`）。
2. **loader.js 加载机制**（`packages/web-runtime/src/loader.js`）：
   - `?es_pkg=`（1940 行）：按包名从 IndexedDB（`key=pkg:{esPackage}`）加载；`getPackageInfo` 网络 resolve 失败会**降级用本地缓存**。
   - `?zipUrl=`（2128 行）：XHR 下载→JSZip 解压→`loadBundlesFromZip`。
   - 无参数（2156 行 `else`）：显示「请上传 ZIP 包开始预览」（当前空白态）。
   - 全局 API：`window.__VIRTUAL_FS__`（含 `loadFromData`）；`saveZipToDB`/`loadZipBytesToVirtualFS` 未暴露全局。
3. **投屏插件已构建进 web-runtime/dist**（web-renderer 注册 `pluginModules`）：`xy-process-bridge`→`ProcessBridgeModule`（设备名 get/set）、`xy-ip`、`xy-huawei-cast`、`xy-device-dum`。
4. **关键发现（影响 P1b）**：xiaoyoucast web 层**不直接调用 `DlnaModule` 的投屏方法**——`src` 里仅 3 处调 `DlnaModule`（都是 `setIpWhiteListModel/addIpToWhiteList/removeIpFromWhiteList`，见 `tools/index.ts:378/391/404`）。真正的投屏由 Android 原生底座 `com.extscreen.runtime.dlna.DlnaModule`（完整 DLNA 协议栈）完成。web-renderer **没有 `DlnaModule` 的 web 适配**。

> 结论：P1a 复用 `es_pkg` 机制即可；P1b 的「真实投屏」是**桌面端独立新增能力**（Rust DLNA 客户端 + 对接投屏触发点），不是简单桥接现有调用。

---

## 3. P1a：默认加载 es.tv.huan.escast（路径 B — es_pkg + 预置缓存）

### 原理
`es_pkg` 分支：先 `loadZipFromDB(esPackage)` 读缓存 → `getPackageInfo` 网络 resolve（离线失败）→ 缓存 md5 一致或 resolve 失败则**用缓存加载**。所以只要 IndexedDB 预置了 `pkg:es.tv.huan.escast` 缓存，离线也能加载。

### 实现（零改 web-runtime 源码）
1. **内置资源**：`cp android.zip` → `src-tauri/resources/builtin/es.tv.huan.escast.zip`（纳入 git）。
2. **staging 聚合脚本** `scripts/build-frontend.mjs`（新增）：同步 `web-runtime/dist` → `.frontend-staging/`；拷贝 `resources/builtin/*.zip` → `.frontend-staging/builtin/`。`tauri.conf.json` 的 `frontendDist` 指向 `.frontend-staging`；dev `serve.mjs` 服务 staging。
3. **初始 URL**：`index.html?es_pkg=es.tv.huan.escast`（在 `tauri.conf.json` 的 `app.windows[].url` 或 Rust builder 设置）。
4. **desktop 注入脚本**（desktop-runtime 自带，随 WebView 加载后由 Rust `webview.eval()` 注入；自带 JSZip）：
   - 检测 IndexedDB `pkg:es.tv.huan.escast` 是否已预置。
   - 首次/缺失：`fetch('/builtin/es.tv.huan.escast.zip')`（同源，`tauri://localhost` serve）→ 自带 JSZip 解压 → 复制 loader.js 的写入逻辑（`openZipDB` + `store.put({files, entryFile, zipName, timestamp, packageMd5, version}, 'pkg:es.tv.huan.escast')`）→ `location.href='?es_pkg=es.tv.huan.escast'`（reload，缓存命中 → 加载成功）。
   - 后续启动：缓存命中，`es_pkg` 直接加载（resolve 失败降级）。
5. **注**：`builtin/*.zip` 由 staging serve 同源暴露，故注入脚本用 `fetch` 即可，无需额外 Rust `load_builtin_app` 命令（简化）。

### 验证清单（本机 `cargo tauri dev`）
- [ ] 窗口启动即进入 xiaoyoucast（**不再是**「请上传 ZIP 包开始预览」）
- [ ] Console 无 loader 报错；`index.android.js` 执行；`appRegister` 注册 `es.tv.huan.escast`
- [ ] DLNA 设备名可用：`Native.callNativeWithPromise('ProcessBridgeModule','getDlnaName')` 解析得到设备名
- [ ] 16:9 窗口比例锁定仍生效
- [ ] 二次启动走缓存（IndexedDB 命中，不再 reload）

---

## 4. P1b：真实 DLNA 投屏（Rust SSDP/UPnP）

> **方向修正（2026-08-10）**：原计划按「DLNA 客户端（控制器，桌面推送媒体到电视）」设计；但用户明确「我要的就是服务端啊」——即桌面端作为 **DLNA DMR 接收端（被投屏的"电视"）**，让手机/其他投屏器把视频投到这台桌面电脑上播放。故已实现的是 **DMR 服务端栈**（SSDP 被发现自己 + HTTP 设备描述 + SOAP AVTransport 控制 + 收到 Play 时全屏播放），而非客户端推送。下述「客户端」相关条目（dlna_discover/dlna_cast 推送到设备）与本阶段实际交付不符，以本注为准；若日后确需桌面推送能力，再另案处理。

### 已实现（DMR 服务端，`src-tauri/src/dlna/`）
- 模块：`device_desc.rs`（设备描述/SCPD XML）、`av_transport.rs`（AVTransport 状态机）、`soap.rs`（SOAP 解析/构造/动作处理）、`ssdp.rs`（UDP 多播发现：NOTIFY alive + 应答 M-SEARCH）、`http_server.rs`（手写 HTTP，服务描述 + 收 SOAP）、`mod.rs`（入口 + `dlna_start`/`dlna_stop` 命令）。
- 停服信号用 `broadcast`（一次唤醒 HTTP+SSDP 全部任务，避免 restart 时旧任务泄漏占着 1900 端口）；`bind_ssdp` 用 `local_ip` 作多播接口（macOS 不能用 0.0.0.0）。
- 前端：`dlna_overlay.js` 通过 `WebviewWindowBuilder.initialization_script` 注入（不改 web-runtime 本体），`withGlobalTauri:true` 下监听 `dlna://play` → 全屏 `<video>` 播放；Esc 退出。
- 编译：`cargo build` 零警告通过。**端到端需真实 DLNA 控制器（手机/投屏器）把媒体投到本机才能验，沙箱/headless 不可验。**

### 关键技术约束
- xiaoyoucast web 层不直接调 `DlnaModule` 投屏方法 → 桌面端需**独立实现 DLNA 客户端**，并提供给投屏触发点（不能以"桥接现有调用"思路做）。
- web-renderer 无 `DlnaModule` web 适配 → 需新增桌面侧 `DlnaModule` 适配，把投屏调用路由到 `invoke()`。

### Rust 侧（`src-tauri/src/dlna/`）
- **SSDP 发现**：UDP 多播 `239.255.255.250:1900`，发 `M-SEARCH` 搜索 `urn:schemas-upnp-org:device:MediaRenderer:1`，解析各设备 `location`。
- **设备描述**：拉取 `location` XML → 解析 `AVTransport` service 的 `controlURL`/`eventSubURL` + `FriendlyName` + `UDN`。
- **投屏控制**（SOAP/UPnP）：`SetAVTransportURI(uri)` + `Play`。
- **状态查询**（可选）：`GetTransportInfo` / `GetPositionInfo`。
- **Tauri commands**：`dlna_discover()→device[]`、`dlna_cast(deviceId, mediaUrl)`、`dlna_stop(deviceId)`、`dlna_get_name()`。

### 前端桥接
- 新增 `DlnaModule` web 适配（desktop-runtime 自带，注入或并入 web-renderer 均可）：把 xiaoyoucast 投屏触发路由到上述 `invoke`。
- **P1b 第 0 步（必须先做）**：定位 xiaoyoucast 投屏按钮 handler / `cast-api`（`src/api/cast-api/`、`src/pages/*` 投屏入口）的触发路径，确定如何注入桌面 DLNA 能力（hook 投屏调用 / 提供上层投屏 API / 调试入口）。

### 验证依赖（重要，需如实告知）
- 真实投屏**必须接真实 DLNA 接收设备（支持 DLNA 的电视/盒子）**才能端到端验证；**沙箱 / headless 无法验证**（本助手环境无 GUI、无局域网设备）。
- 建议 P1b 先做**最小可用验证**：Rust DLNA 客户端 + 桌面调试面板（选设备 + 输入视频 URL + 投屏），验证「发现→推送→电视播放」全链路；再对接 xiaoyoucast 投屏按钮。

---

## 5. 风险与坑

| 风险 | 说明 / 处置 |
|------|------------|
| B 路径首次 reload | 首次启动注入脚本预置缓存后会 reload 一次（缓存命中后正常），可接受 |
| IndexedDB schema 耦合 | 注入脚本写入的 key/value 结构需与 loader.js `saveZipToDB` 一致（受控，仅写不读内部逻辑） |
| staging 同步 | `frontendDist` 指向 `.frontend-staging` 后，web-renderer 改动需重跑 `build-frontend` |
| 真实投屏验证依赖硬件 | 需真实 DLNA 设备；沙箱不可验，留待你本机局域网验证 |
| IP 白名单等非核心 | `DlnaModule` 的 `setIpWhiteList*` 在桌面端可降级为 no-op（不影响核心投屏） |
| xiaoyoucast 投屏触发点未知 | P1b 第 0 步必须先调研，避免闷头实现后对接不上 |

---

## 6. 实施步骤总表

**P1a（默认加载）**
| # | 步骤 | 改动 |
|---|------|------|
| 1 | 内置资源入库 | `src-tauri/resources/builtin/es.tv.huan.escast.zip` |
| 2 | 前端聚合脚本 | `scripts/build-frontend.mjs`（新增） |
| 3 | 指向 staging | `tauri.conf.json` frontendDist → `../.frontend-staging` |
| 4 | 默认启动参数 | `app.windows[].url` = `index.html?es_pkg=es.tv.huan.escast` |
| 5 | desktop 注入脚本 | 自带 JSZip + 预置 IndexedDB + 首次 reload（Rust `webview.eval` 注入） |
| 6 | CSP / 构建钩子 | `security.csp` 放行 `tauri://localhost`；`.frontend-staging` 加 `.gitignore` |
| 7 | 验证 | §3 清单全绿 |

**P1b（真实投屏）**
| # | 步骤 | 改动 |
|---|------|------|
| 0 | 调研 xiaoyoucast 投屏触发点 | 定位 cast-api / 投屏按钮 handler |
| 1 | Rust SSDP 发现 + 设备描述解析 | `src-tauri/src/dlna/` |
| 2 | Rust UPnP 投屏控制（SetAVTransportURI + Play） | `dlna_*` commands |
| 3 | 前端 `DlnaModule` 适配 + 接入投屏触发点 | desktop 注入 / web-renderer |
| 4 | 桌面调试面板（最小可用验证） | 选设备 + 视频 URL + 投屏 |
| 5 | 本机局域网真机验证 | 需真实 DLNA 电视/盒子 |

---

## 7. 路线

`P1a 默认加载` → `P1b 真实投屏（最小可用验证 → 对接 xiaoyoucast）` → `P3 Windows 构建` → `P4 签名/公证/自动更新`

---

## 8. 延伸：桌面端 DLNA 真实投屏的可行性说明

- **能做到**：桌面端 Rust 实现标准 DLNA/UPnP（SSDP 发现 + AVTransport 控制），对支持 DLNA 的电视/盒子执行「推送媒体 URL → 播放」，这是通用协议，与 Android 原生 `DlnaModule` 能力对等。
- **做不到/受限**：① 无法投到非 DLNA 设备（如仅支持 AirPlay 的苹果设备，需另走 AirPlay 协议）；② 依赖同一局域网 + 目标设备开机可发现；③ xiaoyoucast 线上「投屏码」体系依赖服务端，桌面端第一阶段先走本地直投，不打通服务端。
- **结论**：第一阶段 P1b 以「本地 DLNA 直投」为交付目标，验证链路通后，再视需要对接 xiaoyoucast 投屏 UI。

---

## 9. 实现进度补充（2026-08-10：启动三问题修复）

用户本机 `npm run dev` 反馈三个问题，已全部修复（`cargo build` 零警告通过，沙箱 headless 已验证编译 + 二进制不 abort + serve 1420=200；窗口渲染/缩放/es_pkg 重定向实际效果需本机 GUI 验证）：

1. **宽高比不对（实为 Rust 侧 abort）**：原 16:9 钳制把 `inner_size()` 物理像素当逻辑 `set_size` 回去，Retina 下窗口尺寸指数爆炸 → macOS NSWindow abort。修复：用 `scale_factor()` 把物理像素换算回逻辑像素再算再 set（`src/main.rs`）。
2. **resize 不缩放**：web-runtime 内置 `scaleApp()` 用加载时固定尺寸、且 `window.innerWidth/innerHeight` 被重定义为 1920/1080。新增注入脚本 `src-tauri/ui_scale.js`，用 `document.documentElement.clientWidth/Height` 真实视口在 resize 时重算 `#main-container` scale 并居中，经 `initialization_script` 注入（前端零改动）。
3. **es_pkg 预加载**：`ui_scale.js` 的 `ensureEsPkg()` 在 URL 无显式加载参数时 `location.replace` 追加 `?es_pkg=es.tv.huan.escast`，触发 loader 既有 es_pkg 分支。**当前为在线 resolve 分支**（`getPackageInfo` 联网拉包）；计划 §3 的"路径 B 离线预置 zip 到 IndexedDB"尚未实现，离线/无包环境会空白，需后续补离线兜底。

---

## 10. es_pkg 预加载方案更正（2026-08-10 21:08）

§9 中"es_pkg 预加载用 `ui_scale.js` 的 `ensureEsPkg()` 做 `location.replace`"**已废弃**——实测 Tauri/WKWebView 在 `initialization_script`（document-start）阶段调用 `location.replace` 不可靠（页面初始化早期 reload 被忽略），导致 loader 顶层同步读取的 `urlParams` 拿到裸 URL → 走 web-runtime 默认壳页，表现为"启动不是小柚投屏"。

**现行方案（dev）**：`tauri.conf.json` 的 `devUrl` 直接设为 `http://localhost:1420/?es_pkg=es.tv.huan.escast`，初始 URL 自带参数，loader 必读。`ui_scale.js` 的 `ensureEsPkg` 保留仅作 release 兜底（但 document-start reload 同样不可靠，release 不保证生效）。

**TODO（release 可靠化）**：把 release 入口改为 meta-refresh 薄壳 HTML（`frontend/index.html` → `<meta http-equiv=refresh content=0;url=app/index.html?es_pkg=es.tv.huan.escast>`），`beforeBuildCommand` 负责把 web-runtime/dist 复制到 `frontend/app`，`frontendDist` 指向 `frontend`。这样 release 初始加载即带 es_pkg，不依赖 document-start JS reload。

**TODO（离线兜底）**：`getPackageInfo` 在线 resolve 依赖 `run.quicktvui.com` 存在 `es.tv.huan.escast` 且本机联网；离线/包缺失会抛 `Error: 无法加载应用`。彻底离线首次启动需内置 escast 的 zip 并预置 IndexedDB（loader 已有 `saveZipToDB` 缓存机制，首次联网成功即写入缓存，之后离线可用）。

---

## 11. es_pkg 在线拉包失败根因（CORS）+ 修复（2026-08-10 21:33）

§9/§10 的 devUrl 带参 + document-start reload 思路只解决"参数注入时机"，但**没解决跨域**：桌面 production web-runtime/dist 的 autoProxy 不会把 run.quicktvui.com 改写成 /proxy（PROXY_ALL_CROSS_ORIGIN 仅 dev 构建为 true；DEFAULT_PROXY_PATTERNS 不命中 run.quicktvui.com；且 .zip 下载被显式排除代理）。浏览器预检无 ACAO → 拦截 → getPackageInfo 抛错 → 页面报"无法加载应用"。

用户本地能跑通，是因为 web-cli dev 自带 DevServer /proxy 同域转发 + 全代理，并非 run.quicktvui.com 放行跨域。

**修复（已实现）**：Rust HTTP 代理命令 proxy_http（reqwest，绕过浏览器 CORS）+ 注入脚本 proxy_fetch.js（包裹 fetch/XHR 原型，仅 *.quicktvui.com 走代理，响应 base64 回传还原）。web-runtime 零改动，dev/release 通用。服务端 POST /api/app/resolve 验证可达（返回 400 拒绝非真实加密体，真实加密体经代理必得 200）。

**遗留**：离线首次兜底（内置 zip 预置 IndexedDB）未做；release 入口 meta-refresh 薄壳（§10 TODO）未做。
