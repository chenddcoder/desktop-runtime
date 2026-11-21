//! 宿主机自测：在没有鸿蒙设备的 Mac 上验证**移植版** WebDAV 服务端。
//!
//! 为什么能这么测：`harmony-bridge/src/webdav.rs` 是纯 Rust（std + tokio + serde），
//! 一行鸿蒙 API 都没有；只有 `tauri::AppHandle` 换成了本地的 `host::HostHandle`。
//! 所以同一份源码能在 macOS 上编译运行，行为与真机一致 —— 这样改 webdav 逻辑就不必
//! 「编 HAP → 装机 → 手机投一个文件」才能知道对不对。
//!
//! 用法：
//!   cargo run --release                       # 起服务 + 持续打印事件与落盘列表
//!   WD_ROOT=/tmp/files cargo run --release    # 指定落盘根（默认 /tmp/webdav-host-files）
//!   WD_READ=/tmp/files/es_files/a.nes cargo run --release   # 只测读 base64（含越界拒绝）
//!
//! 配合 curl 验证：
//!   curl --noproxy '*' -X PUT --data-binary @demo.nes \
//!     "http://127.0.0.1:5990/es_files/files/nes/demo.nes"

#[path = "../../../harmony-bridge/src/host.rs"]
mod host;
#[path = "../../../harmony-bridge/src/webdav.rs"]
mod webdav;

use std::time::Duration;

fn main() {
    let root = std::env::var("WD_ROOT").unwrap_or_else(|_| "/tmp/webdav-host-files".to_string());
    webdav::set_root(&root);
    let rt = host::runtime();

    // 只测「读文件」这一条：用来验 base64 无损 + 越界拒绝
    if let Ok(path) = std::env::var("WD_READ") {
        match rt.block_on(webdav::es_read_file_base64(path)) {
            Ok(b64) => println!("READ ok len={} b64={}", b64.len(), b64),
            Err(e) => println!("READ fail {e}"),
        }
        return;
    }

    match rt.block_on(webdav::webdav_start(Some(5990))) {
        Ok(i) => println!("START ok url={} port={} root={}", i.url, i.port, i.root),
        Err(e) => {
            println!("START fail {e}");
            return;
        }
    }

    // 等 accept 循环就位，再持续把事件泵出来（模拟 ArkTS 侧 80ms 轮询）
    std::thread::sleep(Duration::from_millis(200));
    let mut last: Vec<String> = Vec::new();
    for _ in 0..80 {
        std::thread::sleep(Duration::from_millis(250));
        let events = host::drain();
        if !events.is_empty() {
            println!("EVENTS {}", serde_json::to_string(&events).unwrap());
        }
        let files = rt.block_on(webdav::es_list_files()).unwrap_or_default();
        if files != last {
            println!("LIST {files:?}");
            last = files;
        }
    }
    let _ = rt.block_on(webdav::webdav_stop());
    println!("STOP");
}
