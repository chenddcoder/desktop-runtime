//! 宿主能力：tokio runtime、事件队列、局域网 IP。
//!
//! 为什么需要「事件队列」而不是回调：
//!   桌面端 Rust 可以直接 `app.emit()` 把事件推给前端（Tauri 自带 IPC）。鸿蒙侧要让
//!   Rust 线程回调 JS，得用 NAPI 的 ThreadsafeFunction —— 那条路上每次调用都要过
//!   一次线程调度，而且 ArkTS 侧还得管生命周期。这里改成**队列 + 轮询**：
//!   Rust 把事件塞进 VecDeque，ArkTS 用 `bridge_poll` 每 80ms 取一次，
//!   再经 `controller.runJavaScript("__tauriHostEmit(...)")` 送到页面。
//!
//!   延迟代价：最坏 80ms。可用于「文件落盘通知」这类一次性事件；
//!   ⛔ 手柄 UDP 不走这条路（60Hz 按键经不起轮询抖动），它在 ArkTS 侧直接收。

use std::collections::VecDeque;
use std::net::UdpSocket;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde::Serialize;

/// 事件队列上限。落盘通知是低频事件，512 足够；超了丢最旧的，绝不无限涨。
const QUEUE_CAP: usize = 512;

#[derive(Clone, Debug, Serialize)]
pub struct HostEvent {
    pub name: String,
    pub payload: serde_json::Value,
}

fn queue() -> &'static Mutex<VecDeque<HostEvent>> {
    static Q: OnceLock<Mutex<VecDeque<HostEvent>>> = OnceLock::new();
    Q.get_or_init(|| Mutex::new(VecDeque::new()))
}

/// 往队列里塞一条事件（Rust 任意线程可调）。
pub fn emit_value(name: &str, payload: serde_json::Value) {
    let mut q = queue().lock().unwrap_or_else(|e| e.into_inner());
    if q.len() >= QUEUE_CAP {
        q.pop_front();
    }
    q.push_back(HostEvent {
        name: name.to_string(),
        payload,
    });
}

/// 取走全部待发事件（ArkTS 每轮 `bridge_poll` 调一次）。空队列返回 `[]`。
pub fn drain() -> Vec<HostEvent> {
    let mut q = queue().lock().unwrap_or_else(|e| e.into_inner());
    q.drain(..).collect()
}

/// 宿主句柄 —— 用来**替代**桌面端的 `tauri::AppHandle`。
///
/// 只提供 `emit`：移植过来的模块（webdav / endpoint）除了发事件，没有用到 AppHandle
/// 的其它能力（没有窗口操作、没有路径解析）。多一个方法都是负担。
#[derive(Clone, Copy)]
pub struct HostHandle;

impl HostHandle {
    /// 与 `tauri::Emitter::emit` 同签名，移植时只需换类型名。
    pub fn emit<S: Serialize>(&self, event: &str, payload: S) -> Result<(), ()> {
        match serde_json::to_value(payload) {
            Ok(v) => {
                emit_value(event, v);
                Ok(())
            }
            Err(_) => Err(()),
        }
    }
}

/* ---------------- tokio runtime ---------------- */

/// 全局多线程 runtime。第一次用时创建，进程生命周期内复用。
///
/// ⛔ 不要在 NAPI 调用里临时 `Runtime::new()` —— 每次建 runtime 都要起线程池，
///    ArkTS 侧一次 `webdav_start` 就会多泄漏一批线程。
pub fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("quickapp-rt")
            .build()
            .expect("build tokio runtime")
    })
}

/* ---------------- 局域网 IP ---------------- */

/// 本机在局域网里的 IPv4。
///
/// 做法：向 8.8.8.8:80 建一个 UDP socket 再看 `local_addr` —— UDP `connect` 不发包，
/// 只是让内核选出口网卡，所以**不需要联网也能拿到正确地址**（拿到的是路由表选出的源地址）。
/// 比遍历网卡简单，且不会把回环/链路本地地址报给手机。
pub fn lan_ip() -> String {
    match UdpSocket::bind("0.0.0.0:0") {
        Ok(sock) => {
            let _ = sock.set_read_timeout(Some(Duration::from_millis(200)));
            if sock.connect("8.8.8.8:80").is_ok() {
                if let Ok(addr) = sock.local_addr() {
                    let ip = addr.ip().to_string();
                    if !ip.starts_with("127.") && !ip.starts_with("0.") {
                        return ip;
                    }
                }
            }
            "127.0.0.1".to_string()
        }
        Err(_) => "127.0.0.1".to_string(),
    }
}
