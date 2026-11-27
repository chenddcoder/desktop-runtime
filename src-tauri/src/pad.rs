//! 手机手柄 · UDP 快通道 —— 补 Android 插件 `com.quicktvui.plugin.wasm` 里 `PadModule` 的能力。
//!
//! ## 协议（与小程序 `utils/cast/channel.js` 逐字一致，只增不改）
//! ```text
//! 手机 → 电视   "seq,mask"              1P（旧手机只发两段）
//!               "seq,mask,t"            第 3 段是手机毫秒戳，只进日志，不算延迟
//!               "seq,mask,t,seat"       seat = 0/1（新手机才发）
//! 电视 → 手机   "ack,seq[,seat]"        原样回传序号 + 实际座位（旧手机只读前两段）
//! 探活          手机发 "ping"，电视回 "ack,-1,2"（2 = 支持的座位数，不碰按键）
//! ```
//! `mask` 位：bit0=A bit1=B bit2=Select bit3=Start bit4=Up bit5=Down bit6=Left bit7=Right。
//!
//! ## 为什么桌面端也要这一层
//! 电视上 `plugin-wasm` 的 `PadModule.init()` 自带 `startInternal(5991)`，UDP 收包**直落内核**
//! （`WasmStage.offerPadInput`），JS 不参与。桌面壳没有这个插件，但手机小程序走的是同一个
//! IP + 5991 —— 不起这个端口，桌面端就只能靠 5990 落盘那条慢通道（100ms+）。
//!
//! ## ⛔ 与落盘通道的关系：桌面端**必须**由 JS 下发
//! 电视上 UDP 通道是「内核直收」，所以 JS 要彻底收手（否则两路抢同一份按键表）。
//! 桌面端内核跑在 JS 里（`WebWasmBackend`），Rust 只能把包**交给 JS**：
//!   · 事件 `pad://input` 只携带位图，不直接写内核；
//!   · 页面按「位图变了才下发」写进模拟器（见 `Play.vue::syncPad`）。
//! 所以同一条 UDP 通道，两边在 JS 层的处理是相反的 —— 这个差异写在
//! `pad-udp.ts` 的桌面分支里，别在主流程里混着判。
//!
//! ## 端口
//! 首选 5991（手机端写死）；被占就顺延最多 10 个。**信返回值里的 port**，别硬编码 ——
//! 顺延之后手机打 5991 是打不通的，页面要能把真实端口显示出来。

use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Emitter};

/// 手机端写死的端口。被占就顺延（见 PREFERRED_PORT..+PORT_SCAN）
const PREFERRED_PORT: u16 = 5991;
const PORT_SCAN: u16 = 10;
/// 单包上限：手柄包最多几十字节，64 字节足够，超了直接丢
const MAX_PACKET: usize = 64;

#[derive(Clone, Debug, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PadInfo {
    pub ok: bool,
    pub port: u16,
    pub target: String,
    pub error: String,
}

#[derive(Clone, Debug, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PadStats {
    pub running: bool,
    pub port: u16,
    /// 收到的包数
    pub recv: u64,
    /// 被页面「取走并下发」的次数（桌面端由事件驱动，近似等于位图变化次数）
    pub applied: u64,
    /// 序号缺口总数（估算丢包）
    pub seq_gap: u64,
    /// 解析失败 / 超长包
    pub dropped: u64,
    /// 距离最近一次收包的毫秒数；**从没收到过是 0**
    pub last_recv_ago_ms: u64,
    /// 最近一个包的位图（seat0；桌面轮询兜底只还原 1P，2P 靠事件里的 seat）
    pub mask: u32,
    pub target: String,
    pub error: String,
}

struct PadRt {
    running: bool,
    port: u16,
    /// 按座位各存一份最近位图：位图没变就不推事件
    masks: [u32; 2],
    recv: u64,
    applied: u64,
    seq_gap: u64,
    dropped: u64,
    last_recv: Option<Instant>,
    /// 上一包的序号；-1 = 还没收过（手机重启 seq 归零时不能算成大丢包）
    last_seq: i64,
    target: String,
    error: String,
    stop: Option<Arc<AtomicBool>>,
}

impl Default for PadRt {
    fn default() -> Self {
        Self {
            running: false,
            port: 0,
            masks: [0, 0],
            recv: 0,
            applied: 0,
            seq_gap: 0,
            dropped: 0,
            last_recv: None,
            last_seq: -1,
            target: String::new(),
            error: String::new(),
            stop: None,
        }
    }
}

fn rt() -> &'static Mutex<PadRt> {
    static RT: OnceLock<Mutex<PadRt>> = OnceLock::new();
    RT.get_or_init(|| Mutex::new(PadRt::default()))
}

/// 解析 `seq,mask[,t[,seat]]`。
///
/// 任何一段不合法就整包判死（宁可丢一包，也不要把错位数据当按键）。
/// 第 3 段（时间戳）只用于日志；第 4 段 `seat` 缺省为 0（旧手机只发 2~3 段）。
fn parse_packet(text: &str) -> Option<(i64, u32, u8)> {
    let mut it = text.trim().split(',');
    let seq: i64 = it.next()?.trim().parse().ok()?;
    let mask: u32 = it.next()?.trim().parse().ok()?;
    let mut seat: u8 = 0;
    if let Some(ts) = it.next() {
        // 第三段存在时再看有没有第四段；`ts` 本身只进日志，不校验
        let _ = ts;
        if let Some(s) = it.next() {
            let v: i64 = s.trim().parse().ok()?;
            if v != 0 && v != 1 {
                return None; // 非法座位整包丢弃，绝不把 2P 串到 1P
            }
            seat = v as u8;
        }
    }
    Some((seq, mask, seat))
}

/// 位图 → 8 个键（0/1）。顺序与 `pad-core.ts` 的 `b[]` 一致：
/// 上、下、左、右、A、B、SELECT、START
fn bits_of(mask: u32) -> [u8; 8] {
    [
        ((mask >> 4) & 1) as u8, // Up
        ((mask >> 5) & 1) as u8, // Down
        ((mask >> 6) & 1) as u8, // Left
        ((mask >> 7) & 1) as u8, // Right
        (mask & 1) as u8,        // A
        ((mask >> 1) & 1) as u8, // B
        ((mask >> 2) & 1) as u8, // Select
        ((mask >> 3) & 1) as u8, // Start
    ]
}

fn spawn_loop(app: AppHandle, sock: UdpSocket, stop: Arc<AtomicBool>) {
    std::thread::Builder::new()
        .name("es-pad-udp".into())
        .spawn(move || {
            let mut buf = [0u8; MAX_PACKET + 1];
            // 500ms 超时：既让循环能及时看到 stop 标志，又不会空转烧 CPU
            let _ = sock.set_read_timeout(Some(Duration::from_millis(500)));
            while !stop.load(Ordering::Relaxed) {
                let (n, peer) = match sock.recv_from(&mut buf) {
                    Ok(v) => v,
                    Err(e) => {
                        // WouldBlock / TimedOut 是正常心跳，别当错误刷屏
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) {
                            continue;
                        }
                        let mut g = rt().lock().unwrap_or_else(|e| e.into_inner());
                        g.error = format!("recv: {e}");
                        continue;
                    }
                };
                if n == 0 || n > MAX_PACKET {
                    let mut g = rt().lock().unwrap_or_else(|e| e.into_inner());
                    g.dropped += 1;
                    continue;
                }
                let text = match std::str::from_utf8(&buf[..n]) {
                    Ok(s) => s,
                    Err(_) => {
                        let mut g = rt().lock().unwrap_or_else(|e| e.into_inner());
                        g.dropped += 1;
                        continue;
                    }
                };

                // 探活：只回执，不碰按键（手机用它判「这台电视在不在听」）
                // 第 3 段 = 支持的座位数，新手机据此决定能不能选 2P
                if text.trim().eq_ignore_ascii_case("ping") {
                    let _ = sock.send_to(b"ack,-1,2", peer);
                    continue;
                }

                let (seq, mask, seat) = match parse_packet(text) {
                    Some(v) => v,
                    None => {
                        let mut g = rt().lock().unwrap_or_else(|e| e.into_inner());
                        g.dropped += 1;
                        continue;
                    }
                };

                // 先回执再记状态：手机侧 800ms 超时回收，回执越早越好。
                // 带回实际座位，手机据此提示「座位被占」/决定 1P、2P。
                let _ = sock.send_to(format!("ack,{seq},{seat}").as_bytes(), peer);

                let changed = {
                    let mut g = rt().lock().unwrap_or_else(|e| e.into_inner());
                    g.recv += 1;
                    g.last_recv = Some(Instant::now());
                    // 序号缺口：只在确实前进时算，手机重启（seq 归零）不能算丢包
                    if g.last_seq >= 0 && seq > g.last_seq {
                        let jump = seq - g.last_seq - 1;
                        if jump > 0 && jump < 1000 {
                            g.seq_gap += jump as u64;
                        }
                    }
                    g.last_seq = seq;
                    let si = seat as usize;
                    let changed = g.masks[si] != mask;
                    g.masks[si] = mask;
                    if changed {
                        g.applied += 1;
                    }
                    changed
                };

                /*
                 * 只在**位图变化**时推事件。
                 * 手机松手后仍然按 20Hz 发中立包（mask=0），逐包推会把 webview 的
                 * 事件队列喂成噪音，而 JS 侧拿到的状态完全一样。
                 * 变化时推一次就够 —— 对齐「按下/抬起」这两个瞬间。
                 */
                if changed {
                    let _ = app.emit(
                        "pad://input",
                        serde_json::json!({
                            "mask": mask,
                            "bits": bits_of(mask).to_vec(),
                            "seq": seq,
                            "seat": seat,
                        }),
                    );
                }
            }
        })
        .ok();
}

/// 起监听（幂等）。已在听时直接返回当前信息，不重复 bind。
#[tauri::command]
pub fn pad_start(app: AppHandle, port: Option<u16>) -> PadInfo {
    {
        let g = rt().lock().unwrap_or_else(|e| e.into_inner());
        if g.running {
            return PadInfo {
                ok: true,
                port: g.port,
                target: g.target.clone(),
                error: String::new(),
            };
        }
    }

    let first = port.unwrap_or(PREFERRED_PORT);
    let mut bound: Option<(UdpSocket, u16)> = None;
    let mut last_err = String::new();
    for p in first..first.saturating_add(PORT_SCAN) {
        // 绑 0.0.0.0：手机在局域网里从任意网卡进来，绑回环就收不到
        match UdpSocket::bind(("0.0.0.0", p)) {
            Ok(s) => {
                bound = Some((s, p));
                break;
            }
            Err(e) => last_err = format!("bind {p}: {e}"),
        }
    }

    let (sock, real_port) = match bound {
        Some(v) => v,
        None => {
            let mut g = rt().lock().unwrap_or_else(|e| e.into_inner());
            g.error = last_err.clone();
            return PadInfo {
                ok: false,
                port: 0,
                target: String::new(),
                error: last_err,
            };
        }
    };

    let stop = Arc::new(AtomicBool::new(false));
    {
        let mut g = rt().lock().unwrap_or_else(|e| e.into_inner());
        g.running = true;
        g.port = real_port;
        g.error.clear();
        g.stop = Some(stop.clone());
    }
    spawn_loop(app, sock, stop);

    let g = rt().lock().unwrap_or_else(|e| e.into_inner());
    PadInfo {
        ok: true,
        port: g.port,
        target: g.target.clone(),
        error: String::new(),
    }
}

/// 拉一次读数（页面每 ~500ms 调一次就够）。
#[tauri::command]
pub fn pad_stats() -> PadStats {
    let g = rt().lock().unwrap_or_else(|e| e.into_inner());
    PadStats {
        running: g.running,
        port: g.port,
        recv: g.recv,
        applied: g.applied,
        seq_gap: g.seq_gap,
        dropped: g.dropped,
        // ⛔ 没收到过包一律 0（不是 -1）：JS 侧判活先要求 recv > 0
        last_recv_ago_ms: g
            .last_recv
            .map(|t| t.elapsed().as_millis().min(u64::MAX as u128) as u64)
            .unwrap_or(0),
        mask: g.masks[0],
        target: g.target.clone(),
        error: g.error.clone(),
    }
}

/// 目标实例。桌面端没有 wasm 实例概念，只记录（页面显示用），保持与 Android 版同形。
#[tauri::command]
pub fn pad_target(target: String) -> bool {
    let mut g = rt().lock().unwrap_or_else(|e| e.into_inner());
    g.target = target;
    true
}

/// 停监听（收回端口）。下次 `pad_start` 会重新 bind。
#[tauri::command]
pub fn pad_stop() -> bool {
    let (stop, was) = {
        let mut g = rt().lock().unwrap_or_else(|e| e.into_inner());
        let was = g.running;
        let stop = g.stop.take();
        g.running = false;
        g.port = 0;
        g.masks = [0, 0];
        (stop, was)
    };
    if let Some(s) = stop {
        s.store(true, Ordering::Relaxed);
    }
    was
}

/// 清计数（换一局时用；与 Android 版 `startPad` 内部归零同一口径）
#[tauri::command]
pub fn pad_reset() -> bool {
    let mut g = rt().lock().unwrap_or_else(|e| e.into_inner());
    g.recv = 0;
    g.applied = 0;
    g.seq_gap = 0;
    g.dropped = 0;
    g.last_seq = -1;
    g.masks = [0, 0];
    g.last_recv = None;
    true
}
/* ---------------- 单元测试：协议解析（不起端口也能验） ---------------- */

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ok() {
        // 旧手机：2 段 / 3 段，座位缺省 0
        assert_eq!(parse_packet("12,96"), Some((12, 96, 0)));
        assert_eq!(parse_packet("12,96,1730000000000"), Some((12, 96, 0)));
        assert_eq!(parse_packet(" 7 , 8 ,0"), Some((7, 8, 0)));
        // 新手机：第 4 段带座位
        assert_eq!(parse_packet("12,96,1730000000000,1"), Some((12, 96, 1)));
        assert_eq!(parse_packet("12,96,0,0"), Some((12, 96, 0)));
    }

    #[test]
    fn parse_reject() {
        assert_eq!(parse_packet("ping"), None);
        assert_eq!(parse_packet("12"), None);
        assert_eq!(parse_packet("a,b,c"), None);
        assert_eq!(parse_packet(""), None);
        // 非法座位整包丢弃
        assert_eq!(parse_packet("12,96,0,2"), None);
        assert_eq!(parse_packet("12,96,0,x"), None);
    }

    /// 位序必须和 `pad-core.ts` 的 `b[]` 对齐：上、下、左、右、A、B、SEL、STA
    #[test]
    fn bits_layout() {
        // START = bit3
        assert_eq!(bits_of(8), [0, 0, 0, 0, 0, 0, 0, 1]);
        // DOWN+LEFT = bit5|bit6 = 96
        assert_eq!(bits_of(96), [0, 1, 1, 0, 0, 0, 0, 0]);
        // 全按
        assert_eq!(bits_of(0xff), [1, 1, 1, 1, 1, 1, 1, 1]);
    }

    /// 真起一个 UDP socket 走一遍「手机发包 → 收到 → 回 ack」，
    /// 只验协议层，不碰 Tauri（`AppHandle` 只有 runtime 里能构造）。
    #[test]
    fn udp_roundtrip() {
        let srv = UdpSocket::bind(("127.0.0.1", 0)).expect("bind");
        let addr = srv.local_addr().expect("addr");
        let cli = UdpSocket::bind(("127.0.0.1", 0)).expect("bind cli");
        cli.set_read_timeout(Some(Duration::from_millis(1500))).ok();

        cli.send_to(b"3,96,1730000000000", addr).ok();
        let mut buf = [0u8; MAX_PACKET + 1];
        let (n, _) = srv.recv_from(&mut buf).expect("recv");
        let text = std::str::from_utf8(&buf[..n]).unwrap();
        assert_eq!(parse_packet(text), Some((3, 96, 0)));
        // 回执走生产同一份格式
        srv.send_to(format!("ack,{}", 3).as_bytes(), cli.local_addr().unwrap()).ok();
        let (m, _) = cli.recv_from(&mut buf).expect("ack");
        assert_eq!(&buf[..m], b"ack,3");
    }
}
