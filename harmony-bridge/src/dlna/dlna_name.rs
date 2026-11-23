// DLNA 设备名持久化：默认名"投屏显象_xx.xx"，存 JSON 文件，重启保留。
use std::path::PathBuf;

/// 本条 DLNA 设备名配置文件（跟随可执行文件旁，与 devtools.json 同一套定位逻辑）
const FILE_NAME: &str = "dlna-name.json";

/// 配置文件路径：优先 current_dir，兜底可执行文件旁（参考 main.rs devtools_enabled）。
fn config_path() -> Option<PathBuf> {
    if let Ok(d) = std::env::current_dir() {
        let p = d.join(FILE_NAME);
        if p.exists() {
            return Some(p);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let p = parent.join(FILE_NAME);
            return Some(p);
        }
    }
    None
}

/// 读取已持久化的设备名（无文件/解析失败返回 None，静默降级）。
pub fn read_stored_name() -> Option<String> {
    let p = config_path()?;
    let text = std::fs::read_to_string(p).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get("name")?.as_str().map(String::from).filter(|s| !s.is_empty())
}

/// 持久化设备名；任一步失败静默降级（不阻断 DLNA）。
pub fn write_stored_name(name: &str) {
    let p = match config_path() {
        Some(p) => p,
        None => return,
    };
    let json = serde_json::json!({ "name": name }).to_string();
    let _ = std::fs::write(p, json);
}

/// 生成默认名：投屏显象_xx.xx（取 IPv4 后两段，缺省用 00.00）。
pub fn default_name(local_ip: &str) -> String {
    let seg: Vec<&str> = local_ip.split('.').collect();
    if seg.len() == 4 {
        format!("投屏显象_{}.{}", seg[2], seg[3])
    } else {
        "投屏显象_00.00".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_name_uses_last_two_segments() {
        assert_eq!(default_name("192.168.1.23"), "投屏显象_1.23");
        assert_eq!(default_name("10.0.0.5"), "投屏显象_0.5");
    }

    #[test]
    fn default_name_falls_back_on_malformed_ip() {
        assert_eq!(default_name("not-an-ip"), "投屏显象_00.00");
        assert_eq!(default_name("192.168.1"), "投屏显象_00.00");
    }
}