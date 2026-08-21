# DLNA 设备名（dlnaName）打通与可修改设计

## 背景与问题

投屏链路分三层，但 DLNA 设备名在两处各自定义、互不一致，且均无法在运行时修改：

1. **前端 vue（esapp-tvcast）**：`Native.callNativeWithPromise('ProcessBridgeModule', 'getDlnaName')` 已能到达 web-runtime 层。
2. **web-runtime（web-renderer）**：`ProcessBridgeModule.js` 的 `getDlnaName/setDlnaName` 只读写 `localStorage`（默认名 `QuickTV-Web`），未真正连通桌面 DLNA 服务。
3. **desktop-runtime（Rust/Tauri）**：真正广播给手机/电视的 DLNA `friendlyName` 在 `dlna/mod.rs:171` 硬编码为 `快应用投屏 ({local_ip})`，且 `DeviceDesc.friendly_name` 是构造时固定的 `String`，运行时无法修改。

目标：把「广播给其他设备的 DLNA 名」与「前端显示的设备名」统一为同一个可修改、可持久化的值，默认名采用 `投屏显象_xx.xx`（取本机 IPv4 后两段）。

## 调用链路

```
esapp-tvcast (vue)
  └─ Native.callNativeWithPromise('ProcessBridgeModule', 'getDlnaName')
       └─ web-renderer ProcessBridgeModule.getDlnaName()
            └─ 判断是否 Rust 环境：window.__get_invoke() 可用？
                 ├─ 是 → invoke('ProcessBridgeModule.getDlnaName')
                 │         └─ desktop-runtime (Tauri Rust) 读写真实 friendlyName
                 └─ 否 → localStorage 兜底（浏览器/纯 web 环境）
```

## 变更清单

### 1. desktop-runtime（Rust 侧）——核心

**a. 新增「同名 Module」命令**（在 `src-tauri/src/dlna/mod.rs` 中）：

- `#[tauri::command(rename = "ProcessBridgeModule.getDlnaName")]`
  `fn get_dlna_name(app: AppHandle) -> String`：返回当前生效的 friendlyName。
- `#[tauri::command(rename = "ProcessBridgeModule.setDlnaName")]`
  `fn set_dlna_name(app: AppHandle, name: String) -> Result<(), String>`
  ：写入持久化文件并更新内存中的共享 friendlyName（若 DLNA 已运行则热更新广播名）。

> 依赖 Tauri >= 2.11 的 `rename` 属性（当前 Cargo.lock 为 2.11.5，满足）。这样前端 `invoke` 可直接写 `module.func`，与 web-runtime 端命名、以及 vue 端 `callNativeWithPromise` 语义完全一致。

**b. 持久化**（放在 desktop，重启保留）：
- 配置文件：跟随可执行文件/项目根放置 `dlna-name.json`（参考现有 `devtools.json` 的定位兜底逻辑）。
- 内容：`{ "name": "投屏显象_01.23" }`。读写失败时静默降级（不阻断 DLNA）。

**c. 共享可变 friendlyName**：
- 在 `Inner` 状态中增加 `friendly_name: Mutex<String>`。
- 默认名：`format!("投屏显象_{}", last_two_segments(local_ip))`，替换当前 `let friendly = format!("快应用投屏 ({local_ip})")`。
- `DeviceDesc` 的 `friendly_name` 从 `String` 改为读取共享状态：启动时从持久化文件读；已设置则用（`dlna_set_name` 更新共享值 + 落盘）。

**d. 注册命令**：把两个新命令加入 `invoke_handler` 的 `generate_handler![]`。

### 2. web-runtime（web-renderer 层）

修改 `ProcessBridgeModule.js`：
- `getDlnaName`：若 `window.__get_invoke()` 可用 → `invoke('ProcessBridgeModule.getDlnaName')`；失败/非 Rust 环境回落到 `localStorage`。
- `setDlnaName`：优先 `invoke('ProcessBridgeModule.setDlnaName', { name })`，同时写 `localStorage` 兜底。
- 默认名常量 `DEFAULT_DEVICE_NAME = 'QuickTV-Web'` 保留仅作最后兜底。

### 3. esapp-tvcast（前端 vue 层）

- **launcher/index.vue**：`deviceName` 由硬编码 `投屏显象·我的电脑` 改为 `onMounted` 时经 `Native.callNativeWithPromise('ProcessBridgeModule', 'getDlnaName')` 读取并显示。
- **settings/index.vue**：`设备名称` 行显示读取值，点击弹出输入框改名，调用 `setDlnaName`；成功后更新列表值与返回首页顶部昵称。

## 边界与错误处理

- Rust 环境检测：以 `window.__get_invoke`（desktop-runtime 的 proxy_fetch.js 已暴露）是否可用为准。
- 任意 IPC/文件读写失败：不抛给用户崩溃，返回兜底值/静默降级，保证原有 localStorage 路径仍可用。
- 改名对已发现设备的生效：依赖其他 DLNA 控制器重新发现（重新 M-SEARCH/刷新）才更新；不做主动通知。

## 测试

- desktop-runtime：`cargo test`（现有 dlna 测试仍通过）；手动 `curl http://*:{port}/device-desc.xml` 验证 friendlyName 变化。
- web-runtime：浏览器（非 Rust）环境验证 localStorage 兜底路径。
- 端到端：desktop 启动 → 前端 launcher 显示默认名（`投屏显象_xx.xx`）→ 设置页改名 → `device-desc.xml` 名同步更新。