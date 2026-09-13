// DLNA 诊断日志出口：stderr + 落盘。
//
// 为什么要落盘：release 打包后双击运行时 stdout/stderr 无人接管（LaunchServices 不挂终端），
// 真机投屏失败的现场只能靠事后读文件。日志量很小（SSDP 数条/秒、一次投屏数十条 HTTP），
// 每次 append 打开文件的开销可忽略。
//
// 用法与 eprintln! 完全一致：dlog!("[dlna_xxx] a={} b={}", a, b)

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// 单个日志文件上限：超过则清空重写（长会话 / 反复投屏不至于无限增长）。
const MAX_BYTES: u64 = 2 * 1024 * 1024;

static LOG_PATH: Mutex<Option<PathBuf>> = Mutex::new(None);

/// 初始化日志文件（dlna_start 时调用）：append 保留历史，超限则清空重写。
/// 返回日志路径，供排查指引 / UI 展示。
pub fn init(dir: &Path) -> Option<PathBuf> {
    let _ = std::fs::create_dir_all(dir);
    let path = dir.join("dlna.log");
    if std::fs::metadata(&path).map(|m| m.len() > MAX_BYTES).unwrap_or(false) {
        let _ = std::fs::remove_file(&path);
    }
    *LOG_PATH.lock().unwrap() = Some(path.clone());
    line(&format!(
        "==== dlna_start {} ====",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    ));
    line(&format!("log file: {}", path.display()));
    Some(path)
}

/// 当前日志文件路径（未 init 时为 None）。
pub fn path() -> Option<PathBuf> {
    LOG_PATH.lock().unwrap().clone()
}

/// 写一行：stderr + 落盘。落盘失败静默（日志不能反过来影响功能）。
pub fn line(msg: &str) {
    eprintln!("{msg}");
    let p = LOG_PATH.lock().unwrap().clone();
    let Some(p) = p else { return };
    let ts = chrono::Local::now().format("%H:%M:%S%.3f");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
        let _ = writeln!(f, "{ts} {msg}");
    }
}

/// 与 eprintln! 同签名的日志宏，直接替换即可让该行同时落盘。
macro_rules! dlog {
    ($($arg:tt)*) => {
        $crate::dlna::trace::line(&format!($($arg)*))
    };
}
pub(crate) use dlog;
