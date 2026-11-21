# 桌面端跑 NES 快应用（真 wasm 内核 + UDP 手柄）

> 2026-11-15 · 对应 `EsApp/esapp-nes/src/nes/wasm/web-*.ts`、`src/cast/webdav.ts`、`src/cast/pad-udp.ts`、
> `desktop-runtime/src-tauri/src/pad.rs`

## 一句话

桌面壳（Tauri）里没有 `com.quicktvui.plugin.wasm`，但这台宿主本来就是完整浏览器 ——
所以 **`WebAssembly.instantiate` 直接跑 `kernels.assets.ts` 里那份 asm 内核**，
和电视端同一个内核、同一份卡带、同样的调色板；jsnes 只留作兜底。

## 三条后端对照

| | Android（真机 / 电视） | 桌面壳 / 网页 | jsnes（兜底） |
|---|---|---|---|
| 类 | `WasmBackend` | `WebWasmBackend` | `Emulator` |
| 谁推进帧 | 插件 native 线程 | **JS 的 `step()`** | JS 的 `step()` |
| 画面 | `<wasm-view>` 自绘 | `<canvas-view>` + `putImageData` | 同左（走调色板 + 脏矩形） |
| 音频 | 原生 `AudioTrack` | `AudioOut` 的 WebAudio 分支 | 同左 |
| 暂停 | 插件 `:paused` | 帧泵不调 `step()` + 清音频队列 | 帧泵不调 `step()` |
| 探针 `eng=` | `wamr-aot` / `wamr-int` | **`web-wasm`** | `jsnes` |

选择顺序（`Play.vue::pickBackend`）：`IS_WEB` → 有 `WebAssembly` 走 `WebWasmBackend`，否则 jsnes；
非 web 宿主 → 先 `waitModule('getRuntimeInfo')`，通了走 `WasmBackend`，失败退 jsnes。

## 内核 ABI（与 `插件/wasm/nesasm-core/tools/romrun.mjs` 逐字一致）

```text
init()
写 prg_rom_addr() / chr_rom_addr() 后备区      ← ⛔ 必须 init 之后写（init 会覆盖 CHR）
mapper_info(mapper, prgBanks, chrBanks, mirror) ← 返回 0 才算接住这块卡带
set_pc(窗口 prg_addr()+0x7ffc 的复位向量)       ← ⛔ 读后备区会错位 18KB
set_buttons(mask) / run_frame(n) / fb()
```

帧缓冲是 **`0xAABBGGRR`**（Android ARGB_8888 内存序），按 Uint32 塞进 ImageData 颜色就是对的
—— 和 jsnes 的 `0xBBGGRR` 在这个视角下同构，所以 `VideoOut.presentDirect()` 一份代码吃两种输入。

音频环：`[head:u32][tail:u32][data:i16 × cap]`，48kHz 单声道，一帧约 798 样本。
消费方式就是原生线程干的那件事：读 head/tail → 搬数据 → tail 推上去。

## 桌面端补的三块

### 1. `web-kernel.ts` / `web-backend.ts`（新）

浏览器侧内核驱动 + 与 `WasmBackend` 同形的后端。判据读数：

- `stats().fps` —— 1 秒滑动窗口（**不是**累计平均，暂停一次就永久拉低）
- `stats().inputMove` —— 按键后第几帧画面变（帧指纹 FNV-1a，隔 97 像素采样）
- `stats().samples` —— 真正喂进 `AudioOut` 的样本数（>0 才说明声音出去了）

### 2. `webdav.ts` 的桌面分支（原来缺）

之前只认 Android 插件，桌面壳里 `initCastServer()` 会去装一个永远装不上的插件，
最终停在 `phase=error`、地址 `-`、手柄未连接 —— **投屏收卡带整条链是死的**。

现在 `IS_DESKTOP` 直接走 `invoke('webdav_start')`，把 Rust 返回的 `{url,ip,port,root}`
落到 `castServer`；Rust 的 `webdav://upload` 事件再**还原成 Android 插件的
`OnWebdavEvent` 形态**（`{action:'upload_success', data:{path,fileName,href,size}}`），
页面所有逻辑一行没改。

> 为什么不在 Rust 里直接发插件同款 JSON：`webdav.rs` 是桌面壳**所有**快应用共用的
> （投屏显象也在用），端差异留在端里更便宜。

### 3. `pad.rs`（新）：UDP 5991

手机小程序写死 5991，桌面壳没有 `PadModule` 就只能走 5990 落盘那条慢通道（100ms+）。

协议与小程序 `utils/cast/channel.js` 逐字一致：

```text
手机 → 电视  "seq,mask,t"    ASCII
电视 → 手机  "ack,seq"       ping 回 "ack,-1"
```

命令：`pad_start` / `pad_stats` / `pad_target` / `pad_stop` / `pad_reset`；事件 `pad://input`。

## ⛔ 桌面端与 Android 端在 JS 层是**相反**的

电视上 UDP 通道是「内核直收」（插件在帧线程里写 `set_buttons`），所以 JS 必须彻底收手，
否则两路抢同一份按键表 → 按着不动会自己松开。

桌面端内核跑在 JS 里，Rust 只能把包**交给 JS**：`pad://input` → `padState.buttons` → 帧泵下发。

```ts
// Play.vue::syncPad
const udpDirect = padUdp.on && !(emu instanceof WebWasmBackend)
```

漏了这一句的症状：**HUD 显示手柄在线、游戏一动不动，且没有任何报错**。

## 验收

```bash
# 桌面端对等环境（headless Chrome + Node 桥复刻 Rust 的 webdav/pad）
cd EsApp/esapp-nes
node ../_tool/verify-web.mjs . --serve \
  --scenario verify/scenario-desktop.json \
  --bridge ../../desktop-runtime/scripts/verify-nes-bridge.mjs \
  --init-js ../../desktop-runtime/scripts/desktop-tauri-mock.js
# → 26 项全绿（含 eng=web-wasm、UDP START 进模拟器、松手抬起）

# Rust 侧协议层
cd desktop-runtime/src-tauri && cargo test --bin quickapp-desktop pad::
# → 4 passed（含真起 UDP socket 的 roundtrip）

# 常规回归
node ../_tool/verify-web.mjs . --serve --scenario verify/scenario.json      # 33
node ../_tool/verify-web.mjs . --serve --scenario verify/scenario-grid.json # 17
```

## 踩过的坑

1. **`--init-js` 不能省**。不带 `desktop-tauri-mock.js` 时页面里没有 `__TAURI__`，
   `IS_DESKTOP` 为假 → 走网页分支，WebDAV 顶着一个假地址（`192.168.40.93:5990`，实际没人听），
   验收看似「能跑」但投屏是假的。
2. **UDP 包体顺序是 `seq,mask`**，写反了电视会把 mask 当序号回执（`ack,1`），
   按键位图全错位 —— 表现为「按 START 变成按 A」，很容易误判成内核问题。
3. **`ctxAttached` 必须等真的拿到 2D 上下文才置位**（`VideoOut.webAttached`）。
   canvas 节点晚到一次就永久黑屏，而帧泵的「每 200ms 补挂」重试会被那个标志挡住。
4. **桌面 `padUdp.recv` 是 Rust 侧计数**，判活只能用 `lastRecvAgoMs`：
   手机松手后仍以 20Hz 发中立包（recv 一直涨），拿「有没有增量」判活会误掉线。
5. **端口从 5990 / 5991 起可能顺延**（Rust 各扫 40 / 10 个），
   地址一律信返回值，别硬编码 —— 硬编码二维码会指向没人监听的端口。
6. **⛔ 被遮挡的 WKWebView 会把 `setTimeout` 帧泵降频**。验收时窗口若压在别的窗口后面，
   一次 150ms 的单包短按大概率「落在两帧之间」，看上去像手柄没通；真手机是 20Hz 连发，
   所以不是缺陷，但**验收必须持续按住**（`node /tmp/pad-hold.mjs 32 2000` 那种 20Hz×2s）。
7. **桌面壳的声音要用户手势解锁**：WebKit 的 `AudioContext` 在无手势时恒 suspended，
   `unlockAudio` 挂在 `keydown` / `pointerdown` 上，所以**右栏 ♪ 一开始是灰的**，
   `soundOn` 判据 `emu.audio.contextState === 'running'` 恒假。点一下窗口（或按任意键）就出声。

## 真机实测记录（2026-11-15，v1.0.14）

真 Tauri 窗口（`cargo run --bin quickapp-desktop`，pid 93543，窗口标题「卡带机」1680×945）：

| 环节 | 观测 | 结论 |
| --- | --- | --- |
| 发版 | `es.com.nes.tv@1.0.14`，373.6 KB / 30 个 assets，md5 校验通过 | 包完整 |
| WebDAV | `curl 127.0.0.1:5990/__info` → `running:true, port:5990` | Rust 真在听 |
| 投卡带 | `PUT /es_files/files/smoke-desktop.nes` → 201，**5 秒后 5991 起来** | `onWebdav → openRom → Play → pad_start` 全链通 |
| UDP 协议 | `ping→ack,-1`、`1,8→ack,1`、`2,0→ack,2` | 序号/回执对 |
| 画面 | 窗口截图里 2048 标题画面 → 按 START 后出现两块 `2` | 内核在跑、canvas 在上屏 |
| 按键生效 | 20Hz×2s 按住 DOWN → 三块 `2` 从第 2 行滑到第 4 行（新块生成） | **手机 → Rust → JS → wasm `set_buttons` → 画布 全链通** |
| 手柄指示灯 | 右栏圆点由灰转**绿** | `padState.online` 被 `applyDesktopMask` 置真 |
| 声音 | ♪ 灰（未解锁） | 符合预期，需用户手势 |

> 截图归档：`EsApp/esapp-nes/design/exports/runtime/desktop-real-window.png`
> ⛔ 整屏 `screencapture` 在这台机上返回**全黑**（无屏幕录制权限，3360×2100 单色 7056000 px），
> 必须用窗口 ID 抓：`screencapture -x -o -l <windowid>`（ID 用 `CGWindowListCopyWindowInfo` 拿，
> 见 `/tmp/winlist.swift`）。别把「全黑」当成应用黑屏。
