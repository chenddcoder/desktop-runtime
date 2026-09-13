// 迷你 HTTP 服务端 —— 替代 Android EndpointModule 的本地 HTTP server
// 职责：① 提供 device-desc.xml / *-scpd.xml（GET）
//       ② 接收投屏控制 SOAP（POST /AVTransport/control 等），落地动作，触发播放事件
// 仅服务 DLNA 控制所需的少数固定路径，故手写 HTTP/1.1 解析，无需引入完整 HTTP 栈。

use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;

use tauri::{AppHandle, Emitter};

use crate::dlna::av_transport::AvTransport;
use crate::dlna::device_desc::DeviceDesc;
use crate::dlna::soap;
use crate::dlna::trace::dlog;

pub async fn run_http(
    app: AppHandle,
    port: u16,
    desc: Arc<DeviceDesc>,
    av: Arc<AvTransport>,
    control_port: Option<u16>,
    device_id: &str,
    service_id: &str,
    shutdown: &mut broadcast::Receiver<()>,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", port)).await?;
    loop {
        tokio::select! {
            _ = shutdown.recv() => break,
            res = listener.accept() => {
                match res {
                    Ok((stream, peer)) => {
                        let app = app.clone();
                        let desc = desc.clone();
                        let av = av.clone();
                        let device_id = device_id.to_string();
                        let service_id = service_id.to_string();
                        tokio::spawn(async move {
                            let _ = handle_conn(stream, app, desc, av, peer, control_port, &device_id, &service_id).await;
                        });
                    }
                    Err(_) => break,
                }
            }
        }
    }
    Ok(())
}

/// 查找请求头结束位置，返回 (header_end, 分隔符长度)：
/// header_end 是 \r\n\r\n（标准）或 \n\n（LF-only，部分 DLNA 客户端/乐播 SDK）
/// 的**起点**；body 起点 = header_end + 分隔符长度（\r\n\r\n=4，\n\n=2）。
/// 找不到返回 None。
fn find_header_end(buf: &[u8]) -> Option<(usize, usize)> {
    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
        return Some((i, 4));
    }
    if let Some(i) = buf.windows(2).position(|w| w == b"\n\n") {
        return Some((i, 2));
    }
    None
}

/// 判断 HTTP 方法 token 是否合法（用于校验请求行，跳过残留数据/解析错位）。
fn is_http_method(tok: &str) -> bool {
    matches!(
        tok,
        "GET" | "POST" | "HEAD" | "SUBSCRIBE" | "UNSUBSCRIBE" | "NOTIFY" | "OPTIONS" | "PUT" | "DELETE" | "M-SEARCH"
    )
}

async fn handle_conn(
    mut stream: TcpStream,
    app: AppHandle,
    desc: Arc<DeviceDesc>,
    av: Arc<AvTransport>,
    peer: std::net::SocketAddr,
    control_port: Option<u16>,
    device_id: &str,
    service_id: &str,
) -> std::io::Result<()> {
    // 跨请求读缓冲：支持 keep-alive（同一连接多个请求，Android OkHttp 连接池必需）。
    // 之前实现只 read 一次 + Connection: close：TCP 不保证一次 read 拿到完整请求，
    // 网络抖动时第一次 read 只有部分数据 → 头/body 解析失败 → 500 → 客户端该轮
    // 进度丢失（间歇性"有时候更新有时候不更新"）；且 close 破坏连接复用导致重试丢轮询。
    let mut read_buf: Vec<u8> = Vec::with_capacity(16384);
    let mut tmp = [0u8; 4096];

    loop {
        // ---- 1. 循环读 + 定位真正的请求行 ----
        // 校验头部首行是否为合法 HTTP 方法：若 read_buf 开头是残留数据
        // （上一次请求 body 未消费/头解析错位，如 LF-only 头 + content-length
        // 大小写不匹配），丢弃该段继续找，避免 method 被解析成 SOAP body 内容
        // （method=<s:Envelope → 投屏/进度全部失效）。
        let (header_end, sep_len) = loop {
            match find_header_end(&read_buf) {
                Some((i, sep)) => {
                    let head = String::from_utf8_lossy(&read_buf[..i]);
                    let first = head.lines().next().unwrap_or("");
                    let tok = first.split_whitespace().next().unwrap_or("");
                    if is_http_method(tok) {
                        break (i, sep);
                    }
                    // 残留/错位数据：丢弃到该段结束，继续找真正的请求行
                    dlog!("[dlna_http] skip stale head: tok={tok:?}");
                    read_buf.drain(..i + sep);
                }
                None => {
                    let n = stream.read(&mut tmp).await?;
                    if n == 0 {
                        return Ok(()); // 客户端关闭
                    }
                    read_buf.extend_from_slice(&tmp[..n]);
                    if read_buf.len() > 256 * 1024 {
                        return Ok(()); // 异常大请求，放弃（防恶意）
                    }
                }
            }
        };

        let head_str = String::from_utf8_lossy(&read_buf[..header_end]).into_owned();
        let first_line = head_str.lines().next().unwrap_or("").to_string();
        let parts: Vec<&str> = first_line.split_whitespace().collect();
        if parts.len() < 2 {
            return Ok(());
        }
        let method = parts[0].to_string();
        let path = parts[1].to_string();

        // 解析头：Content-Length / Connection（HTTP 头大小写不敏感）
        let mut content_length = 0usize;
        let mut conn_keep_alive = false;
        let mut has_conn_header = false;
        for line in head_str.lines().skip(1) {
            let t = line.trim();
            let lower = t.to_ascii_lowercase();
            if let Some(v) = lower.strip_prefix("content-length:") {
                content_length = v.trim().parse().unwrap_or(0);
            } else if lower.starts_with("connection:") {
                has_conn_header = true;
                let v = lower.strip_prefix("connection:").unwrap_or("").trim();
                // HTTP/1.1 默认 keep-alive；显式 close 才关闭
                conn_keep_alive = !v.contains("close");
            }
        }
        // 无 Connection 头：HTTP/1.1 默认 keep-alive
        if !has_conn_header {
            conn_keep_alive = true;
        }

        // ---- 2. 按 Content-Length 读完整 body ----
        let body_start = header_end + sep_len;
        while read_buf.len() < body_start + content_length {
            let n = stream.read(&mut tmp).await?;
            if n == 0 {
                break;
            }
            read_buf.extend_from_slice(&tmp[..n]);
        }
        let body_end = std::cmp::min(read_buf.len(), body_start + content_length);
        let body = String::from_utf8_lossy(&read_buf[body_start..body_end]).to_string();
        // 消费本请求数据
        read_buf.drain(..body_end);

        // ---- 3. 处理请求 ----
        if method == "GET" || method == "HEAD" {
            // 客户端来拉 device-desc.xml（或 SCPD）= 它已认可我们的 SSDP 响应、进入
            // "读设备能力"阶段。这一行是「搜不到设备」与「搜到但不投」的分界点，必须留痕：
            // 只看到 M-SEARCH 却没有 GET → 客户端拿到了 LOCATION 但连不上（防火墙/IP 错）。
            match desc.handle(&path) {
                Some((b, ct)) => {
                    // ⚠️ device-desc.xml 响应重复附加抖音播放列表扩展头（对齐 dlna_demo）：
                    // 无论手机在 SSDP 结果阶段还是读设备 XML 阶段做能力判断，都能看到一致数据。
                    // 抖音指纹 SERVER 只用于描述文档响应；scpd 等普通路径保持标准 DLNA SERVER。
                    // 列表功能关闭（control_port=None，默认）→ 标准指纹 + 无扩展头，纯公版。
                    let (server, extra) =
                        if path == "/device-desc.xml" && control_port.is_some() {
                            (
                                crate::dlna::playlist::wire::DOUYIN_SERVER,
                                crate::dlna::playlist::discovery_headers(
                                    control_port,
                                    device_id,
                                    service_id,
                                ),
                            )
                        } else {
                            (crate::dlna::playlist::wire::STD_SERVER, Vec::new())
                        };
                    write_response(&mut stream, 200, &ct, &b, conn_keep_alive, server, &extra).await?;
                    dlog!("[dlna_http] {method} {path} from={peer} -> 200 ({}B)", b.len());
                }
                None => {
                    write_response(&mut stream, 404, "text/plain", "Not found", conn_keep_alive, crate::dlna::playlist::wire::STD_SERVER, &[]).await?;
                    dlog!("[dlna_http] {method} {path} from={peer} -> 404 (unknown path)");
                }
            }
        } else if method == "POST" {
            match soap::parse_soap(&body) {
                Some(parsed) => {
                    // 请求日志：记录每个 SOAP 动作与来源客户端 IP。
                    // MetaData 字段可能携带巨大 XML（抖音投屏会带视频元数据），摘要时剔除防刷屏。
                    let mut brief: Vec<String> = Vec::new();
                    for (k, v) in &parsed.params {
                        if k.ends_with("MetaData") || k == "MetaData" {
                            continue;
                        }
                        brief.push(format!("{k}={}", if v.len() > 60 { &v[..60] } else { v }));
                    }
                    dlog!(
                        "[dlna_soap_req] action={} from={} params=[{}]",
                        parsed.action,
                        peer,
                        brief.join(" ")
                    );
                    let outcome = soap::handle_action(&parsed.action, &parsed.params, &av);
                    let resp_params = soap::response_params(&parsed.action, &av);
                    let xml =
                        soap::build_soap_response(&parsed.action, &parsed.service_type, &resp_params);
                    // 响应体摘要（截断防刷屏）：用于核对客户端实际收到的 XML 结构是否合规。
                    let flat: String = xml.chars().filter(|c| !c.is_whitespace()).take(160).collect();
                    dlog!("[dlna_soap_resp] body={flat}...");
                    match outcome {
                        soap::ActionOutcome::Play(url) => {
                            // 通知前端：有人把视频/图片投到这台桌面电脑了。
                            // mediaType 由 SetAVTransportURI 的 DIDL/URI 判定
                            // （见 media_kind）：image 时前端走图片层，video/audio 走播放器。
                            let kind = av.media_kind();
                            let _ = app.emit(
                                "dlna://play",
                                serde_json::json!({ "url": url, "mediaType": kind.as_str() }),
                            );
                            dlog!("[dlna_emit] dlna://play mediaType={} url={url}", kind.as_str());
                        }
                        soap::ActionOutcome::Seek(secs) => {
                            // 手机端拖动进度 → 前端 <video> 跟随跳转
                            let _ = app.emit("dlna://seek", serde_json::json!({ "position": secs }));
                            dlog!("[dlna_emit] dlna://seek position={secs}");
                        }
                        soap::ActionOutcome::Stop => {
                            // 手机端停止投屏 → 前端收起全屏播放层
                            let _ = app.emit("dlna://stop", serde_json::json!({}));
                            dlog!("[dlna_emit] dlna://stop");
                        }
                        soap::ActionOutcome::Pause => {
                            let _ = app.emit("dlna://pause", serde_json::json!({}));
                            dlog!("[dlna_emit] dlna://pause");
                        }
                        _ => {}
                    }
                    write_response(&mut stream, 200, "text/xml; charset=\"utf-8\"", &xml, conn_keep_alive, crate::dlna::playlist::wire::STD_SERVER, &[])
                        .await?;
                }
                None => {
                    // SOAP 解析失败：把原始报文前 200 字符打出来 —— 客户端用了非标
                    // 命名空间 / 分块编码 / 首行异常时会走到这里，没有报文就没法定位。
                    let raw: String = body.chars().take(200).collect();
                    dlog!("[dlna_soap_req] parse FAILED from={peer} raw={raw:?}");
                    let err = soap::build_soap_error(401, "Invalid SOAP request");
                    write_response(&mut stream, 500, "text/xml; charset=\"utf-8\"", &err, conn_keep_alive, crate::dlna::playlist::wire::STD_SERVER, &[])
                        .await?;
                }
            }
        } else {
            // 非 GET/POST 方法（SUBSCRIBE/UNSUBSCRIBE/NOTIFY/M-SEARCH 等）打日志：
            // 用于确认客户端是否在做 UPnP 事件订阅（GENA LastChange）——部分 DLNA 客户端
            // （抖音/乐播等）靠事件驱动进度条，轮询仅作校验；我们目前不实现订阅（返回 501），
            // 若日志出现 SUBSCRIBE 即证明客户端在等事件。
            let brief: String = head_str
                .lines()
                .take(8)
                .map(|l| l.trim().to_string())
                .collect::<Vec<_>>()
                .join(" | ");
            dlog!("[dlna_http_req] method={method} path={path} from={peer} head=[{brief}]");
            write_response(&mut stream, 501, "text/plain", "Not implemented", conn_keep_alive, crate::dlna::playlist::wire::STD_SERVER, &[])
                .await?;
        }

        // ---- 4. keep-alive：继续处理同一连接的下一个请求；close 则结束 ----
        if !conn_keep_alive {
            return Ok(());
        }
    }
}

async fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &str,
    keep_alive: bool,
    server: &str,
    extra_headers: &[(String, String)],
) -> std::io::Result<()> {
    let status_text = match status {
        200 => "OK",
        404 => "Not Found",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        _ => "OK",
    };
    // UPnP Device Architecture 强制：所有 HTTP 响应必须携带值为空的 EXT: 头。
    // 缺失时部分严格客户端（iOS / 抖音等）会丢弃 SOAP 响应体 → 投屏乐观成功（客户端
    // 不看响应）但进度条/状态永远不更新（客户端轮询响应被丢弃）。
    // Server 头按 UPnP 规范格式：OS/version UPnP/1.1 product/version（部分客户端校验格式）。
    // Date 头为 HTTP/1.1 规范强制（RFC 7231），Apple 严格客户端缺 Date 会拒绝解析响应体。
    // Connection 头跟随客户端：keep-alive 复用连接（Android OkHttp 连接池），close 关闭。
    let date = chrono::Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string();
    let conn = if keep_alive { "keep-alive" } else { "close" };
    let extra: String = extra_headers
        .iter()
        .map(|(k, v)| format!("{k}: {v}\r\n"))
        .collect();
    let header = format!(
        "HTTP/1.1 {status} {text}\r\nContent-Type: {ct}\r\nContent-Length: {len}\r\nDate: {date}\r\nEXT:\r\nServer: {server}\r\n{extra}Connection: {conn}\r\n\r\n",
        status = status,
        text = status_text,
        ct = content_type,
        len = body.len(),
        server = server,
        extra = extra
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_header_end_detects_crlf() {
        // 标准 \r\n\r\n：返回 (起点, 4)
        let buf = b"POST /x HTTP/1.1\r\nHost: a\r\n\r\nBODY";
        let r = find_header_end(buf);
        assert_eq!(r, Some((25, 4))); // \r\n\r\n 起点在 25
    }

    #[test]
    fn find_header_end_detects_lf_only() {
        // LF-only 头（部分 DLNA 客户端）：\n\n，返回 (起点, 2)
        let buf = b"POST /x HTTP/1.1\nHost: a\n\nBODY";
        let r = find_header_end(buf);
        assert_eq!(r, Some((24, 2))); // \n\n 起点在 24
    }

    #[test]
    fn find_header_end_prefers_crlf_over_lf() {
        // 同时存在时 \r\n\r\n 优先匹配（\n\n 在 22，\r\n\r\n 在 24）——
        // 若因此定位到 body 内的分隔符，由 handle_conn 的 is_http_method
        // 请求行校验跳过残留段自愈。
        let buf = b"POST /x HTTP/1.1\nH: a\n\nX\r\n\r\nBODY";
        let r = find_header_end(buf);
        assert_eq!(r, Some((24, 4)));
    }

    #[test]
    fn http_method_tokens_validated() {
        assert!(is_http_method("POST"));
        assert!(is_http_method("GET"));
        assert!(is_http_method("SUBSCRIBE"));
        assert!(is_http_method("M-SEARCH"));
        assert!(!is_http_method("<s:Envelope"));
        assert!(!is_http_method("xmlns:s="));
        assert!(!is_http_method(""));
    }
}
