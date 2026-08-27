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
pub mod playlist;
pub mod soap;
pub mod ssdp;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use tauri::{AppHandle, Emitter, Manager};
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
    // 抖音播放列表 TCP 通道（dlna_start 内先于 SSDP/HTTP 启动，控制端口写入发现头）。
    playlist: Mutex<Option<Arc<playlist::PlaylistChannel>>>,
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
            playlist: Mutex::new(None),
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

/// 枚举本机所有活跃的非回环 IPv4 接口地址（解析 `ifconfig` 输出，零依赖）。
///
/// 过滤虚拟/链路接口（lo/utun/awdl/llw/bridge/gif/stf/ipsec/tun/tap/vmenet/vmnet/
/// anpi/pdp_ip 及 link-local 169.254），真实接口（en*/eth* 等）排前。
/// 多网卡环境（WiFi + USB 网卡/虚拟机网卡）下**不能**用"连 8.8.8.8 取默认出口 IP"——
/// 默认出口可能是非 WiFi 接口（实测 40.64 WiFi + 55.29 第二接口时出口 IP=55.29），
/// SSDP 需 join **全部**接口的多播组，LOCATION 用第一个真实接口 IP。
pub(crate) fn list_local_ipv4() -> Vec<String> {
    use std::process::Command;
    let out = match Command::new("ifconfig").output() {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        _ => return Vec::new(),
    };
    let skip_iface = |name: &str| -> bool {
        [
            "lo", "utun", "awdl", "llw", "bridge", "gif", "stf", "ipsec", "tun", "tap",
            "vmenet", "vmnet", "anpi", "pdp_ip",
        ]
        .iter()
        .any(|k| name.starts_with(k))
    };
    let mut pairs: Vec<(String, String)> = Vec::new(); // (iface, ip)
    let mut cur: Option<String> = None;
    for line in out.lines() {
        let t = line.trim_start();
        if t.contains(": flags=") {
            cur = t.split(':').next().map(|s| s.to_string());
        } else if let Some(rest) = t.strip_prefix("inet ") {
            let ip = rest.split_whitespace().next().unwrap_or("");
            if !ip.is_empty() && !ip.starts_with("127.") && !ip.starts_with("169.254.") {
                if let Some(name) = &cur {
                    if !skip_iface(name) && !pairs.iter().any(|(_, p)| p == ip) {
                        pairs.push((name.clone(), ip.to_string()));
                    }
                }
            }
        }
    }
    // 排序：en*（WiFi/以太网）优先，其余按名；长度优先避免 en10 < en2 字典序问题
    pairs.sort_by(|a, b| {
        let rank = |n: &str| if n.starts_with("en") { 0 } else { 1 };
        rank(&a.0)
            .cmp(&rank(&b.0))
            .then_with(|| a.0.len().cmp(&b.0.len()))
            .then_with(|| a.0.cmp(&b.0))
    });
    pairs.into_iter().map(|(_, ip)| ip).collect()
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
    let ifaces = list_local_ipv4();
    let local_ip = match ifaces.first() {
        Some(ip) => {
            eprintln!("[dlna_start] local IP = {ip} (ifaces={ifaces:?})");
            ip.clone()
        }
        None => {
            eprintln!("[dlna_start] list_local_ipv4 empty (no active non-loopback IPv4)");
            return Err(
                "【步骤1/3 获取本机IP】失败: 未发现活跃的非回环 IPv4 接口（ifconfig 无结果？网卡未连网络？）"
                    .into(),
            );
        }
    };
    let uuid = format!("quickapp-desktop-{}", uuid_simple());
    // 统一设备名单源（与 get_dlna_name 对齐）：持久化优先，缺省按 local_ip 生成默认名。
    let name = dlna_name::read_stored_name().unwrap_or_else(|| dlna_name::default_name(&local_ip));
    // 写回单源，保证后续 get_dlna_name / dlna_start 读到一致的名字。
    *inner.friendly_name.lock().unwrap() = Some(name.clone());
    let desc = Arc::new(DeviceDesc::new(uuid.clone(), name.clone()));
    let av = Arc::new(AvTransport::new());
    // 把状态机存进全局，前端据此上报真实播放进度（GetPositionInfo 才能返回非零 RelTime）。
    *inner.av.lock().unwrap() = Some(av.clone());

    // —— 抖音播放列表 TCP 通道（**列表功能总开关控制**，2026-08-25 陈兄要求）——
    // 手机（抖音）发现本设备后连上 control_port：GetDeviceInfo 握手 → Play/AddDramaList
    // 下发剧集列表 → 本地按 dramaId 合并维护。列表当前项变化时回调 emit dlna://play，
    // 前端（esapp-tvcast casting 页）收到新 url 换源播放；播完自动切集见 auto_next。
    // ⚠️ 抖音极速版对 PushMediaInfo 存在 `must not be null` 闪退风险（崩溃堆栈已实锤，
    // 未根治前**默认关闭**）。开启：环境变量 DOUYIN_PLAYLIST=1，或 dlna-config.json
    // 写 {"playlistEnabled": true}（与 dlna-device-id.json 同目录）。关闭时：
    // ① 不监听 45165；② control_port=None → SSDP/HTTP 发现层回标准指纹、无扩展头，
    //    抖音投屏退回普通 DLNA 单集链路（短视频 5s 伪装等非列表行为不受影响）。
    let playlist_enabled = playlist::playlist_enabled();
    eprintln!("[dlna_start] playlist feature enabled={playlist_enabled}");
    let service_id = format!("{}-{}", uuid_simple(), random_digits(4));
    let device_id = playlist::device_id();
    let control_port: Option<u16> = if playlist_enabled {
        let playlist_device = playlist::PlaylistDeviceIdentity {
            ip: local_ip.clone(),
            name: name.clone(),
            package_name: "cn.chenddcoder.tvcast".to_string(),
            device_id: device_id.clone(),
            os_version: std::env::consts::OS.to_string(),
            device_model: std::env::consts::ARCH.to_string(),
            device_brand: "desktop".to_string(),
        };
        let app_pl = app.clone();
        let pl_channel = match playlist::PlaylistChannel::start(
            playlist_device,
            inner.shutdown_tx.subscribe(),
            Box::new(move |item| {
                // 列表当前项变化（手机 Play/AddDramaList/选集命令）→ 通知前端换源播放
                eprintln!(
                    "[dlna_playlist] play request → url={} title={}",
                    item.url,
                    item.title
                );
                // addOn 激活判据：**按投屏 URL 判断是否抖音源**（9 域名 + ott_cast）。
                // 非抖音 URL 即使走了列表通道也不激活列表行为（playlist_mode=false、
                // 不 set_uri/play）——公版投屏链路零污染。抖音 URL 才置列表模式：
                // GetPositionInfo 转为如实报告（禁用 fake_short / force_complete 伪装），
                // 否则客户端自己也判"播完"发起第二路切集竞态。
                if let Some(av) = state().av.lock().unwrap().as_ref() {
                    let douyin = crate::dlna::playlist::is_douyin_url(&item.url);
                    av.set_playlist_mode(douyin);
                    if douyin {
                        // 关键修复（2026-08-25 实测）：列表通道模式下抖音（极速版）靠
                        // Play/AddDramaList/选集命令起播/换集，**不发送 SetAVTransportURI**
                        //（日志全程无 SetAVTransportURI）。若此处不 set_uri，GetPositionInfo
                        // 的 TrackURI 恒为空（实测 TrackURI="" + force_complete=true 残留），
                        // 抖音端轮询判定设备异常，按下切集时 TrackURI 突变直接退出投屏
                        //（实测 client disconnected）。set_uri 让 TrackURI/TrackMetaData 始终
                        // 跟随当前列表项，同时清 force_complete/pos/dur（起播后前端重新上报）。
                        av.set_uri(&item.url, "");
                        av.play();
                    }
                }
                let _ = app_pl.emit(
                    "dlna://play",
                    serde_json::json!({ "url": item.url, "title": item.title }),
                );
            }),
        )
        .await
        {
            Ok(ch) => ch,
            Err(e) => {
                eprintln!("[dlna_start] playlist channel failed: {e}");
                return Err(format!(
                    "【步骤2/4 播放列表通道】失败: {e} · 45165/临时端口无法绑定"
                ));
            }
        };
        let cp = pl_channel.control_port();
        *inner.playlist.lock().unwrap() = Some(pl_channel.clone());
        eprintln!("[dlna_start] playlist channel port={cp}");
        Some(cp)
    } else {
        eprintln!(
            "[dlna_start] playlist feature DISABLED: BDLE channel skipped (开启: DOUYIN_PLAYLIST=1 或 dlna-config.json playlistEnabled=true)"
        );
        None
    };

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
                "【步骤3/4 HTTP端口】失败: 范围 {preferred}..{} 全部被占用或权限不足(kind={:?} detail={}) · 请查 `lsof -i :{}` 或换一个端口",
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
    let control_http = control_port;
    let device_id_http = device_id.clone();
    let service_id_http = service_id.clone();
    let mut shutdown_http = inner.shutdown_tx.subscribe();
    tokio::spawn(async move {
        let _ = http_server::run_http(
            app_http,
            http_port,
            desc_http,
            av_http,
            control_http,
            &device_id_http,
            &service_id_http,
            &mut shutdown_http,
        )
        .await;
    });

    // —— SSDP（UDP 多播发现，替代 EndpointModule 的 UDP 监听）——
    eprintln!("[dlna_start] binding SSDP on 239.255.255.250:1900 (ifaces={ifaces:?})");
    let socket = match ssdp::bind_ssdp(&ifaces).await {
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
                "【步骤4/4 SSDP多播】失败: kind={:?} detail={} · 可能原因: ① 1900 端口被占用(如已有 Plex/Jellyfin DLNA,或上次进程没死干净) ② macOS 多播权限未授权(系统设置→隐私与安全→本地网络) ③ 防火墙拦截 UDP 239.255.255.250:1900 ④ 本机 IP({local_ip}) 不在路由活跃接口上{hint}",
                e.kind(),
                e
            ));
        }
    };
    let mut shutdown_ssdp = inner.shutdown_tx.subscribe();
    let uuid_ssdp = uuid.clone();
    let local_ip_ssdp = local_ip.clone();
    let app_ssdp = app.clone();
    let control_ssdp = control_port;
    let device_id_ssdp = device_id.clone();
    let service_id_ssdp = service_id.clone();
    // WiFi 切换时 SSDP 重绑定成功 → 回调同步 playlist 通道设备 IP（GetDeviceInfo/PushMediaInfo 上报新 IP）；
    // 列表功能关闭时 inner.playlist 为 None，回调跳过（无通道可同步）。
    let pl_ssdp = inner.playlist.lock().unwrap().clone();
    tokio::spawn(async move {
        ssdp::run_ssdp(
            app_ssdp,
            socket,
            &uuid_ssdp,
            local_ip_ssdp,
            http_port,
            control_ssdp,
            &device_id_ssdp,
            &service_id_ssdp,
            Box::new(move |new_ip: &str| {
                eprintln!("[dlna_start] SSDP rebound, sync playlist device ip={new_ip}");
                if let Some(pl) = &pl_ssdp {
                    pl.set_ip(new_ip.to_string());
                }
            }),
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
    // 停播放列表通道（关监听 + 清列表）
    if let Some(pl) = inner.playlist.lock().unwrap().take() {
        pl.stop();
    }
    // 复位列表模式标志：下次非列表投屏（普通 DLNA 客户端）恢复 fake_short/force_complete 链路
    if let Some(av) = state().av.lock().unwrap().as_ref() {
        av.set_playlist_mode(false);
    }
    // broadcast 一次唤醒所有订阅任务（HTTP + SSDP + playlist），不会漏唤醒
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
    eprintln!(
        "[dlna_report_position] position={position}ms duration={duration}ms playing={playing} paused={paused}"
    );
    let inner = state();
    if let Some(av) = inner.av.lock().unwrap().as_ref() {
        av.update_position(position);
        if duration > 0 {
            av.update_duration(duration);
        }
        av.update_playback(playing, paused);
    }
}

/// 快应用 ESPlayerManager 播放状态上报（与 Android 端 EsNativeModule.sendRemoteEvent 语义对齐）。
/// 事件名/载荷由快应用侧（esapp-tvcast dlna-bridge）按 xiaoyoucast 契约发送：
///   play / pause / stop → 更新 AvTransport 播放状态（客户端 GetTransportInfo 轮询感知）
///   position {position: ms} → 更新真实进度（GetPositionInfo RelTime）；**列表模式播完自动切下一集**
///   duration {duration: ms} → 更新总时长（GetPositionInfo TrackDuration）
///   next → 下键/手动切集：列表模式本地切下一集，非列表模式 force_complete 骗客户端切
/// 注意：快应用侧 position/duration 均为**毫秒**，AvTransport 内部也按**毫秒**存储
/// （RelTime 输出带毫秒小数，避免 <1s 进度被截断成 0 导致客户端误判未播放）。
#[tauri::command]
pub async fn dlna_send_remote_event(
    app: tauri::AppHandle,
    event_name: String,
    event_data: Option<serde_json::Value>,
) -> Result<(), String> {
    let inner = state();
    let data = event_data.unwrap_or(serde_json::Value::Null);
    eprintln!("[dlna_send_remote_event] {event_name} data={data}");
    let av = match inner.av.lock().unwrap().as_ref() {
        Some(av) => av.clone(),
        None => {
            // DLNA 未启动时忽略（快应用可能先于 dlna_start 完成上报）。
            return Err("DLNA 服务未运行".into());
        }
    };
    let num_field = |key: &str| -> Option<u64> {
        // position/duration 直接存毫秒（AvTransport 内部即毫秒存储）：
        // 之前 ms/1000 整数除法会把 <1s 的上报截断成 0 → RelTime 恒为
        // 00:00:00 → 客户端（Android 抖音）判定"未播放"、进度条不更新。
        data.get(key).and_then(|v| v.as_u64())
    };
    // 列表模式进度同步（GetStatusInfo 内部字段维护；切集见 "next" 分支注释）
    let playlist_ref = inner.playlist.lock().unwrap().as_ref().cloned();
    match event_name.as_str() {
        "play" => av.update_playback(true, false),
        "pause" => av.update_playback(false, true),
        "stop" => {
            // 列表模式：STOP 只是"本集结束"的兜底信号（前端 onPlayerCompleted 1.2s 后发），
            // **不切集** —— 播完切集由 position 上报统一触发，避免换集流程中
            // （客户端 Stop→SetAVTransportURI→Play）误判连环切集。
            av.update_playback(false, false);
        }
        "position" => {
            if let Some(ms) = num_field("position") {
                av.update_position(ms);
                // 同步进度到播放列表状态（供内部维护；GetStatusInfo 仍回 demo 形态 0/0）
                if let Some(pl) = &playlist_ref {
                    pl.update_progress(av.duration(), ms);
                    let dur = av.duration();
                    // 播完自动切集（列表模式）：前端播完兜底连报 pos>=dur → 本地
                    // move(1) → emit dlna://play(下一集) + push。⚠️ 抖音极速版
                    // **列表模式不响应 SOAP 播完信号**（实测），只能服务端切；
                    // push 安全性由 normalize_bean 保证（防抖音端 checkNotNull 崩溃，
                    // 2026-08-25 堆栈实锤——demo 原样回传同样闪退）。
                    if pl.has_playlist() && dur > 0 && ms >= dur {
                        auto_next(&app, &av, pl, false).await;
                    }
                }
            }
        }
        "duration" => {
            if let Some(ms) = num_field("duration") {
                av.update_duration(ms);
                if let Some(pl) = &playlist_ref {
                    pl.update_progress(ms, av.position());
                }
            }
        }
        "next" => {
            // TV 下键/手动切集：
            //  - 列表模式 → 服务端本地切下一集（auto_next：move + set_uri + emit +
            //    push）。⚠️ 抖音极速版**列表模式不响应 SOAP 播完信号**（实测按
            //    force_complete 后它不动作，只会干等），切集只能服务端发起；push
            //    安全性由 normalize_bean 保证（dramaBeans 必填字段补齐，防抖音端
            //    checkNotNull 崩溃——2026-08-25 堆栈实锤，demo 原样回传同样闪退）。
            //  - 非列表模式 → force_complete 伪装播完，客户端（抖音）轮询后 SetAVTransportURI 换集
            if let Some(pl) = &playlist_ref {
                if pl.has_playlist() {
                    if !auto_next(&app, &av, pl, true).await {
                        // 已是最后一集：无下一项，退化为 force_complete（客户端自行处理）
                        av.set_force_complete();
                    }
                } else {
                    av.set_force_complete();
                }
            } else {
                av.set_force_complete();
            }
        }
        // 快应用 DLNA 就绪：通知 webview 侧（dlna_overlay.js）补发缓存的投屏请求。
        // 不依赖 window 全局信号，走 Rust → overlay 事件（对齐真机原生广播语义）。
        "tvcast_ready" => {
            let _ = app.emit("dlna://app-ready", serde_json::json!({}));
        }
        // heartbeat_response / tv_cmd / addDeviceEvent / sendInfoToAndroidCastEvent 等
        // 在桌面 DLNA 场景无对应能力，仅记录日志，不影响状态机。
        _ => {}
    }
    Ok(())
}

/// 列表模式切集：playlist.move(1) 拿到下一项 → 更新 AvTransport（换 uri/置 Playing）
/// → emit dlna://play 通知前端换源 → PushMediaInfo 同步手机端列表 UI。
/// ⚠️ 抖音极速版**列表模式不响应 SOAP 播完信号**（force_complete 按下后它不动作），
/// 切集只能服务端发起；PushMediaInfo 的安全性由 normalize_bean 保证（dramaBeans
/// 必填字段补齐，防抖音端 excuteBdleMessage checkNotNull 崩溃——2026-08-25 堆栈
/// 实锤，demo 原样回传同样闪退）。
/// `force=true`：手动切集（TV 遥控 next），跳过 1s 防抖；`false`：播完自动切，
/// 保留防抖去重（前端 position 连报 dur+1000 只切一次）。
/// 返回是否成功切到下一项（末尾无下一项返回 false）。
async fn auto_next(
    app: &AppHandle,
    av: &Arc<AvTransport>,
    pl: &Arc<playlist::PlaylistChannel>,
    force: bool,
) -> bool {
    // notify=false：只切集不触发 on_play_request 回调——addOn URL 守卫必须在
    // set_uri/emit **之前**执行（回调会先 emit，守卫就形同虚设），且避免双 emit。
    match pl.next_and_get(false, force) {
        Some(item) => {
            // addOn 守卫：仅抖音源 URL 才本地切集（列表混入非抖音 URL 时拒绝）
            if !crate::dlna::playlist::is_douyin_url(&item.url) {
                return false;
            }
            // 换源：清 force_complete / 进度 / 时长，置 Playing（GetPositionInfo 回新集状态）
            av.set_uri(&item.url, "");
            av.play();
            eprintln!("[dlna_playlist] auto-next → dramaId={} url={}", item.drama_id, item.url);
            // 通知前端换源播放（esapp-tvcast casting onDlnaPlay → initPlay 换源）
            let _ = app.emit(
                "dlna://play",
                serde_json::json!({ "url": item.url, "title": item.title }),
            );
            // 手机端列表/进度 UI 跟随（normalize_bean 已保证条目结构完整，不触发崩溃）
            pl.push_media_info().await;
            true
        }
        None => false,
    }
}

/// 生成随机数字后缀（service_id / message_id 用）。
fn random_digits(len: usize) -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| char::from(b'0' + rng.gen_range(0..10)))
        .collect()
}

/// 诊断上报：dlna_overlay.js 把 webview 侧链路状态打回 Rust 终端（eprintln），
/// 用于在看不到 webview 控制台时确认真实环境里投屏事件是否到达快应用。
/// 同时追加写入日志文件（app_log_dir/desktop-runtime.log），方便 release 包
/// （无控制台）把 webview 错误详情取回排查。
#[tauri::command]
pub fn dlna_debug_log(
    app: tauri::AppHandle,
    msg: String,
    data: Option<serde_json::Value>,
) -> Result<(), String> {
    let data = data.unwrap_or(serde_json::Value::Null);
    eprintln!("[dlna_debug_log] {msg} data={data}");
    // 同时写多个候选目录（macOS release 为沙盒应用，dirs 解析到 container 内路径，
    // 与 dev 的非沙盒路径不同；写多处保证至少一处成功）：
    //   - app_log_dir:  ~/Library/Logs/<id>            （dev 无沙盒时）
    //   - app_data_dir: ~/Library/Application Support/<id>（dev）/
    //                   ~/Library/Containers/<id>/Data/Library/Application Support/<id>（release 沙盒）
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(dir) = app.path().app_log_dir() {
        candidates.push(dir);
    }
    if let Ok(dir) = app.path().app_data_dir() {
        candidates.push(dir);
    }
    for dir in candidates {
        if std::fs::create_dir_all(&dir).is_err() {
            continue;
        }
        let file = dir.join("desktop-runtime.log");
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(file) {
            use std::io::Write;
            let _ = writeln!(f, "[{}] {msg} data={data}", now_log_ts());
        }
    }
    Ok(())
}

fn now_log_ts() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().to_string())
        .unwrap_or_default()
}

/// 同步取本机局域网 IPv4（仅用于默认名兜底，失败回空串）。
fn get_local_ip_now() -> String {
    list_local_ipv4().into_iter().next().unwrap_or_default()
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
