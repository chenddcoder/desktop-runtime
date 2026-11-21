//! 桌面端「自起 HTTP 服务」—— 补上 Android 插件 `com.quicktvui.plugin.endpoint` 的能力。
//!
//! 为什么需要它：留言板这类「大屏接手机」的快应用，在 Android 上靠插件起局域网服务，
//! 手机扫码后直连电视（host 模式）。桌面端（WKWebView/WebView2）没有这个原生插件，
//! 而不改 line 的话 `host.ts` 会把 web 一律判成 relay（轮询中继）——
//! 后果是**没有二维码、左下角永远显示「离线」**，桌面端只能当一面不会更新的墙。
//!
//! 这里用 tokio 手写一个够用的 HTTP 端，语义与插件**逐字对齐**：
//!
//!   - `initHttp([port])`            → `{ success, httpPort, ip, deviceUuid }`（端口会顺延）
//!   - 收到请求                      → 前端 `EventBus.$emit('HttpRequest', {...})`
//!   - `sendHttpResponse([...])`     → 写回响应（插件只等 5s）
//!
//! ## ⛔ 与插件对齐的三条硬约束（照抄 Android 侧踩过的坑）
//!
//! 1. **请求要原样交给 JS 算**，Rust 不参与业务。路由/状态/历史全在 `messageboard-tv/src/server/`，
//!    这里重写一遍必然与电视端漂移（同一份 `.mbd` 两种口径是最难查的那类 bug）。
//! 2. **path 必须带 query**。插件 v1.0.1 起会把 query 拼回 path，手机端的 `?since=` /
//!    `?room=` 全靠它；只传 pathname 会让增量拉取退化成全量或直接 400。
//! 3. **回包超时按 5s**（`HttpServer.REQUEST_TIMEOUT_MS`）。超时必须显式回 504 并摘掉 pending，
//!    否则请求号会一直挂着、下一次同号请求会串包。
//!
//! ## 端口
//! 首选 39200（与手机端 `core/pair.js` 的 `TV_PORT` 一致 —— 扫码后手机不会补端口），
//! 被占就顺延 40 个。**二维码里的地址要用真实端口**，不能假设就是 39200。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::Serialize;
use tauri::Emitter;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

/// 单请求体上限：便签 `.mbd` 只有几十 KB，8MB 是给「发文件」留的余量
const MAX_BODY: usize = 8 * 1024 * 1024;
/// 与 Android 插件 `HttpServer.REQUEST_TIMEOUT_MS` 一致
const REPLY_TIMEOUT_MS: u64 = 5000;
/// 与手机端 `core/pair.js` 的 `TV_PORT` 一致
const PREFERRED_PORT: u16 = 39200;
/// 服务端收请求的超时（手机半包/断线不能把连接挂死）
const REQUEST_TIMEOUT_SEC: u64 = 30;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointInfo {
    pub success: bool,
    pub http_port: u16,
    pub ip: String,
    pub device_uuid: String,
}

struct Instance {
    shutdown: Arc<AtomicBool>,
    info: EndpointInfo,
}

fn state() -> &'static Mutex<Option<Instance>> {
    static S: OnceLock<Mutex<Option<Instance>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}

struct Reply {
    status: u16,
    body: String,
    content_type: String,
}

fn pending() -> &'static Mutex<HashMap<u64, oneshot::Sender<Reply>>> {
    static P: OnceLock<Mutex<HashMap<u64, oneshot::Sender<Reply>>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(HashMap::new()))
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn lan_ip() -> String {
    crate::dlna::list_local_ipv4()
        .into_iter()
        .find(|ip| !ip.starts_with("127.") && !ip.starts_with("169.254."))
        .unwrap_or_else(|| "127.0.0.1".to_string())
}

/* ---------------- 起 / 停 ---------------- */

#[tauri::command]
pub async fn endpoint_start(app: tauri::AppHandle, port: Option<u16>) -> Result<EndpointInfo, String> {
    if let Ok(g) = state().lock() {
        if let Some(ins) = g.as_ref() {
            return Ok(ins.info.clone());
        }
    }

    let start = port.unwrap_or(PREFERRED_PORT);
    let mut listener = None;
    let mut used = 0u16;
    for p in start..start.saturating_add(40) {
        match TcpListener::bind(("0.0.0.0", p)).await {
            Ok(l) => {
                listener = Some(l);
                used = p;
                break;
            }
            Err(_) => continue,
        }
    }
    let listener = listener.ok_or_else(|| {
        format!(
            "没有可用端口（{}-{} 全被占）",
            start,
            start.saturating_add(39)
        )
    })?;

    let ip = lan_ip();
    let info = EndpointInfo {
        success: true,
        http_port: used,
        device_uuid: format!("desktop-{}-{}", ip, used),
        ip: ip.clone(),
    };

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_task = shutdown.clone();
    let app_task = app.clone();

    tauri::async_runtime::spawn(async move {
        loop {
            if shutdown_task.load(Ordering::SeqCst) {
                break;
            }
            let accept = tokio::time::timeout(Duration::from_millis(300), listener.accept()).await;
            match accept {
                Ok(Ok((mut stream, _addr))) => {
                    let app = app_task.clone();
                    tauri::async_runtime::spawn(async move {
                        let _ = tokio::time::timeout(
                            Duration::from_secs(REQUEST_TIMEOUT_SEC),
                            handle(&mut stream, &app),
                        )
                        .await;
                        let _ = stream.shutdown().await;
                    });
                }
                Ok(Err(_)) => continue,
                Err(_) => continue, // timeout：回去检查 shutdown
            }
        }
    });

    if let Ok(mut g) = state().lock() {
        *g = Some(Instance {
            shutdown,
            info: info.clone(),
        });
    }
    Ok(info)
}

#[tauri::command]
pub fn endpoint_stop() -> Result<bool, String> {
    let mut g = state().lock().map_err(|e| format!("锁失败: {e}"))?;
    if let Some(ins) = g.take() {
        ins.shutdown.store(true, Ordering::SeqCst);
        if let Ok(mut p) = pending().lock() {
            p.clear();
        }
        Ok(true)
    } else {
        Ok(false)
    }
}

#[tauri::command]
pub fn endpoint_info() -> Option<EndpointInfo> {
    state().lock().ok().and_then(|g| g.as_ref().map(|i| i.info.clone()))
}

/// 前端算完响应后回填。
///
/// ⛔ 返回 Ok(false) 表示「请求号已经过期（超时被摘掉）」——这不报错：
/// 插件侧同样静默丢包，客户端拿到的是 504。把它当错误抛会让 JS 侧误以为桥断了。
#[tauri::command]
pub fn endpoint_reply(
    request_id: u64,
    status: u16,
    body: String,
    content_type: Option<String>,
) -> Result<bool, String> {
    let mut g = pending().lock().map_err(|e| format!("锁失败: {e}"))?;
    let Some(tx) = g.remove(&request_id) else {
        return Ok(false);
    };
    let reply = Reply {
        status,
        body,
        content_type: content_type.unwrap_or_else(|| "application/json; charset=utf-8".to_string()),
    };
    Ok(tx.send(reply).is_ok())
}

/* ---------------- 请求处理 ---------------- */

struct Request {
    method: String,
    /// **带 query**（插件 v1.0.1 口径）
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

async fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut tmp = [0u8; 4096];

    let head_end = loop {
        if let Some(p) = find_subslice(&buf, b"\r\n\r\n") {
            break p;
        }
        let n = stream.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > 64 * 1024 {
            return None;
        }
    };

    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let first = lines.next()?;
    let mut parts = first.split_whitespace();
    let method = parts.next()?.to_ascii_uppercase();
    // ⛔ 保留 query：手机端的增量游标走 `?since=`，丢掉就退化成全量
    let path = parts.next()?.to_string();

    let mut headers = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }

    let header = |name: &str| -> Option<String> {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    };

    let leftover = buf.len().saturating_sub(head_end + 4);
    let mut body: Vec<u8> = if leftover > 0 {
        buf[head_end + 4..].to_vec()
    } else {
        Vec::new()
    };

    // 手机端（wx.request / OkHttp）一律走 Content-Length。chunked 只把已读到的字节透传，
    // **不做 chunk 解码** —— 留言板的 `.mbd` 都是完整 JSON，没有流式上传的场景，
    // 真要支持流式得先有需求（写一个半吊子解析器比不支持更难查）。
    let chunked = header("transfer-encoding")
        .map(|v| v.to_ascii_lowercase().contains("chunked"))
        .unwrap_or(false);
    if !chunked {
        let len = header("content-length")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0)
            .min(MAX_BODY);
        while body.len() < len {
            let n = stream.read(&mut tmp).await.ok()?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&tmp[..n]);
            if body.len() > MAX_BODY {
                break;
            }
        }
        body.truncate(len);
    }

    Some(Request {
        method,
        path,
        headers,
        body,
    })
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "OK",
    }
}

async fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    // CORS 一律放行：手机端是原生 HTTP 客户端，但 web 预览/调试工具会按浏览器规则发预检，
    // 卡在这里会表现成「手机上打得开、浏览器里 404」。
    let head = format!(
        "HTTP/1.1 {} {}\r\n\
         Content-Type: {}\r\n\
         Content-Length: {}\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Access-Control-Allow-Headers: *\r\n\
         Access-Control-Allow-Methods: GET,POST,PUT,DELETE,OPTIONS\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\r\n",
        status,
        reason(status),
        content_type,
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await
}

async fn handle(stream: &mut TcpStream, app: &tauri::AppHandle) {
    let Some(req) = read_request(stream).await else {
        return;
    };

    // 预检直接回，不进 JS（JS 侧没有 OPTIONS 的路由，下去只会拿到 404）
    if req.method == "OPTIONS" {
        let _ = write_response(stream, 204, "text/plain", &[]).await;
        return;
    }

    let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
    let (tx, rx) = oneshot::channel();
    if let Ok(mut g) = pending().lock() {
        g.insert(id, tx);
    }

    let payload = serde_json::json!({
        "requestId": id,
        "method": req.method,
        "path": req.path,
        "headers": serde_json::to_string(&req.headers).unwrap_or_else(|_| "[]".to_string()),
        "body": String::from_utf8_lossy(&req.body).to_string(),
    });

    if let Err(e) = app.emit("endpoint://request", payload) {
        if let Ok(mut g) = pending().lock() {
            g.remove(&id);
        }
        let body = format!(r#"{{"ok":false,"error":"emit failed: {e}"}}"#);
        let _ = write_response(stream, 500, "application/json; charset=utf-8", body.as_bytes()).await;
        return;
    }

    match tokio::time::timeout(Duration::from_millis(REPLY_TIMEOUT_MS), rx).await {
        Ok(Ok(reply)) => {
            let ct = reply.content_type;
            let _ = write_response(stream, reply.status, &ct, reply.body.as_bytes()).await;
        }
        Ok(Err(_)) => {
            // 发送端被 drop（stop 时 clear）→ 当 503
            let _ = write_response(
                stream,
                503,
                "application/json; charset=utf-8",
                br#"{"ok":false,"error":"endpoint stopped"}"#,
            )
            .await;
        }
        Err(_) => {
            if let Ok(mut g) = pending().lock() {
                g.remove(&id);
            }
            let _ = write_response(
                stream,
                504,
                "application/json; charset=utf-8",
                br#"{"ok":false,"error":"js handler timeout"}"#,
            )
            .await;
        }
    }
}
