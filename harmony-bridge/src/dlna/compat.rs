//! Tauri 类型的最小替身，只为让 `src-tauri/src/dlna` 原样搬过来能编。
//!
//! 桌面端 DLNA 模块只依赖 AppHandle 的三件事：`emit`、`clone`、`path()`。
//! 这里对应给到：
//!   - `AppHandle` = `host::HostHandle`（事件进队列，ArkTS 轮询取走）
//!   - `Emitter::emit` = 同名透传
//!   - `Manager::path()` = 由 ArkTS 传入的沙箱目录（`dlna_set_root`）

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use crate::host::HostHandle;

pub use crate::host::HostHandle as AppHandle;

pub trait Emitter {
    fn emit<S: serde::Serialize>(&self, event: &str, payload: S) -> Result<(), ()>;
}

impl Emitter for HostHandle {
    fn emit<S: serde::Serialize>(&self, event: &str, payload: S) -> Result<(), ()> {
        HostHandle.emit(event, payload)
    }
}

/// 沙箱根目录（ArkTS 侧传 `context.filesDir`）。没设置时退到临时目录，
/// 保证 DLNA 日志/设备名配置文件至少能落盘，不会因路径缺失直接崩。
fn root_slot() -> &'static Mutex<Option<PathBuf>> {
    static R: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(None))
}

pub fn set_root(path: &str) {
    let p = PathBuf::from(path.trim());
    let mut g = root_slot().lock().unwrap_or_else(|e| e.into_inner());
    *g = if p.as_os_str().is_empty() { None } else { Some(p) };
}

pub fn root() -> PathBuf {
    let g = root_slot().lock().unwrap_or_else(|e| e.into_inner());
    g.clone().unwrap_or_else(std::env::temp_dir)
}

pub struct Paths;

impl Paths {
    pub fn app_log_dir(&self) -> Result<PathBuf, ()> {
        Ok(root().join("dlna-logs"))
    }

    pub fn app_data_dir(&self) -> Result<PathBuf, ()> {
        Ok(root())
    }
}

pub trait Manager {
    fn path(&self) -> Paths;
}

impl Manager for HostHandle {
    fn path(&self) -> Paths {
        Paths
    }
}
