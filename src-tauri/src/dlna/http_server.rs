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

pub async fn run_http(
    app: AppHandle,
    port: u16,
    desc: Arc<DeviceDesc>,
    av: Arc<AvTransport>,
    shutdown: &mut broadcast::Receiver<()>,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", port)).await?;
    loop {
        tokio::select! {
            _ = shutdown.recv() => break,
            res = listener.accept() => {
                match res {
                    Ok((stream, _)) => {
                        let app = app.clone();
                        let desc = desc.clone();
                        let av = av.clone();
                        tokio::spawn(async move {
                            let _ = handle_conn(stream, app, desc, av).await;
                        });
                    }
                    Err(_) => break,
                }
            }
        }
    }
    Ok(())
}

async fn handle_conn(
    mut stream: TcpStream,
    app: AppHandle,
    desc: Arc<DeviceDesc>,
    av: Arc<AvTransport>,
) -> std::io::Result<()> {
    let mut buf = [0u8; 8192];
    let n = stream.read(&mut buf).await?;
    if n == 0 {
        return Ok(());
    }
    let req = String::from_utf8_lossy(&buf[..n]);

    let first_line = match req.lines().next() {
        Some(l) => l,
        None => return Ok(()),
    };
    let parts: Vec<&str> = first_line.split_whitespace().collect();
    if parts.len() < 2 {
        return Ok(());
    }
    let method = parts[0];
    let path = parts[1];

    // 解析 Content-Length（POST 需要）
    let mut content_length = 0usize;
    for line in req.lines() {
        if let Some(v) = line.trim().strip_prefix("Content-Length:") {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }

    // 提取 body（\r\n\r\n 之后），不足则继续读
    let body = match req.find("\r\n\r\n") {
        Some(i) => {
            let start = i + 4;
            let mut body = req[start..].to_string();
            while body.len() < content_length {
                let mut chunk = [0u8; 4096];
                let cn = stream.read(&mut chunk).await?;
                if cn == 0 {
                    break;
                }
                body.push_str(&String::from_utf8_lossy(&chunk[..cn]));
            }
            body
        }
        None => String::new(),
    };

    if method == "GET" || method == "HEAD" {
        match desc.handle(path) {
            Some((b, ct)) => write_response(&mut stream, 200, &ct, &b).await?,
            None => write_response(&mut stream, 404, "text/plain", "Not found").await?,
        }
        return Ok(());
    }

    if method == "POST" {
        match soap::parse_soap(&body) {
            Some(parsed) => {
                let outcome = soap::handle_action(&parsed.action, &parsed.params, &av);
                let resp_params = soap::response_params(&parsed.action, &av);
                let xml = soap::build_soap_response(&parsed.action, &parsed.service_type, &resp_params);
                match outcome {
                    soap::ActionOutcome::Play(url) => {
                        // 通知前端：有人把视频投到这台桌面电脑了
                        let _ = app.emit("dlna://play", serde_json::json!({ "url": url }));
                    }
                    soap::ActionOutcome::Seek(secs) => {
                        // 手机端拖动进度 → 前端 <video> 跟随跳转
                        let _ = app.emit("dlna://seek", serde_json::json!({ "position": secs }));
                    }
                    soap::ActionOutcome::Stop => {
                        // 手机端停止投屏 → 前端收起全屏播放层
                        let _ = app.emit("dlna://stop", serde_json::json!({}));
                    }
                    soap::ActionOutcome::Pause => {
                        let _ = app.emit("dlna://pause", serde_json::json!({}));
                    }
                    _ => {}
                }
                write_response(&mut stream, 200, "text/xml; charset=\"utf-8\"", &xml).await?;
            }
            None => {
                let err = soap::build_soap_error(401, "Invalid SOAP request");
                write_response(&mut stream, 500, "text/xml; charset=\"utf-8\"", &err).await?;
            }
        }
        return Ok(());
    }

    write_response(&mut stream, 501, "text/plain", "Not implemented").await?;
    Ok(())
}

async fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &str,
) -> std::io::Result<()> {
    let status_text = match status {
        200 => "OK",
        404 => "Not Found",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        _ => "OK",
    };
    let header = format!(
        "HTTP/1.1 {status} {text}\r\nContent-Type: {ct}\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n",
        status = status,
        text = status_text,
        ct = content_type,
        len = body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}
