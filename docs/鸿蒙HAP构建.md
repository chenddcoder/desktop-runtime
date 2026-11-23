# 鸿蒙 HAP 构建（desktop-runtime 的第三个端）

> 目标：把 `desktop-runtime` 里那份 `web-runtime` 前端，以**鸿蒙原生应用（HAP）**形态发布，
> 与 macOS 桌面包共用同一套前端代码，**web-runtime 一行不改**。

## 1. 为什么不用 `tauri build` 直接出 HAP

| 路线 | 现状 | 结论 |
| --- | --- | --- |
| Tauri 官方 | 支持矩阵只有 桌面 / Android / iOS，**没有 ohos target** | 不可用 |
| 社区 fork `feat/open-harmony` | `tauri` / `wry` / `tao` 三分支，最后提交 2026-07-30，未合入主线 | 可用但停更，生产要锁 rev 自维护 |
| 自建 ArkWeb 壳 + NAPI | 官方支持的路径（ArkWeb + napi-ohos） | **本项目采用** |

fork 路线的额外代价：`tauri-plugin-dialog` / `-fs` / `-shell` 三个官方插件在鸿蒙端没有官方实现，
必须先补才能编过；而且已知坑是「前端 assets 被编进 `.so`」，改一行前端也要重编 Rust 二进制。

自建壳的收益：前端 dist 放在 `rawfile/`，改前端只重打包 HAP（秒级），Rust 能力按需下沉。

## 2. 架构

```
harmony/entry/src/main/ets/
├─ entryability/EntryAbility.ets   全屏沉浸 + 常亮（投屏/大屏场景）
├─ pages/Index.ets                 ArkWeb 页面
│    ├─ onInterceptRequest          https://appassets.local/*  →  rawfile/web/*
│    ├─ javaScriptProxy             window.__ohosBridge.call(reqId, cmd, argsJson)
│    └─ javaScriptOnDocumentStart   注入 __TAURI__ shim（早于 web-runtime 主 JS）
└─ bridge/
     ├─ InvokeRouter.ets            invoke 命令分发（未实现的返回 __unsupported__）
     ├─ NapiBridge.ets              libquickapp_bridge.so 封装
     └─ TauriShim.ets               document-start 注入的 JS 源码
```

请求流向：

```
web-runtime (ArkWeb 内)
   fetch/XHR ──► proxy_fetch.js 拦截 ──► window.__TAURI__.core.invoke(cmd,args)
                                              │
                                    __ohosBridge.call(...)
                                              ▼
                                   InvokeRouter (ArkTS)
                                        │           │
                                  ArkTS 原生     NapiBridge ──► libquickapp_bridge.so (Rust)
                                              │
                                   runJavaScript("__tauriBridgeResolve(...)")
                                              ▼
                                        Promise resolve
```

下行事件（Rust/ArkTS → 前端）走 `controller.runJavaScript("__tauriHostEmit(name, payloadJson)")`，
对应 `window.__TAURI__.event.listen(name, cb)`。

## 3. 构建

```bash
cd desktop-runtime
./scripts/build-hap.sh              # 全量：同步 dist + 编 Rust 桥 + 打 HAP
./scripts/build-hap.sh --no-rust    # 只打前端壳
./scripts/build-hap.sh --skip-web   # 前端没改，跳过 dist 同步
./scripts/build-hap.sh --release
```

产物：`harmony/entry/build/default/outputs/default/entry-default-{unsigned,signed}.hap`

脚本内部做的事（每一步都有坑，别绕过）：

1. **同步 dist → `rawfile/web`**，跳过 `.gz`（hvigor 自己压缩），并清掉上一版残留 chunk。
2. **编 Rust 桥**：`aarch64-unknown-linux-ohos` 是 **tier 3 目标，官方不提供预编译 std**，
   所以必须 nightly + `-Zbuild-std`（`ohrs` 也是这么干的）。链接器显式指 NDK 的
   `aarch64-unknown-linux-ohos-clang`，否则 rustc 会去找系统 `cc` 然后失败。
3. **镜像到英文目录**：`hvigor` 硬性限制工程路径只能含字母/数字/`-_.() @`，
   而本仓库根目录叫「扩展屏快应用」→ 直接构建会报 `00306003 Invalid project path`。
   脚本把工程镜像到 `~/.quickapp-harmony-build` 再构建，产物搬回仓库。
4. **hvigorw assembleHap**（用 DevEco 自带的 node 与 hvigor，不依赖本机 node 版本）。
5. 产物搬回 `harmony/entry/build/.../outputs/`。

## 4. API 版本（决定能装到哪些设备）

`harmony/build-profile.json5` 现在是：

- `targetSdkVersion`: `6.1.1(24)` —— 用最新 SDK 编译
- `compatibleSdkVersion`: `5.0.5(17)` —— 最低支持 HarmonyOS 5.0.5

**不要随手把 compatibleSdkVersion 提到 24**：那样只有 HarmonyOS 6.1+ 能装，5.x 主流真机和本机三台
模拟器（TV=API 19 / 2in1=API 17 / Mate X5=API 21）全部装不上。真要覆盖更老的 4.x/5.0.0，
需要先装对应 API 的 SDK，再降。

## 5. 签名（唯一需要人工的一步）

`.p7b` profile 里 **bundleName 是写死的**：`~/.ohos/config/` 里现成的那几份都属于
`com.chenddcoder.leisure` / `cast` / `mathviz` 等老包，**不能给新包名复用**。
自动签名的 p12 密码也是 DevEco 加密存进 `build-profile.json5` 的，没法手工搬。

所以首次需要：

1. DevEco 打开 `desktop-runtime/harmony`
2. File → Project Structure → Signing Configs → 勾选 **Automatically generate signature**
3. 登录华为开发者账号，等它生成完（会写回 `harmony/build-profile.json5` 的 `signingConfigs`）
4. 重跑 `./scripts/build-hap.sh` → 出 `entry-default-signed.hap`

之后每次构建都会自动签名（脚本原样同步 `build-profile.json5`，不会把签名配置冲掉）。

## 6. 真机验证

```bash
HDC="$DEVECO/Contents/sdk/default/openharmony/toolchains/hdc"
$HDC list targets
$HDC install -r harmony/entry/build/default/outputs/default/entry-default-signed.hap
$HDC shell aa start -a EntryAbility -b com.chenddcoder.touping

# 页面侧日志（Index.ets 的 console / 资源错误都走这个 tag）
$HDC shell hilog -T ToupingPage
# invoke 分发日志
$HDC shell hilog -T ToupingRouter
# Rust 桥加载情况
$HDC shell hilog -T ToupingNapi
```

排查优先级：`ToupingPage` 里 `[web]` 开头的控制台输出是页面真实报错来源
（ArkWeb 的 `onConsole` 会把 web 侧 console 全量转到 hilog，比抓不到 console 的桌面端更好查）。

## 7. 桌面 22 个命令在鸿蒙侧的落地状态

| 命令 | 状态 | 实现位置 | 说明 |
| --- | --- | --- | --- |
| `bridge_ping` / `bridge_version` | ✅ | ArkTS `InvokeRouter` | 壳存活探测 |
| `bridge_rust_ping` / `bridge_rust_info` | ✅ | Rust NAPI | 验证 `.so` 加载与 NAPI 往返 |
| `app_exit` | ✅ | ArkTS | `UIAbilityContext.terminateSelf()` |
| `proxy_http` | ✅ | ArkTS `ProxyHttp.ets`（`@ohos.net.http`） | 含与 `proxy.rs` 一致的 SSRF 私有网段拦截；手写 TLS 客户端不划算 |
| `webdav_start/stop/info` | ✅ | Rust `webdav.rs`（移植） | 落盘根由 ArkTS 用 `context.filesDir` 传入 |
| `es_read_file_base64` / `es_list_files` | ✅ | Rust `webdav.rs`（移植） | 越界读仍被 `canonicalize` 拦住 |
| `pad_start/stats/target/stop/reset` | ✅ | ArkTS `PadUdp.ets`（UDP 5991） | 60Hz 按键不能走轮询，改 ArkTS 直收直推 |
| `endpoint_*` | ✅ | ArkTS `EndpointServer.ets`（`TCPSocketServer`） | 端口 39200 起顺延；请求原样发 `endpoint://request`，5s 不回 504；path 保留 query |
| `http_download_*` | ✅ | ArkTS `HttpDownload.ets`（`@ohos.net.http`） | open 一次性取回整包，read 按 1MB 切片回 base64；es_pkg 下载快应用包走这条 |
| `dlna_*`（8 个） | ✅ | Rust `harmony-bridge/src/dlna/*`（从 `src-tauri/src/dlna` 整体移植） | 启动即拉起 5001；SSDP/HTTP/SOAP/播放列表通道全在 Rust；事件走 `host` 队列 |

### 7.1 事件怎么从 Rust 回到页面

桌面端 Rust 直接 `app.emit()`（Tauri 自带 IPC）。鸿蒙侧没有这条路，硬做就要 NAPI 的
`ThreadsafeFunction`（每包一次跨线程调度 + 生命周期管理）。改成分层：

```
Rust 线程 ──push──> host::QUEUE(VecDeque)  ──bridge_poll(~80ms)──> ArkTS
         ──runJavaScript("__tauriHostEmit(name,payloadJson)")──> 页面
```

页面侧看到的仍是 `desktopListen('webdav://upload', cb)`，与桌面**完全一致**，业务零改动。
代价是最大 80ms 延迟 —— 一次性通知（文件落盘）无所谓；**手柄按键因此不走这条路**，
在 `PadUdp.ets` 里 ArkTS 直收直推。

### 7.2 DLNA 与 endpoint 的落地方式（2026-11 补齐）

早期结论「鸿蒙普通应用拿不到组播」只对旧接口成立。当前 Network Kit 的
`MulticastSocket`（API 11+）与 Socket 服务一样**只声明 `ohos.permission.INTERNET`**，
`addMembership` 不再要 system_basic 权限 —— 所以 SSDP 是可行的。

做法不是用 ArkTS 重写一遍协议栈，而是把桌面端 `src-tauri/src/dlna` **整体搬进
`harmony-bridge`**（纯 Rust + tokio + socket2，不依赖 tauri 运行时）：

- `dlna/compat.rs` 提供 `AppHandle(=host::HostHandle)` / `Emitter` / `Manager.path()` 三个替身，
  `emit` 直接进 `host` 事件队列，由 `bridge_poll` 转给页面（与 webdav 同一条路）
- 日志/设备名/播放列表配置的目录由 `dlna_set_root(context.filesDir)` 指定
- `Index.ets` 启动 1.2s 后自动 `dlna_start(5001)`，与桌面端 setup 阶段一致

**本机 IP 的来源（踩过坑）**：桌面端 `list_local_ipv4()` 是解析 `ifconfig` 输出，那是
macOS/BSD 的写法；鸿蒙沙箱里既没这个可执行文件也没 PATH，结果直接是空列表 ——
于是设备名退化成「投屏显象_00.00」，SSDP 一个接口都 join 不上，手机端搜不到本机。
现在两层兜底：

1. Rust 侧改用 `libc::getifaddrs` 直接问内核（跳过 `lo/p2p/dummy/tun/rmnet/...`、
   过滤 `127./169.254.`），`wlan*/eth*` 排前；
2. `NetInfo.ets::resolveLanIp()` 用 `connection.getDefaultNet()` → `getConnectionProperties()`
   取系统权威地址（默认网络取不到则遍历 `getAllNets()`），启动时经
   `dlna_set_local_ip(ip)` 存为 hint，`dlna_start` 仅在枚举为空时采用。

`get_dlna_name()`（页面标题来源）同样先走 hint，且 IP 为空时**不缓存**兜底名 ——
否则页面在 `dlna_start` 之前调一次就会把 `00.00` 焊死。

**投屏不播（最容易漏的一环）**：Rust DMR 只负责回 SOAP 200 + `emit("dlna://play")`，
真正换源播放的是页面里的快应用，它只认原生契约 `ACTION_DLNA` / `ACTION_DLNA_STATUS`。
桌面端靠 `src-tauri/dlna_overlay.js`（`main.rs` 的 `initialization_script`）做这层翻译；
鸿蒙侧一开始整段没移植，于是「能发现、能投、但不播」——事件到了页面没有监听者。

现在 `bridge/DlnaOverlay.ets` 是同一份翻译，随 `javaScriptOnDocumentStart` 在
`TAURI_SHIM_JS` 之后注入：`dlna://play → ACTION_DLNA{actionType:'playerUrl',url,title,mediaType}`、
`seek/stop/pause → ACTION_DLNA_STATUS`，EventDispatcher 未就绪时缓存 + 500ms 轮询补发，
`dlna://app-ready` 加速补发。

同时 `Web` 组件补了两个属性，缺一个都会「投屏成功但黑屏」：

| 属性 | 为什么 |
| --- | --- |
| `.mixedMode(MixedMode.All)` | 页面 origin 是 `https://appassets.local`，手机推的是 `http://192.168.x.x:port/...`，默认策略直接拦掉这段混合内容 |
| `.mediaPlayGestureAccess(false)` | 投屏是程序化 `play()`，电视前没有点击动作；默认策略会把 `<video>` 挂起，手机端看到的一直是 STOPPED |

排查顺序（hilog / `dlna.log`）：
1. `[web] [dlna_overlay] ready | EventDispatcher 已就绪，投屏桥可用` —— 没有这行说明快应用没跑起来；
2. `[web] [dlna_overlay] cast-play | 收到投屏请求` —— 没有说明事件没到页面（看 `[dlna_emit] dlna://play`）；
3. `[web] [dlna_overlay] bridge | broadcast ACTION_DLNA` —— 有这行但仍黑屏，问题在业务层或媒体地址本身。

`endpoint_*` 则用 ArkTS 的 `TCPSocketServer` 实现（`EndpointServer.ets`）：手机扫码直连、
请求原样交给页面算、5s 超时回 504 —— 语义与 Android 插件/桌面 `endpoint.rs` 对齐。

真机排查：`dlna.log` 里 `[dlna_ssdp]` 开头是自检（本机多发多收是否通），
收不到 M-SEARCH 时先看这行，再区分是组播被系统拦还是协议不匹配。

### 7.3 宿主机自测（没有真机也能验 WebDAV）

`harmony-bridge/src/webdav.rs` 是纯 Rust、不碰鸿蒙 API，所以能在 Mac 上直接跑：

```bash
# 临时 crate：把 host.rs / webdav.rs 以 #[path] 引进来，起服务后 curl 灌一个文件
cd /tmp/wdtest && WD_ROOT=/tmp/wdtest/files ./target/release/wdtest &
curl --noproxy '*' -X PUT --data-binary @demo.nes "http://127.0.0.1:5992/es_files/files/nes/demo.nes"
# → 201；事件队列里出现 {"name":"webdav://upload","payload":{path,name,size}}
```

本机实测结果：`PUT 201` → 落盘 → `webdav://upload {name:"demo.nes",size:14}` →
`es_list_files` 返回绝对路径 → `es_read_file_base64` 回 `TkVTGmhlbGxvLWNhcnQ=`（14 字节无损），
读根外文件（`/etc/hosts`）被拒。**这条验证了移植没有语义漂移，不必等真机。**

## 8. 已知坑（都踩过）

- **工程路径不许有中文**：`00306003 Invalid project path`。见上面第 3 步。
- **`onInterceptRequest` 放行必须 `return null`**，但 ArkWeb 的类型声明是非空的
  （`Callback<OnInterceptRequestEvent, WebResourceResponse>`），`null as WebResourceResponse`
  会被 ArkTS 判「类型不重叠」。当前用 `ESObject` 中转（`Index.ets` 的 `intercept`）。
- **`__CROSS_APP_RESOLVER_BASE__` 会被 index.html 无条件覆盖成 `location.origin`**，
  在 document-start 改全局变量没用 → 必须用 `?resolver=` query 覆盖。
- **`aarch64-unknown-linux-ohos` 没有预编译 std**，`rustup target add` 会 404 / 卡住。
  必须 nightly + `-Zbuild-std`。
- **`.so` 的导出名必须与 `index.d.ts` 逐字一致**，否则编译期过、运行期 `is not a function`。
  更坑的是 napi-ohos 1.2 **默认把 Rust 的 snake_case 转成 camelCase**（`bridge_info` → `bridgeInfo`），
  页面拿到的是 `{"__error__":"rust bridge not loaded: undefined is not callable"}`。
  必须逐个写 `#[napi(js_name = "...")]` 固定导出名，并给构建加 `NAPI_BUILD_TARGET_NAME=quickapp_bridge`
  （否则模块名是 crate 名 `harmony_bridge`，requireNapi 直接找不到）。
- **本机模拟器起不来**：`Emulator -start Huawei_TV` 会卡在镜像校验之后（进程占 300%+ CPU、
  日志不再前进），这套沙箱环境里 GUI 起不来。验证只能走真机。
- **模板字符串里不许出现反引号**：`TauriShim.ets` 里整个注入脚本是一个模板字面量，
  注释里写 `` `desktopListen()` `` 会**提前闭合模板**，报出来的是「Cannot find name 'desktopListen'」
  —— 看着像漏了 import，其实是字符串被切断了。要写代码名就用普通引号或直接裸写。
- **`SocketRemoteInfo.family` 是字符串（`'IPv4'`），`NetAddress.family` 是数字（1/2）**：
  收包后想回 ack 必须显式转一次，直接 `const peer: NetAddress = info.remoteInfo` 编译期就报错。
- **ArkTS 不接受裸对象字面量**：`header: x ? x : {}` 会报 `arkts-no-untyped-obj-literals`，
  必须先声明 `const h: Record<string, string> = {}` 再赋值。
- **`http.RequestMethod` 里没有 `PATCH`**：写了就是编译错误；`ProxyHttp.ets` 的 `methodOf` 里
  已注释说明「PATCH 按 GET 发」，别以为支持了。
- **bash 里 `$VAR` 后面紧跟中文/全角字符**会被并进变量名（`BUILD_DIR（` → unbound variable），
  统一写 `${VAR}`。

## 9. 目录

```
desktop-runtime/
├─ harmony/                        DevEco 工程（用 DevEco 打开它来做签名）
│   ├─ AppScope/app.json5          bundleName = com.chenddcoder.touping
│   ├─ build-profile.json5         signingConfigs 由 DevEco 自动签名写入
│   └─ entry/
│       ├─ libs/arm64-v8a/         libquickapp_bridge.so（脚本产出，勿手改）
│       └─ src/main/
│           ├─ ets/                EntryAbility / Index / bridge
│           └─ resources/rawfile/web/   web-runtime dist（脚本同步）
├─ harmony-bridge/                 Rust NAPI crate（napi-ohos）
│   └─ src/
│       ├─ lib.rs                  #[napi] 导出（命令名与 ArkTS 侧逐字一致）
│       ├─ host.rs                 tokio runtime / 事件队列 / lan_ip
│       └─ webdav.rs               由 src-tauri/src/webdav.rs 移植（改三处，见文件头）
└─ scripts/build-hap.sh            一键构建
```

ArkTS 侧（`harmony/entry/src/main/ets/`）：

| 文件 | 职责 |
| --- | --- |
| `bridge/TauriShim.ets` | document-start 注入 `__TAURI__` / `__TAURI_INTERNALS__` / `isTauri` |
| `bridge/InvokeRouter.ets` | 22 个命令的路由与 `__unsupported__` 兜底 |
| `bridge/NapiBridge.ets` | `.so` 的类型声明与错误包装 |
| `bridge/ProxyHttp.ets` | `proxy_http`（`@ohos.net.http` + SSRF 拦截） |
| `bridge/PadUdp.ets` | 手柄 UDP 5991（收包 → `pad://input`） |
| `pages/Index.ets` | ArkWeb 容器、rawfile 拦截、事件泵、退出 |
