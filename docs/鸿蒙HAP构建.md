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
| `endpoint_*` | ⛔ | — | 无前端消费方（`messageboard-tv` 走的是原生插件 `EndpointModule`），先不写死代码 |
| `http_download_*` | ⛔ | — | 流式下载要 reqwest+TLS，鸿蒙目标下不稳；用到再按 ArkTS 分块重做 |
| `dlna_*`（9 个） | ⛔ | — | 见下 |

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

### 7.2 为什么 DLNA 这轮不做

`dlna_*` 的核心是 SSDP：**必须收 239.255.255.250:1900 的组播 M-SEARCH**，否则连「被手机发现」
都做不到。鸿蒙对组播的约束是系统级（`ohos.permission.MULTICAST` 属 system_basic，普通应用
申请不到，`requestPermissions` 里写了也只是安装时被忽略）。定向单播（知道手机 IP 直接发
NOTIFY）能凑出半个 DMR，但手机端的「搜索设备」列表里永远看不到本机 —— 这等于把 DLNA
最核心的发现能力砍掉，不如明确标不支持，让页面走「手机小程序投屏」那条已验证的路。

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
