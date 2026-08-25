// SSDP 发现层 —— 从 工具类/dlna-cast 的 ssdp-handler.ts 移植
// 接管 Android EndpointModule 的 UDP 能力：监听 M-SEARCH 并单播应答 + 周期性多播 NOTIFY。
// 这是桌面端作为 DLNA DMR「可被局域网发现」的关键。

use std::net::Ipv4Addr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use socket2::{Domain, Socket, Type};
use tauri::{AppHandle, Emitter};
use tokio::net::UdpSocket;
use tokio::sync::broadcast;

pub const SSDP_MULTICAST_ADDR: &str = "239.255.255.250";
pub const SSDP_PORT: u16 = 1900;

/// 绑定 1900 多播端口并加入组。
///
/// ⚠️ macOS / BSD 上必须显式设 `SO_REUSEADDR`，否则同主机任何一个
/// UPnP/DLNA 服务（Plex / Jellyfin / Bonjour / 上次没死干净的实例）
/// 占住 1900 我们就 EADDRINUSE。注意：对通配地址 `0.0.0.0:1900`，macOS 的
/// SO_REUSEADDR **不**允许同进程双 socket 共存（实测重建报 os error 48）——
/// 重建必须**先 drop 旧 fd 再 bind**（见 run_ssdp）。
///
/// `ifaces` 为本机所有活跃网卡 IPv4：逐个 `join_multicast_v4` 加入多播组，
/// 保证 WiFi 切换 / 多网卡（WiFi + USB 网卡）下**任何网段**的手机都能收到
/// M-SEARCH（macOS 多播组成员资格按接口绑定，只 join 一个接口则其他网卡收不到）。
/// 单个接口 join 失败（如接口刚失效）仅跳过不整体失败；全部失败才返回 Err。
pub async fn bind_ssdp(ifaces: &[String]) -> std::io::Result<UdpSocket> {
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
    let mut joined = 0usize;
    for iface in ifaces {
        if let Ok(ip) = Ipv4Addr::from_str(iface) {
            match socket.join_multicast_v4(multi, ip) {
                Ok(()) => joined += 1,
                Err(e) => eprintln!("[dlna_ssdp] join multicast on {iface} failed (skip): {e}"),
            }
        }
    }
    if joined == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AddrNotAvailable,
            "no interface could join multicast group",
        ));
    }
    Ok(socket)
}

/// 运行 SSDP：立即发 3 轮 NOTIFY alive，随后每 30s 周期广播，并应答收到的 M-SEARCH。
/// `control_port`（BDLEPORT 扩展头）/`device_id`（UID）/`service_id`（SERVICEID）为
/// 抖音播放列表通道的发现头：只有带这些头的 M-SEARCH 响应 + description.xml 响应，
/// 抖音手机端才会连列表 TCP 端口（对齐 dlna_demo 增强发现）。
///
/// ⚠️ WiFi 切换热重建：socket 以 `Arc<Mutex<Option<UdpSocket>>>` 共享持有。
/// 循环内每 5s 枚举本机真实接口（list_local_ipv4，多网卡下以首个真实接口为 primary），
/// primary 变化时重建 socket：**先 drop 旧 fd 再 bind**（macOS 通配地址 0.0.0.0:1900
/// 不允许双 socket 共存，实测重建 EADDRINUSE os error 48），重新 join 全部接口的
/// 多播组并立即广播 NOTIFY alive，同时回调 `on_ip_changed` 让宿主同步更新 playlist
/// 通道的设备 IP —— 否则切 WiFi 后 LOCATION 仍指向旧 IP，手机 GET device-desc.xml
/// 必失败 → "搜不到设备"。不用"连 8.8.8.8 取默认出口 IP"判定：多网卡（WiFi +
/// USB 网卡/虚拟机网卡）下默认出口可能是非 WiFi 接口（实测 40.64 WiFi + 55.29
/// 第二接口时拿到 55.29），会误判变化并把 LOCATION 指到手机不可达的网段。
pub async fn run_ssdp(
    app: AppHandle,
    socket: UdpSocket,
    uuid: &str,
    mut local_ip: String,
    http_port: u16,
    control_port: Option<u16>,
    device_id: &str,
    service_id: &str,
    on_ip_changed: Box<dyn FnMut(&str) + Send + Sync + 'static>,
    shutdown: &mut broadcast::Receiver<()>,
) {
    let shared: Arc<tokio::sync::Mutex<Option<UdpSocket>>> =
        Arc::new(tokio::sync::Mutex::new(Some(socket)));
    let _ = notify_all(&shared, uuid, &local_ip, http_port, control_port, device_id, service_id).await;

    let mut interval = tokio::time::interval(Duration::from_secs(30));
    let mut ip_check = tokio::time::interval(Duration::from_secs(5));
    let mut on_ip_changed = on_ip_changed;
    let mut buf = [0u8; 4096];
    loop {
        tokio::select! {
            _ = shutdown.recv() => break,
            _ = ip_check.tick() => {
                let ifaces = crate::dlna::list_local_ipv4();
                let primary = ifaces.first().cloned().unwrap_or_default();
                if primary.is_empty() {
                    eprintln!("[dlna_ssdp] ip probe failed (no active interface), keep current {local_ip}");
                    continue;
                }
                if primary != local_ip {
                    eprintln!("[dlna_ssdp] IP changed {local_ip} -> {primary}, rebinding multicast...");
                    // 必须先释放旧 fd：macOS 通配地址 0.0.0.0:1900 不允许双 socket 共存
                    // （SO_REUSEADDR 对通配不生效，实测 os error 48）。
                    *shared.lock().await = None;
                    match bind_ssdp(&ifaces).await {
                        Ok(s) => {
                            *shared.lock().await = Some(s);
                            local_ip = primary;
                            on_ip_changed(&local_ip);
                            eprintln!("[dlna_ssdp] rebound on {local_ip}, announcing alive");
                            notify_all(&shared, uuid, &local_ip, http_port, control_port, device_id, service_id).await;
                        }
                        Err(e) => {
                            // 重建失败：尝试用旧 IP 恢复（旧接口可能仍有效，双网卡兜底）
                            eprintln!("[dlna_ssdp] rebind failed: {e}, restoring old socket...");
                            match bind_ssdp(&[local_ip.clone()]).await {
                                Ok(s) => {
                                    *shared.lock().await = Some(s);
                                    eprintln!("[dlna_ssdp] restored on {local_ip}");
                                }
                                Err(e2) => {
                                    eprintln!("[dlna_ssdp] restore failed too: {e2}, ssdp dead until next probe");
                                }
                            }
                        }
                    }
                }
            }
            _ = interval.tick() => {
                notify_all(&shared, uuid, &local_ip, http_port, control_port, device_id, service_id).await;
            }
            res = recv_packet(&shared, &mut buf) => {
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
                    if let Some(responses) = handle_msearch(&data, uuid, &local_ip, http_port, control_port, device_id, service_id) {
                        // 随机 0-100ms 延迟再回复，避免同网段风暴（与 TS 版一致）
                        let delay = Duration::from_millis((system_micros() % 100) as u64);
                        tokio::time::sleep(delay).await;
                        for resp in &responses {
                            // ① 单播响应（规范路径；但 ARP 不可达的虚拟网卡客户端收不到）
                            let single = {
                                let guard = shared.lock().await;
                                match guard.as_ref() {
                                    Some(s) => s.send_to(resp.as_bytes(), addr).await,
                                    None => Ok(0),
                                }
                            };
                            eprintln!("[dlna_ssdp] M-SEARCH respond to={addr} st={st:?} send={single:?}");
                            // ② 响应同时发多播组（兼容层保底）：SSDP 客户端加入
                            //    239.255.255.250:1900 组后能收到发往组的所有包（多播不依赖 ARP）。
                            //    卓易通等安卓兼容层虚拟网卡能发多播但不响应 ARP（入站单播
                            //    EHOSTUNREACH/Host is down）→ 单播响应永远到不了，设备搜不到。
                            //    多播响应让客户端拿到 LOCATION 后主动 GET device-desc（出站
                            //    由客户端发起 ARP，能通）→ 设备可被发现。
                            let mcast = {
                                let guard = shared.lock().await;
                                match guard.as_ref() {
                                    Some(s) => s.send_to(resp.as_bytes(), (SSDP_MULTICAST_ADDR, SSDP_PORT)).await,
                                    None => Ok(0),
                                }
                            };
                            if let Err(e) = &mcast {
                                eprintln!("[dlna_ssdp] M-SEARCH respond multicast failed: {e}");
                            }
                        }
                        // ③ 补发多播 NOTIFY alive：部分客户端只监听多播主动广播。
                        let location = format!("http://{local_ip}:{http_port}/device-desc.xml");
                        for msg in build_notify_messages(uuid, &location, control_port, device_id, service_id) {
                            let r = {
                                let guard = shared.lock().await;
                                match guard.as_ref() {
                                    Some(s) => s.send_to(msg.as_bytes(), (SSDP_MULTICAST_ADDR, SSDP_PORT)).await,
                                    None => Ok(0),
                                }
                            };
                            if let Err(e) = r {
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

/// 从共享 socket 收包；重建期间 socket 为 None 时挂起（等下一个事件）。
async fn recv_packet(
    shared: &Arc<tokio::sync::Mutex<Option<UdpSocket>>>,
    buf: &mut [u8],
) -> std::io::Result<(usize, std::net::SocketAddr)> {
    let mut guard = shared.lock().await;
    match guard.as_mut() {
        Some(s) => s.recv_from(buf).await,
        None => std::future::pending::<std::io::Result<(usize, std::net::SocketAddr)>>().await,
    }
}

/// 对当前共享 socket 广播 3 轮 NOTIFY alive（socket 为空时静默跳过）。
async fn notify_all(
    shared: &Arc<tokio::sync::Mutex<Option<UdpSocket>>>,
    uuid: &str,
    local_ip: &str,
    http_port: u16,
    control_port: Option<u16>,
    device_id: &str,
    service_id: &str,
) {
    if let Some(s) = shared.lock().await.as_ref() {
        let _ = send_notify(s, uuid, local_ip, http_port, control_port, device_id, service_id).await;
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
    control_port: Option<u16>,
    device_id: &str,
    service_id: &str,
) -> std::io::Result<()> {
    let location = format!("http://{local_ip}:{http_port}/device-desc.xml");
    for _ in 0..3 {
        for msg in build_notify_messages(uuid, &location, control_port, device_id, service_id) {
            socket
                .send_to(msg.as_bytes(), (SSDP_MULTICAST_ADDR, SSDP_PORT))
                .await?;
        }
    }
    Ok(())
}

fn build_notify_messages(
    uuid: &str,
    location: &str,
    control_port: Option<u16>,
    device_id: &str,
    service_id: &str,
) -> Vec<String> {
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
    // ⚠️ 列表功能开启（control_port=Some）时 NOTIFY 带抖音 SERVER 指纹 + 全套扩展头
    //（对齐 demo sendNotify）：抖音/乐播若走被动监听 ssdp:alive 发现设备，NOTIFY 没有
    // 能力头就不会连 BDLEPORT 列表通道（降级为普通 DLNA）。普通 DLNA 客户端忽略未知头。
    // 列表功能关闭（默认，见 playlist::playlist_enabled）→ 标准指纹 + 无扩展头 +
    // max-age=1800，纯公版 DLNA 行为。
    let douyin = control_port.is_some();
    let (ext_str, cache_control, server) = if douyin {
        let ext = crate::dlna::playlist::discovery_headers(control_port, device_id, service_id);
        (
            ext.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect::<String>(),
            "max-age=66",
            crate::dlna::playlist::wire::DOUYIN_SERVER,
        )
    } else {
        (
            String::new(),
            "max-age=1800",
            crate::dlna::playlist::wire::STD_SERVER,
        )
    };
    entries
        .into_iter()
        .map(|(nt, usn)| build_notify(&nt, &usn, location, &ext_str, cache_control, server))
        .collect()
}

fn build_notify(
    nt: &str,
    usn: &str,
    location: &str,
    ext_headers: &str,
    cache_control: &str,
    server: &str,
) -> String {
    format!(
        "NOTIFY * HTTP/1.1\r\nHOST: {addr}:{port}\r\nCACHE-CONTROL: {cache}\r\n{ext}LOCATION: {loc}\r\nSERVER: {server}\r\nNT: {nt}\r\nNTS: ssdp:alive\r\nUSN: {usn}\r\nContent-Length: 0\r\n\r\n",
        addr = SSDP_MULTICAST_ADDR,
        port = SSDP_PORT,
        cache = cache_control,
        loc = location,
        server = server,
        nt = nt,
        usn = usn,
        ext = ext_headers
    )
}

fn handle_msearch(
    data: &str,
    uuid: &str,
    local_ip: &str,
    http_port: u16,
    control_port: Option<u16>,
    device_id: &str,
    service_id: &str,
) -> Option<Vec<String>> {
    let first = data.lines().next()?;
    if !first.starts_with("M-SEARCH * HTTP/1.1") {
        return None;
    }
    let st = st_from_data(data);
    if !should_respond(&st, uuid) {
        return None;
    }
    let location = format!("http://{local_ip}:{http_port}/device-desc.xml");
    if st == "ssdp:all" {
        // 对齐 demo respondToSearch：ssdp:all 需逐类型回多条（每条 ST/USN 独立）。
        // 只回一条会让严格解析的客户端认为服务清单不完整（无 AVTransport → 不可投）。
        let targets = [
            "upnp:rootdevice",
            &format!("uuid:{uuid}"),
            "urn:schemas-upnp-org:device:MediaRenderer:1",
            "urn:schemas-upnp-org:service:AVTransport:1",
            "urn:schemas-upnp-org:service:RenderingControl:1",
            "urn:schemas-upnp-org:service:ConnectionManager:1",
        ];
        Some(
            targets
                .iter()
                .map(|t| build_msearch_response(t, uuid, &location, control_port, device_id, service_id))
                .collect(),
        )
    } else {
        Some(vec![build_msearch_response(
            &st,
            uuid,
            &location,
            control_port,
            device_id,
            service_id,
        )])
    }
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
/// ⚠️ M-SEARCH 响应：列表功能开启（control_port=Some）时带**抖音兼容指纹 SERVER** +
/// 播放列表扩展头（BITMAP/BDLEPORT/UID/SERVICEID/X-User-Agent）——这是抖音手机端
/// 打开列表 TCP 通道的必要条件（dlna_demo 已 A/B 实测——普通 SERVER 时抖音只走
/// SetAVTransportURI/Play，从不连列表端口）。NOTIFY alive 同样带全套头（demo
/// sendNotify 同款）——被动监听发现的路径也不能少。
/// 列表功能关闭（默认）→ 标准指纹 + 无扩展头 + max-age=1800，纯公版行为。
fn build_msearch_response(
    st: &str,
    uuid: &str,
    location: &str,
    control_port: Option<u16>,
    device_id: &str,
    service_id: &str,
) -> String {
    let usn = if st == "ssdp:all" || st == "upnp:rootdevice" {
        format!("uuid:{uuid}::upnp:rootdevice")
    } else if st.starts_with("uuid:") {
        st.to_string()
    } else {
        format!("uuid:{uuid}::{st}")
    };
    let douyin = control_port.is_some();
    let (ext_str, cache_control, server) = if douyin {
        let ext = crate::dlna::playlist::discovery_headers(control_port, device_id, service_id);
        (
            ext.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect::<String>(),
            "max-age=66",
            crate::dlna::playlist::wire::DOUYIN_SERVER,
        )
    } else {
        (
            String::new(),
            "max-age=1800",
            crate::dlna::playlist::wire::STD_SERVER,
        )
    };
    format!(
        "HTTP/1.1 200 OK\r\nCACHE-CONTROL: {cache}\r\nDATE: {date}\r\nEXT:\r\nLOCATION: {loc}\r\nSERVER: {server}\r\nST: {st}\r\nUSN: {usn}\r\n{ext}Content-Length: 0\r\n\r\n",
        cache = cache_control,
        date = http_date(),
        loc = location,
        server = server,
        st = st,
        usn = usn,
        ext = ext_str
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

    // NOTIFY alive 必须带抖音 SERVER 指纹 + 全套扩展头（demo sendNotify 对齐）。
    // 被动监听发现的抖音端依赖这些头决定是否连 BDLEPORT 列表通道。
    #[test]
    fn notify_carries_douyin_fingerprint_and_playlist_headers() {
        let msgs = build_notify_messages("test-uuid", "http://192.168.1.2:5001/device-desc.xml", Some(45165), "12345", "svc-1");
        assert!(!msgs.is_empty());
        for m in &msgs {
            assert!(m.contains(&format!("SERVER: {}", crate::dlna::playlist::wire::DOUYIN_SERVER)), "NOTIFY missing douyin SERVER: {m}");
            assert!(m.contains("BITMAP: 0x800"), "NOTIFY missing BITMAP: {m}");
            assert!(m.contains("BDLEPORT: 45165"), "NOTIFY missing BDLEPORT: {m}");
            assert!(m.contains("UID: 12345"), "NOTIFY missing UID: {m}");
            assert!(m.contains("SERVICEID: svc-1"), "NOTIFY missing SERVICEID: {m}");
            assert!(m.contains("X-User-Agent: redsonic"), "NOTIFY missing X-User-Agent: {m}");
        }
    }

    // ssdp:all 必须逐类型回多条（严格客户端按每条 USN 建服务清单，只回一条会判不可投）。
    #[test]
    fn msearch_ssdp_all_returns_one_response_per_target() {
        let data = "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\nST: ssdp:all\r\n\r\n";
        let responses = handle_msearch(data, "test-uuid", "192.168.1.2", 5001, Some(45165), "12345", "svc-1").unwrap();
        assert_eq!(responses.len(), 6);
        for r in &responses {
            assert!(r.contains("BITMAP: 0x800"));
            assert!(r.contains("BDLEPORT: 45165"));
        }
        let sts: Vec<&str> = responses.iter().filter_map(|r| {
            r.lines().find(|l| l.starts_with("ST: ")).map(|l| l.trim_start_matches("ST: "))
        }).collect();
        assert!(sts.contains(&"upnp:rootdevice"));
        assert!(sts.contains(&"uuid:test-uuid"));
        assert!(sts.contains(&"urn:schemas-upnp-org:device:MediaRenderer:1"));
        assert!(sts.contains(&"urn:schemas-upnp-org:service:AVTransport:1"));
    }

    // 单一 ST 查询仍回单条（回显 ST）。
    #[test]
    fn msearch_single_st_returns_single_response() {
        let data = "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\nST: urn:schemas-upnp-org:device:MediaRenderer:1\r\n\r\n";
        let responses = handle_msearch(data, "test-uuid", "192.168.1.2", 5001, Some(45165), "12345", "svc-1").unwrap();
        assert_eq!(responses.len(), 1);
        assert!(responses[0].contains("ST: urn:schemas-upnp-org:device:MediaRenderer:1"));
        assert!(responses[0].contains("USN: uuid:test-uuid::urn:schemas-upnp-org:device:MediaRenderer:1"));
    }

    // 列表功能关闭（control_port=None）→ NOTIFY/M-SEARCH 回标准指纹 + 无扩展头 +
    // max-age=1800（纯公版 DLNA 行为；开启时才带抖音指纹与 BDLEPORT 能力头）。
    #[test]
    fn discovery_falls_back_to_std_when_playlist_disabled() {
        let msgs = build_notify_messages("test-uuid", "http://192.168.1.2:5001/device-desc.xml", None, "12345", "svc-1");
        assert!(!msgs.is_empty());
        for m in &msgs {
            assert!(m.contains(&format!("SERVER: {}", crate::dlna::playlist::wire::STD_SERVER)), "NOTIFY should use std SERVER when playlist disabled: {m}");
            assert!(m.contains("CACHE-CONTROL: max-age=1800"), "NOTIFY should use max-age=1800 when playlist disabled: {m}");
            assert!(!m.contains("BDLEPORT"), "NOTIFY must not carry BDLEPORT when playlist disabled: {m}");
            assert!(!m.contains("BITMAP"), "NOTIFY must not carry BITMAP when playlist disabled: {m}");
            assert!(!m.contains("X-User-Agent"), "NOTIFY must not carry X-User-Agent when playlist disabled: {m}");
        }
        let data = "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\nST: upnp:rootdevice\r\n\r\n";
        let responses = handle_msearch(data, "test-uuid", "192.168.1.2", 5001, None, "12345", "svc-1").unwrap();
        for r in &responses {
            assert!(r.contains(&format!("SERVER: {}", crate::dlna::playlist::wire::STD_SERVER)), "M-SEARCH should use std SERVER when playlist disabled: {r}");
            assert!(!r.contains("BDLEPORT"), "M-SEARCH must not carry BDLEPORT when playlist disabled: {r}");
        }
    }

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
        let r = bind_ssdp(&["127.0.0.1".to_string()]).await;
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
