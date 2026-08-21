// DLNA 服务端（DMR）模块总入口
// 把 工具类/dlna-cast 的 JS 协议栈（依赖 Android EndpointModule 起 HTTP/UDP）整体移植到 Rust：
//   - device_desc : 设备描述 / SCPD 的 XML 生成
//   - av_transport: 投屏状态机
//   - soap        : SOAP 解析 / 构建 / 动作处理
//   - ssdp        : UDP 多播发现（替代 EndpointModule 的 UDP 监听）
//   - http_server : 本地 HTTP server（替代 EndpointModule 的本地 HTTP server）
// 前端通过 invoke("dlna_start"/"dlna_stop") 启动/停止；收到投屏时 emit("dlna://play") 给前端播放。

pub mod av_transport;
pub mod device_desc;
pub mod dlna_name;
pub mod http_server;
pub mod soap;
pub mod ssdp;

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use tauri::AppHandle;
use tokio::sync::broadcast;

use av_transport::AvTransport;
use device_desc::DeviceDesc;

#[derive(serde::Serialize, Clone)]
pub struct DlnaStartResult {
    pub success: bool,
    pub port: u16,
    pub uuid: String,
}

struct Inner {
    running: AtomicBool,
    shutdown_tx: broadcast::Sender<()>,
    // 启动成功后记录端口/uuid，供 dlna_status 轮询返回权威状态（前端不再依赖一次性事件）。
    port: Mutex<Option<u16>>,
    uuid: Mutex<String>,
    // 投屏播放状态机在 dlna_start 内创建后存入，供前端 report_position / GetPositionInfo 读取真实进度。
    av: Mutex<Option<Arc<AvTransport>>>,
    // DLNA 设备名单源：为 None 表示尚未确定（未加载持久化/未启动）。
    friendly_name: Mutex<Option<String>>,
    // 运行中的 DeviceDesc（dlna_start 成功后存入），set_dlna_name 热更新广播名时取出调用。
    desc: Mutex<Option<Arc<DeviceDesc>>>,
}

static STATE: OnceLock<Arc<Inner>> = OnceLock::new();

fn state() -> &'static Arc<Inner> {
    STATE.get_or_init(|| {
        let (tx, _rx) = broadcast::channel::<()>(1);
        Arc::new(Inner {
            running: AtomicBool::new(false),
            shutdown_tx: tx,
            port: Mutex::new(None),
            uuid: Mutex::new(String::new()),
            av: Mutex::new(None),
            friendly_name: Mutex::new(None),
            desc: Mutex::new(None),
        })
    })
}

/// 前端轮询用的权威状态：不依赖一次性 emit（auto-start 成功事件往往在监听者注册前发出，会被吞）。
#[derive(serde::Serialize, Clone)]
pub struct DlnaStatus {
    pub running: bool,
    pub port: Option<u16>,
    pub uuid: String,
}

#[tauri::command]
pub fn dlna_status() -> DlnaStatus {
    let inner = state();
    let running = inner.running.load(Ordering::SeqCst);
    let port = *inner.port.lock().unwrap();
    let uuid = inner.uuid.lock().unwrap().clone();
    DlnaStatus { running, port, uuid }
}

/// 获取本机局域网 IPv4（UDP connect 技巧：连一个外部地址，取本地出口地址）。
/// 加 3s 超时：没外网 / 没默认路由时会无限阻塞，会把整个 DLNA 启动流程卡死。
async fn get_local_ip() -> std::io::Result<String> {
    let fut = async {
        let socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
        socket.connect("8.8.8.8:80").await?;
        let addr = socket.local_addr()?;
        Ok::<String, std::io::Error>(addr.ip().to_string())
    };
    match tokio::time::timeout(std::time::Duration::from_secs(3), fut).await {
        Ok(r) => r,
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "get_local_ip timeout (no default route to 8.8.8.8?)",
        )),
    }
}

/// 失败时尽量 dump 谁占了端口（mac 用 lsof / linux 用 ss），便于一眼看到 Plex / 上次进程残留。
fn port_holder_hint(_port: u16, kind: std::io::ErrorKind) -> String {
    // 只在 ADDRINUSE 之类看起来"被占"时调用，避免无谓的进程 spawn。
    if !matches!(
        kind,
        std::io::ErrorKind::AddrInUse
            | std::io::ErrorKind::PermissionDenied
            | std::io::ErrorKind::AlreadyExists
    ) {
        return String::new();
    }
    use std::process::Command;
    let cmds: &[(&str, &[&str])] = if cfg!(target_os = "linux") {
        &[
            ("ss", &["-ulnp", "sport", "=", ":1900"]),
            ("lsof", &["-nP", "-iUDP:1900"]),
        ]
    } else {
        &[
            ("lsof", &["-nP", "-iUDP:1900"]),
            ("lsof", &["-nP", "-i:1900"]),
        ]
    };
    let mut out = String::from(" 【端口占用诊断】");
    for (bin, args) in cmds.iter() {
        match Command::new(bin).args(*args).output() {
            Ok(o) if o.status.success() && !o.stdout.is_empty() => {
                let s = String::from_utf8_lossy(&o.stdout);
                let trimmed = s.lines().take(12).collect::<Vec<_>>().join("\n  ");
                out.push_str(&format!("\n  $ {bin} {}\n  {trimmed}", args.join(" ")));
                break;
            }
            _ => continue,
        }
    }
    if out == " 【端口占用诊断】" {
        out.push_str("\n  (无可用查询工具，请手动 `lsof -nP -iUDP:1900`)");
    }
    out
}

/// 绑定 HTTP 端口：优先 preferred，失败则从小范围递进试探。
async fn bind_http_port(preferred: u16) -> std::io::Result<u16> {
    for p in preferred..(preferred + 50) {
        if tokio::net::TcpListener::bind(("0.0.0.0", p)).await.is_ok() {
            return Ok(p);
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AddrInUse,
        "no available http port",
    ))
}

#[tauri::command]
pub async fn dlna_start(app: AppHandle, port: Option<u16>) -> Result<DlnaStartResult, String> {
    eprintln!("[dlna_start] entered, port={:?}", port);
    let inner = state().clone();
    if inner.running.load(Ordering::SeqCst) {
        return Err("DLNA 服务端已在运行（之前一次启动流程尚未完成或已成功）".into());
    }

    eprintln!("[dlna_start] getting local IP...");
    let local_ip = match get_local_ip().await {
        Ok(ip) => {
            eprintln!("[dlna_start] local IP = {ip}");
            ip
        }
        Err(e) => {
            eprintln!("[dlna_start] get_local_ip failed: kind={:?}, detail={}", e.kind(), e);
            return Err(format!(
                "【步骤1/3 获取本机IP】失败 kind={:?} detail={} · 可能原因: ① 当前无默认路由到 8.8.8.8(网线/拔掉时) ② macOS 在某些网络配置下 UDP connect 失败 ③ 防火墙拦截(罕见)",
                e.kind(), e
            ));
        }
    };
    let uuid = format!("quickapp-desktop-{}", uuid_simple());
    // 统一设备名单源（与 get_dlna_name 对齐）：持久化优先，缺省按 local_ip 生成默认名。
    let name = dlna_name::read_stored_name().unwrap_or_else(|| dlna_name::default_name(&local_ip));
    // 写回单源，保证后续 get_dlna_name / dlna_start 读到一致的名字。
    *inner.friendly_name.lock().unwrap() = Some(name.clone());
    let desc = Arc::new(DeviceDesc::new(uuid.clone(), name));
    let av = Arc::new(AvTransport::new());
    // 把状态机存进全局，前端据此上报真实播放进度（GetPositionInfo 才能返回非零 RelTime）。
    *inner.av.lock().unwrap() = Some(av.clone());

    // —— HTTP server（服务设备描述 + 接收 SOAP 控制）——
    let preferred = port.unwrap_or(5001);
    eprintln!("[dlna_start] binding HTTP port near {preferred}...");
    let http_port = match bind_http_port(preferred).await {
        Ok(p) => {
            eprintln!("[dlna_start] HTTP port = {p}");
            p
        }
        Err(e) => {
            eprintln!(
                "[dlna_start] bind_http_port({preferred}..{}) failed: kind={:?}, detail={}",
                preferred + 49,
                e.kind(),
                e
            );
            return Err(format!(
                "【步骤2/3 HTTP端口】失败: 范围 {preferred}..{} 全部被占用或权限不足(kind={:?} detail={}) · 请查 `lsof -i :{}` 或换一个端口",
                preferred + 49,
                e.kind(),
                e,
                preferred
            ));
        }
    };
    let app_http = app.clone();
    let desc_http = desc.clone();
    let av_http = av.clone();
    let mut shutdown_http = inner.shutdown_tx.subscribe();
    tokio::spawn(async move {
        let _ = http_server::run_http(app_http, http_port, desc_http, av_http, &mut shutdown_http).await;
    });

    // —— SSDP（UDP 多播发现，替代 EndpointModule 的 UDP 监听）——
    eprintln!("[dlna_start] binding SSDP on 239.255.255.250:1900 (iface={local_ip})");
    let socket = match ssdp::bind_ssdp(&local_ip).await {
        Ok(s) => s,
        Err(e) => {
            // SSDP 起不来就关掉已起的 HTTP，避免半拉子状态
            let _ = inner.shutdown_tx.send(());
            eprintln!(
                "[dlna_start] ssdp_bind failed: kind={:?}, detail={}",
                e.kind(),
                e
            );
            let hint = port_holder_hint(1900, e.kind());
            return Err(format!(
                "【步骤3/3 SSDP多播】失败: kind={:?} detail={} · 可能原因: ① 1900 端口被占用(如已有 Plex/Jellyfin DLNA,或上次进程没死干净) ② macOS 多播权限未授权(系统设置→隐私与安全→本地网络) ③ 防火墙拦截 UDP 239.255.255.250:1900 ④ 本机 IP({local_ip}) 不在路由活跃接口上{hint}",
                e.kind(),
                e
            ));
        }
    };
    let mut shutdown_ssdp = inner.shutdown_tx.subscribe();
    let uuid_ssdp = uuid.clone();
    let local_ip_ssdp = local_ip.clone();
    let app_ssdp = app.clone();
    tokio::spawn(async move {
        ssdp::run_ssdp(
            app_ssdp,
            socket,
            &uuid_ssdp,
            &local_ip_ssdp,
            http_port,
            &mut shutdown_ssdp,
        )
        .await;
    });

    inner.running.store(true, Ordering::SeqCst);
    *inner.port.lock().unwrap() = Some(http_port);
    *inner.uuid.lock().unwrap() = uuid.clone();
    // 成功启动后把 DeviceDesc 存入 Inner，使 set_dlna_name 能热更新广播名。
    *inner.desc.lock().unwrap() = Some(desc.clone());
    Ok(DlnaStartResult {
        success: true,
        port: http_port,
        uuid,
    })
}

#[tauri::command]
pub async fn dlna_stop() -> Result<(), String> {
    let inner = state();
    if !inner.running.load(Ordering::SeqCst) {
        return Err("DLNA 服务端未运行".into());
    }
    // broadcast 一次唤醒所有订阅任务（HTTP + SSDP），不会漏唤醒
    let _ = inner.shutdown_tx.send(());
    inner.running.store(false, Ordering::SeqCst);
    // 停止后清空运行中的 DeviceDesc，避免残留引用。
    *inner.desc.lock().unwrap() = None;
    Ok(())
}

/// 前端 <video> 每秒上报的真实播放进度，写入 AvTransport，供 GetPositionInfo / GetTransportInfo 读取。
/// 这是 DLNA 客户端进度条能"跟随"桌面端播放的唯一数据来源（之前 GetPositionInfo 永远返回 0:00:00，
/// 导致客户端进度条不动、拖动后读回仍是 0，看起来"进度没更新"）。
#[tauri::command]
pub fn dlna_report_position(position: u64, duration: u64, playing: bool, paused: bool) {
    let inner = state();
    if let Some(av) = inner.av.lock().unwrap().as_ref() {
        av.update_position(position);
        if duration > 0 {
            av.update_duration(duration);
        }
        av.update_playback(playing, paused);
    }
}

/// 同步取本机局域网 IPv4（仅用于默认名兜底，失败回空串）。
fn get_local_ip_now() -> String {
    use std::net::UdpSocket;
    let sock = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(_) => return String::new(),
    };
    match sock.connect("8.8.8.8:80") {
        Ok(_) => sock.local_addr().map(|a| a.ip().to_string()).unwrap_or_default(),
        Err(_) => String::new(),
    }
}

/// 获取 DLNA 设备名。返回顺序：已确定 → 持久化 → 默认名。
/// 命名冻结为 ProcessBridgeModule.getDlnaName，与前端/web-runtime 调用一致。
#[tauri::command(rename = "ProcessBridgeModule.getDlnaName")]
pub fn get_dlna_name() -> String {
    let inner = state();
    if let Some(name) = inner.friendly_name.lock().unwrap().as_ref() {
        return name.clone();
    }
    if let Some(stored) = dlna_name::read_stored_name() {
        *inner.friendly_name.lock().unwrap() = Some(stored.clone());
        return stored;
    }
    // 尚未启动也没持久化：用同步取 IP 生成默认名
    let name = dlna_name::default_name(&get_local_ip_now());
    *inner.friendly_name.lock().unwrap() = Some(name.clone());
    name
}

/// 设置 DLNA 设备名并持久化；DLNA 已运行则热更新广播。
/// 命名冻结为 ProcessBridgeModule.setDlnaName，与前端/web-runtime 调用一致。
#[tauri::command(rename = "ProcessBridgeModule.setDlnaName")]
pub fn set_dlna_name(name: String) -> Result<(), String> {
    let trimmed = name.trim().to_string();
    if trimmed.is_empty() {
        return Err("设备名不能为空".into());
    }
    let inner = state();
    // 与 dlna_start 保持单源一致：先写内存储态，再持久化，最后热更新运行中的 DeviceDesc。
    *inner.friendly_name.lock().unwrap() = Some(trimmed.clone());
    dlna_name::write_stored_name(&trimmed);
    if let Some(desc) = inner.desc.lock().unwrap().as_ref() {
        desc.set_friendly_name(trimmed.clone());
    }
    Ok(())
}

fn uuid_simple() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}", n % 0xFFFF_FFFF_FFFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hint_returns_empty_for_non_address_errors() {
        // 不是 ADDRINUSE 时，hint 应该直接空，不去 spawn lsof
        let h = port_holder_hint(1900, std::io::ErrorKind::NotFound);
        assert!(h.is_empty());
    }

    #[test]
    fn hint_runs_for_addrinuse_without_panic() {
        // ADDRINUSE 触发 lsof / ss；本机没有 Plex 应该回空，或者把可用段补出来。
        // 关键：不 panic、不超时挂死。
        let h = port_holder_hint(1900, std::io::ErrorKind::AddrInUse);
        // 两种皆合法：" 【端口占用诊断】(无可用查询工具…)" 或 实际命中输出
        assert!(h.starts_with(" 【端口占用诊断】"));
    }
}
