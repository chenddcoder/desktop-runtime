//! 投屏显象鸿蒙壳的 Rust 桥。
//!
//! 定位：桌面端 `src-tauri` 里那些**纯 Rust、不依赖 tauri 运行时**的能力，在鸿蒙侧
//! 通过 NAPI 暴露给 ArkTS，避免用 ArkTS 重写一遍 socket 逻辑。
//!
//! 边界（重要）：
//!   - 依赖 `tauri::AppHandle` 做事件 push 的部分，改走 `host::HostHandle` 的事件队列，
//!     由 ArkTS 定时 `bridge_poll` 取出后 `runJavaScript("__tauriHostEmit(...)")` 转给页面；
//!   - 需要系统证书链/代理的（proxy_http）走 ArkTS 的 `@ohos.net.http`，不必下沉到 Rust；
//!   - 手柄 UDP 由 ArkTS 直收（60Hz，不能经轮询）。
//!
//! 调用约定与 Tauri 的 invoke 对齐：入参、出参都是 JSON 字符串，出错返回
//! `{"__error__":"..."}`，由 ArkTS 侧翻译成 invoke 的 reject。

use napi_derive_ohos::napi;

mod host;
mod webdav;

/// 把 `Result<T, String>` 统一压成 JSON 字符串；错误走 `__error__`。
fn to_json<T: serde::Serialize>(r: Result<T, String>) -> String {
    match r {
        Ok(v) => serde_json::to_string(&v).unwrap_or_else(|e| format!("{{\"__error__\":\"serialize: {e}\"}}")),
        Err(e) => serde_json::json!({ "__error__": e }).to_string(),
    }
}

/// 桥存活探测：ArkTS 侧用它确认 .so 真的被加载、NAPI 真的能往返。
#[napi]
pub fn ping(args_json: String) -> String {
    let echo: serde_json::Value = serde_json::from_str(&args_json).unwrap_or(serde_json::Value::Null);
    serde_json::json!({
        "pong": true,
        "shell": "rust-napi",
        "target": "aarch64-unknown-linux-ohos",
        "echo": echo,
    })
    .to_string()
}

/// 版本与构建信息，便于真机上排查「装的是不是最新包」。
#[napi]
pub fn bridge_info() -> String {
    serde_json::json!({
        "name": "quickapp_bridge",
        "version": env!("CARGO_PKG_VERSION"),
        "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
        "modules": ["webdav"],
    })
    .to_string()
}

/// base64 往返冒烟：ArkTS 与 Rust 之间二进制传输的实际路径
/// （WebDAV 收卡带、rawfile 读取都靠它），先验证不丢字节。
#[napi]
pub fn base64_roundtrip(input: String) -> String {
    use base64::Engine;
    let engine = base64::engine::general_purpose::STANDARD;
    match engine.decode(input.as_bytes()) {
        Ok(bytes) => engine.encode(&bytes),
        Err(e) => format!("{{\"__error__\":\"base64 decode failed: {e}\"}}"),
    }
}

/* ---------------- 事件泵 ---------------- */

/// 取走待发给页面的事件（ArkTS 每 80ms 调一次）。返回 `[{name, payload}, ...]`。
#[napi]
pub fn bridge_poll() -> String {
    serde_json::to_string(&host::drain()).unwrap_or_else(|_| "[]".to_string())
}

/* ---------------- WebDAV（投屏收卡带） ---------------- */

/// 设定落盘根。`path` 是鸿蒙沙箱目录，ArkTS 侧用 `context.filesDir` 拼。
/// 必须在 `webdav_start` 之前调用；`es_read_file_base64` / `es_list_files` 也读它。
#[napi]
pub fn webdav_set_root(path: String) -> String {
    let p = path.trim().to_string();
    if p.is_empty() {
        return serde_json::json!({ "__error__": "root path is empty" }).to_string();
    }
    webdav::set_root(&p);
    serde_json::json!({ "ok": true, "root": webdav::current_root().to_string_lossy() }).to_string()
}

/// 起服务。入参 `{"port":5990}`（可省，默认 5990 起顺延 40 个）。
/// 返回 `{url, ip, port, root, running}`。
#[napi]
pub fn webdav_start(args_json: String) -> String {
    let args: serde_json::Value = serde_json::from_str(&args_json).unwrap_or(serde_json::Value::Null);
    let port = args
        .get("port")
        .and_then(|v| v.as_u64())
        .filter(|p| *p > 0 && *p <= 65535)
        .map(|p| p as u16);
    to_json(host::runtime().block_on(webdav::webdav_start(port)))
}

#[napi]
pub fn webdav_stop() -> String {
    to_json(host::runtime().block_on(webdav::webdav_stop()))
}

#[napi]
pub fn webdav_info() -> String {
    to_json(host::runtime().block_on(webdav::webdav_info()))
}

/// 读落盘目录下的文件为 base64（快应用 `FileReaderModule.readBase64` 的鸿蒙实现）。
#[napi]
pub fn es_read_file_base64(path: String) -> String {
    to_json(host::runtime().block_on(webdav::es_read_file_base64(path)))
}

/// 列出落盘目录下的文件（绝对路径数组）。
#[napi]
pub fn es_list_files() -> String {
    to_json(host::runtime().block_on(webdav::es_list_files()))
}
