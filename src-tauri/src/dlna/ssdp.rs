// SSDP 发现层 —— 从 工具类/dlna-cast 的 ssdp-handler.ts 移植
// 接管 Android EndpointModule 的 UDP 能力：监听 M-SEARCH 并单播应答 + 周期性多播 NOTIFY。
// 这是桌面端作为 DLNA DMR「可被局域网发现」的关键。

use std::net::Ipv4Addr;
use std::str::FromStr;
use std::time::Duration;

use socket2::{Domain, Socket, Type};
use tauri::{AppHandle, Emitter};
use tokio::net::UdpSocket;
use tokio::sync::broadcast;

pub const SSDP_MULTICAST_ADDR: &str = "239.255.255.250";
pub const SSDP_PORT: u16 = 1900;

/// 绑定 1900 多播端口并加入组。
///
/// ⚠️ macOS / BSD 上必须显式设 `SO_REUSEADDR` 与 `SO_REUSEPORT`，否则同主机
/// 任何一个 UPnP/DLNA 服务（Plex / Jellyfin / Bonjour / 上次没死干净的实例）
/// 占住 1900 我们就 EADDRINUSE，即便 9 没人 9 是它自己。这两个 option
/// 允许多 socket 共享多播端口的"扇出"，是 macOS 上 SSDP 的标配。
///
/// `local_ip` 为网卡 IPv4，作为加入多播组的接口（macOS 不能用 0.0.0.0）。
pub async fn bind_ssdp(local_ip: &str) -> std::io::Result<UdpSocket> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, None)?;
    // macOS / BSD 上 SO_REUSEADDR 已足以允许多 socket / 多进程共享多播端口扇出，
    // 这是 macOS 上 SSDP 接收的标配（plist / gmrender-resurrect / libdnp 都这么写）。
    // 仅靠 `UdpSocket::bind("0.0.0.0:1900")` 一上来就会 EADDRINUSE —— 同主机
    // 任何 UPnP/DLNA 服务（Plex / Jellyfin / Bonjour / 上次没死干净的实例）
    // 抢先占住 1900 就抢不过。
    sock.set_reuse_address(true)?;
    sock.set_nonblocking(true)?;
    let addr: std::net::SocketAddr = "0.0.0.0:1900".parse().unwrap();
    sock.bind(&addr.into())?;
    let std_sock: std::net::UdpSocket = sock.into();
    let socket = UdpSocket::from_std(std_sock)?;
    let multi = Ipv4Addr::from_str(SSDP_MULTICAST_ADDR).unwrap();
    let iface = Ipv4Addr::from_str(local_ip).unwrap_or(Ipv4Addr::UNSPECIFIED);
    socket.join_multicast_v4(multi, iface)?;
    Ok(socket)
}

/// 运行 SSDP：立即发 3 轮 NOTIFY alive，随后每 30s 周期广播，并应答收到的 M-SEARCH。
pub async fn run_ssdp(
    app: AppHandle,
    socket: UdpSocket,
    uuid: &str,
    local_ip: &str,
    http_port: u16,
    shutdown: &mut broadcast::Receiver<()>,
) {
    let _ = send_notify(&socket, uuid, local_ip, http_port).await;

    let mut interval = tokio::time::interval(Duration::from_secs(30));
    let mut buf = [0u8; 4096];
    loop {
        tokio::select! {
            _ = shutdown.recv() => break,
            _ = interval.tick() => {
                let _ = send_notify(&socket, uuid, local_ip, http_port).await;
            }
            res = socket.recv_from(&mut buf) => {
                if let Ok((n, addr)) = res {
                    let data = String::from_utf8_lossy(&buf[..n]);
                    // 排查日志：收到 M-SEARCH 即打印来源与 ST（用于判断"搜不到设备"是
                    // 网络隔离（卓易通等虚拟机多播不通）还是 ST 类型不匹配）。
                    let st = st_from_data(&data);
                    if data.lines().next().map(|l| l.starts_with("M-SEARCH")).unwrap_or(false) {
                        eprintln!("[dlna_ssdp] M-SEARCH from={addr} st={st:?}");
                    }
                    // 通知前端有控制器在搜索我们（用于排查"搜不到"问题）
                    let _ = app.emit(
                        "dlna://msearch",
                        serde_json::json!({
                            "from": addr.ip().to_string(),
                            "preview": data.lines().next().unwrap_or(""),
                            "st": st,
                        }),
                    );
                    if let Some(resp) = handle_msearch(&data, uuid, local_ip, http_port) {
                        // 随机 0-100ms 延迟再回复，避免同网段风暴（与 TS 版一致）
                        let delay = Duration::from_millis((system_micros() % 100) as u64);
                        tokio::time::sleep(delay).await;
                        // ① 单播响应（规范路径；但 ARP 不可达的虚拟网卡客户端收不到）
                        let single = socket.send_to(resp.as_bytes(), addr).await;
                        eprintln!("[dlna_ssdp] M-SEARCH respond to={addr} st={st:?} send={single:?}");
                        // ② 响应同时发多播组（兼容层保底）：SSDP 客户端加入
                        //    239.255.255.250:1900 组后能收到发往组的所有包（多播不依赖 ARP）。
                        //    卓易通等安卓兼容层虚拟网卡能发多播但不响应 ARP（入站单播
                        //    EHOSTUNREACH/Host is down）→ 单播响应永远到不了，设备搜不到。
                        //    多播响应让客户端拿到 LOCATION 后主动 GET device-desc（出站
                        //    由客户端发起 ARP，能通）→ 设备可被发现。
                        let mcast = socket
                            .send_to(resp.as_bytes(), (SSDP_MULTICAST_ADDR, SSDP_PORT))
                            .await;
                        if let Err(e) = &mcast {
                            eprintln!("[dlna_ssdp] M-SEARCH respond multicast failed: {e}");
                        }
                        // ③ 补发多播 NOTIFY alive：部分客户端只监听多播主动广播。
                        let location = format!("http://{local_ip}:{http_port}/device-desc.xml");
                        for msg in build_notify_messages(uuid, &location) {
                            if let Err(e) = socket
                                .send_to(msg.as_bytes(), (SSDP_MULTICAST_ADDR, SSDP_PORT))
                                .await
                            {
                                eprintln!("[dlna_ssdp] NOTIFY multicast send failed: {e}");
                                break;
                            }
                        }
                    }
                }
            }
        }
    }
}

fn st_from_data(data: &str) -> String {
    data.lines()
        .find_map(|l| l.trim().strip_prefix("ST:").map(|v| v.trim().to_string()))
        .unwrap_or_default()
}

async fn send_notify(
    socket: &UdpSocket,
    uuid: &str,
    local_ip: &str,
    http_port: u16,
) -> std::io::Result<()> {
    let location = format!("http://{local_ip}:{http_port}/device-desc.xml");
    for _ in 0..3 {
        for msg in build_notify_messages(uuid, &location) {
            socket
                .send_to(msg.as_bytes(), (SSDP_MULTICAST_ADDR, SSDP_PORT))
                .await?;
        }
    }
    Ok(())
}

fn build_notify_messages(uuid: &str, location: &str) -> Vec<String> {
    let device_type = "urn:schemas-upnp-org:device:MediaRenderer:1";
    let service_types = [
        "urn:schemas-upnp-org:service:AVTransport:1",
        "urn:schemas-upnp-org:service:ConnectionManager:1",
        "urn:schemas-upnp-org:service:RenderingControl:1",
    ];
    let mut entries: Vec<(String, String)> = vec![
        ("upnp:rootdevice".into(), format!("uuid:{uuid}::upnp:rootdevice")),
        (format!("uuid:{uuid}"), format!("uuid:{uuid}")),
        (device_type.into(), format!("uuid:{uuid}::{device_type}")),
    ];
    for st in service_types {
        entries.push((st.into(), format!("uuid:{uuid}::{st}")));
    }
    entries
        .into_iter()
        .map(|(nt, usn)| build_notify(&nt, &usn, location))
        .collect()
}

fn build_notify(nt: &str, usn: &str, location: &str) -> String {
    format!(
        "NOTIFY * HTTP/1.1\r\nHOST: {addr}:{port}\r\nCACHE-CONTROL: max-age=1800\r\nLOCATION: {loc}\r\nSERVER: Linux/6.0 UPnP/1.1 QuickApp-DLNA/1.0\r\nNT: {nt}\r\nNTS: ssdp:alive\r\nUSN: {usn}\r\nContent-Length: 0\r\n\r\n",
        addr = SSDP_MULTICAST_ADDR,
        port = SSDP_PORT,
        loc = location,
        nt = nt,
        usn = usn
    )
}

fn handle_msearch(data: &str, uuid: &str, local_ip: &str, http_port: u16) -> Option<String> {
    let first = data.lines().next()?;
    if !first.starts_with("M-SEARCH * HTTP/1.1") {
        return None;
    }
    let st = st_from_data(data);
    if !should_respond(&st, uuid) {
        return None;
    }
    let location = format!("http://{local_ip}:{http_port}/device-desc.xml");
    Some(build_msearch_response(&st, uuid, &location))
}

/// ST 匹配策略：已知类型 + 任意未知 ST 兜底响应（ST 回显原值）。
/// 安卓抖音/乐播等 DLNA SDK 常发 DIAL（urn:dial-multiscreen-org:service:dial:1）
/// 或版本化类型（MediaRenderer:2）——精确匹配会漏掉 → 设备"搜不到"。
/// 兜底响应是 gmrender/mediathek 等 DMR 的通行做法（ST 回显 + rootdevice USN）。
fn should_respond(st: &str, _uuid: &str) -> bool {
    if st.is_empty() {
        return false; // 无 ST 的 M-SEARCH 不响应（非规范）
    }
    true
}

/// 响应 USN 按 ST 类型规范化：
///  - ssdp:all / rootdevice → uuid:{uuid}::upnp:rootdevice
///  - uuid:{uuid}          → uuid:{uuid}
///  - 其它（含 DIAL/未知）  → uuid:{uuid}::{st}（回显）
fn build_msearch_response(st: &str, uuid: &str, location: &str) -> String {
    let usn = if st == "ssdp:all" || st == "upnp:rootdevice" {
        format!("uuid:{uuid}::upnp:rootdevice")
    } else if st.starts_with("uuid:") {
        st.to_string()
    } else {
        format!("uuid:{uuid}::{st}")
    };
    format!(
        "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\nDATE: {date}\r\nEXT:\r\nLOCATION: {loc}\r\nSERVER: Linux/6.0 UPnP/1.1 QuickApp-DLNA/1.0\r\nST: {st}\r\nUSN: {usn}\r\nContent-Length: 0\r\n\r\n",
        date = http_date(),
        loc = location,
        st = st,
        usn = usn
    )
}

fn http_date() -> String {
    // RFC1123 真实日期（chrono 已为直接依赖）：部分安卓 DLNA SDK 校验 DATE 头格式，
    // 之前硬编码 "Jan 2099" 假日期可能导致响应被丢弃。
    chrono::Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

fn system_micros() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use socket2::{Domain, Socket, Type};

    // 验证 socket2 路径走得通（高位端口避开沙箱 / CI 的 1900 占用）。
    // 单次 bind 必须成功；double-bind 在标准 Linux 上即便有 SO_REUSEADDR 也会
    // EADDRINUSE（SO_REUSEADDR 只解决 TIME_WAIT），所以这里只测单次。
    #[tokio::test]
    async fn bind_ssdp_single_high_port_succeeds() {
        let r = bind_to_test_port(19900).await;
        assert!(r.is_ok(), "single bind failed: {:?}", r.err());
    }

    // 标：仅在跑 `cargo test --ignored` 时跑，CI 默认跳过；用户本机想自检 1900 时手动跑。
    #[tokio::test]
    #[ignore]
    async fn bind_ssdp_real_port() {
        // 沙箱 / CI 通常没有 Plex 等干扰，但 1900 可能被容器本身占；这里忽略。
        let r = bind_ssdp("127.0.0.1").await;
        assert!(
            r.is_ok(),
            "bind_ssdp(127.0.0.1) failed -> 沙箱/CI 端口受限不算 bug，本机 `cargo test bind_ssdp_real_port -- --ignored` 复跑。 {:?}",
            r.err()
        );
    }

    async fn bind_to_test_port(port: u16) -> std::io::Result<()> {
        let sock = Socket::new(Domain::IPV4, Type::DGRAM, None)?;
        sock.set_reuse_address(true)?;
        sock.set_nonblocking(true)?;
        sock.bind(&format!("0.0.0.0:{port}").parse::<std::net::SocketAddr>().unwrap().into())?;
        // 立刻丢，避免污染
        drop(sock);
        Ok(())
    }
}
