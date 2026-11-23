//! 时间格式化：只依赖 std，避免把 libtime_service_ndk.so 拖进 NEEDED。
//!
//! 为什么不要 chrono：`chrono::Local` / `Utc::now()` 在 OHOS 上会链接
//! `libtime_service_ndk.so`（`OH_TimeService_GetTimeZone`）。这个库不一定在所有
//! 设备的应用沙箱里可见，dlopen 失败的表现就是页面拿到
//! `{"__error__":"rust bridge not loaded"}` —— 整个桥都不可用，代价太大。
//! 日志和 HTTP `DATE` 头只需要 UTC 时间，手写折算足够。

use std::time::{SystemTime, UNIX_EPOCH};

const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// 1970-01-01 起的天数 → (year, month, day)。Howard Hinnant 的 civil_from_days。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn parts() -> (i64, u32, u32, u64, u64, u64, u64) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let millis = now.subsec_millis() as u64;
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    (y, m, d, rem / 3600, (rem % 3600) / 60, rem % 60, millis)
}

/// `Sun, 21 Nov 2026 13:25:16 GMT` —— HTTP/SSDP 的 DATE 头。
pub fn rfc1123_now() -> String {
    let (y, m, d, hh, mm, ss, _) = parts();
    let days = (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        / 86_400) as usize;
    let wd = WEEKDAYS[days % 7];
    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
        wd,
        d,
        MONTHS[(m as usize).saturating_sub(1)],
        y,
        hh,
        mm,
        ss
    )
}

/// `2026-11-21 13:25:16`（UTC）—— 日志落盘用。
pub fn date_time_now() -> String {
    let (y, m, d, hh, mm, ss, _) = parts();
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", y, m, d, hh, mm, ss)
}

/// `13:25:16.123`（UTC）—— 日志行前缀。
pub fn clock_now() -> String {
    let (_, _, _, hh, mm, ss, ms) = parts();
    format!("{:02}:{:02}:{:02}.{:03}", hh, mm, ss, ms)
}
