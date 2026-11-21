//! 桌面端 WebDAV 服务端（真起端口，不是 mock）。
//!
//! 为什么需要它：快应用在 Android 上靠插件 `eskit.plugin.webdav.server` 起局域网服务，
//! 手机小程序把 `.nes` / `.md` / 图片 PUT 上来落盘。桌面端（WKWebView/WebView2）没有这个
//! 原生插件，web-renderer 的 `EsWebdavServerModule` 又是纯 mock —— 于是「投屏收包」在 PC 上
//! 永远是死路。这里用 tokio 手写一个够用的 WebDAV 端，把这条链路补回来：
//!
//!   起端口 → 手机 PUT → 落盘到 app data → 页面 `readBase64` 读回 → 应用打开文件
//!
//! 只实现快应用真正用得到的方法（OPTIONS / PUT / GET / HEAD / DELETE / MKCOL / PROPFIND）。
//! 不做锁、不做 MOVE/COPY、不做 Range —— 这不是通用网盘，别按通用网盘验收。
//!
//! ## 目录约定
//! 落盘根 = `<app_data_dir>/es_files`，URL 路径直接映射到根下。
//! 所以 `PUT /es_files/files/a.nes` → `<app_data>/es_files/es_files/files/a.nes`。
//! 看着多一层，但**必须保留**：快应用侧生成的是带 `/es_files/` 前缀的绝对 URL
//! （与 Android 插件同一口径），少拼这一层就会和真机路径不一致、页面读不回来。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde::Serialize;
use tauri::Emitter;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Clone, Debug, Serialize)]
pub struct WebdavInfo {
    pub url: String,
    pub ip: String,
    pub port: u16,
    pub root: String,
    pub running: bool,
}

struct Instance {
    shutdown: std::sync::Arc<AtomicBool>,
    info: WebdavInfo,
}

fn state() -> &'static Mutex<Option<Instance>> {
    static S: OnceLock<Mutex<Option<Instance>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}

/// 单次请求上限：卡带最大 4MB，给 64MB 足够覆盖文档/图片
const MAX_BODY: usize = 64 * 1024 * 1024;
/// 首选端口（与 Android 插件默认一致）
const PREFERRED_PORT: u16 = 5990;

fn default_root() -> PathBuf {
    // 落盘根可被 `ES_FILES_DIR` 覆盖：验收脚本要能指到临时目录，
    // 用户想换个盘（比如放到大容量外置盘）也不用改二进制。
    if let Some(p) = std::env::var("ES_FILES_DIR")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        return PathBuf::from(p);
    }
    // app_data_dir 在 dev 下是 ~/Library/Application Support/<identifier>
    let base = dirs_app_data().unwrap_or_else(|| std::env::temp_dir());
    base.join("es_files")
}

fn dirs_app_data() -> Option<PathBuf> {
    // 不引新依赖：macOS / Windows / Linux 各一条
    #[cfg(target_os = "macos")]
    {
        if let Ok(home) = std::env::var("HOME") {
            return Some(
                PathBuf::from(home)
                    .join("Library")
                    .join("Application Support")
                    .join("com.chendd.toupingxianxiang"),
            );
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            return Some(PathBuf::from(appdata).join("com.chendd.toupingxianxiang"));
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Ok(home) = std::env::var("HOME") {
            return Some(PathBuf::from(home).join(".local/share/com.chendd.toupingxianxiang"));
        }
    }
    None
}

/// URL 路径 → 磁盘路径（含 percent-decode，中文文件名要能落盘）。
///
/// ⛔ **必须先 decode 再校验，不能拿原文字面量判**。
/// `%2e%2e` 解码出来就是 `..`：只判 `seg == ".."` 的话，一个
/// `PUT /%2e%2e/%2e%2e/Library/xxx` 就能把文件写到落盘根之外，
/// 而这个服务绑的是 `0.0.0.0`（同局域网谁都能连）。
/// 同理 `%2f` / `%5c` 解码出分隔符，等于凭空多跳一层目录，也要一起拒。
fn safe_join(root: &Path, url_path: &str) -> Option<PathBuf> {
    let mut out = root.to_path_buf();
    for seg in url_path.split('/') {
        if seg.is_empty() {
            continue;
        }
        let decoded = percent_decode(seg);
        if decoded.is_empty() || decoded == "." {
            continue;
        }
        if decoded == ".."
            || decoded.contains('/')
            || decoded.contains('\\')
            || decoded.contains('\0')
        {
            return None;
        }
        out.push(decoded);
    }
    Some(out)
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hi = (b[i + 1] as char).to_digit(16);
            let lo = (b[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    /// 这个请求处理完必须关连接（HTTP/1.0、客户端要 close、或 chunked 上传）
    close: bool,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// 读一个完整请求。返回 None 表示连接已断 / 报文损坏。
///
/// `pending` 是上一次请求多读出来的字节（keep-alive 下，一个 TCP 包里可能
/// 已经带了下一个请求的开头）。读完后把多出来的部分放回去，下次从这里接着解析。
async fn read_request(stream: &mut TcpStream, pending: &mut Vec<u8>) -> Option<Request> {
    let mut buf: Vec<u8> = std::mem::take(pending);
    let mut tmp = [0u8; 4096];
    // 读到头部结束（\r\n\r\n）
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
            return None; // 头部过大，直接断
        }
    };

    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let first = lines.next()?;
    let mut parts = first.split_whitespace();
    let method = parts.next()?.to_string();
    let raw_path = parts.next()?.to_string();
    let path = raw_path.split('?').next().unwrap_or("/").to_string();

    let mut headers = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }

    let mut req = Request {
        method,
        path,
        headers,
        body: Vec::new(),
        close: false,
    };

    // keep-alive 判定：只在客户端**明确**要的时候复用连接。
    //
    // HTTP/1.1 的默认语义也是 keep-alive，但这里不能照着默认来：
    // 老的投屏客户端（以及验收里的测试）是「读到 EOF 才算一个响应结束」，
    // 默认开着会让他们一直等我们超时（实测单条测试卡 120s）。
    // 手柄包是我们自己的代码，明确带 `Connection: keep-alive`，只有它走复用。
    let conn = req
        .header("connection")
        .map(|v| v.to_ascii_lowercase())
        .unwrap_or_default();
    req.close = !conn.contains("keep-alive");

    // 头部之后已经读进 buf 的字节（可能是 body 的开头）
    let leftover = buf.len().saturating_sub(head_end + 4);
    let rest: Vec<u8> = if leftover > 0 {
        buf[head_end + 4..].to_vec()
    } else {
        Vec::new()
    };

    // 分块上传：必须真解析。走 Content-Length 那条路会把长度当 0，
    // 写出 0 字节文件却回 201 —— 静默丢数据。
    let chunked = req
        .header("transfer-encoding")
        .map(|v| v.to_ascii_lowercase().contains("chunked"))
        .unwrap_or(false);
    if chunked {
        req.body = read_chunked(stream, rest, MAX_BODY).await?;
        // chunked 的长度靠 0 号 chunk 结尾，边界之后还剩多少无从得知 —— 只能关连接
        req.close = true;
        return Some(req);
    }

    let len = req
        .header("content-length")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    if len > MAX_BODY {
        // 明确拒绝而不是截断：宁可让客户端报错，也不要写出半截文件
        let _ = write_response(stream, 413, &[], b"payload too large", false).await;
        return None;
    }

    req.body = rest;
    while req.body.len() < len {
        let n = stream.read(&mut tmp).await.ok()?;
        if n == 0 {
            break;
        }
        req.body.extend_from_slice(&tmp[..n]);
        if req.body.len() > MAX_BODY {
            break;
        }
    }
    // 读多了的部分属于下一个请求，不能算进 body
    if req.body.len() > len {
        *pending = req.body.split_off(len);
    }
    req.body.truncate(len);
    Some(req)
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// 解析 `Transfer-Encoding: chunked` 的请求体。`pending` = 头部之后已经读到的字节。
///
/// 没有这条路径时，chunked 请求的 `Content-Length` 缺失 → 长度按 0 算 →
/// 落盘一个 0 字节文件、还回 201。客户端以为传成功，实际数据全丢，
/// 而且文件存在、大小不对，排查时最容易被误判成"文件损坏"。
async fn read_chunked(stream: &mut TcpStream, mut pending: Vec<u8>, max: usize) -> Option<Vec<u8>> {
    let mut body: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        // 读 chunk-size 行（可能带 `;ext=xxx`，只取分号前）
        let line_end = loop {
            if let Some(p) = find_subslice(&pending, b"\r\n") {
                break p;
            }
            if pending.len() > 1024 {
                return None; // 大小行不可能这么长，报文有问题
            }
            let n = stream.read(&mut tmp).await.ok()?;
            if n == 0 {
                return None;
            }
            pending.extend_from_slice(&tmp[..n]);
        };
        let line = String::from_utf8_lossy(&pending[..line_end]).to_string();
        let size_txt = line.split(';').next().unwrap_or("").trim().to_string();
        let size = usize::from_str_radix(&size_txt, 16).ok()?;
        pending.drain(..line_end + 2);

        if size == 0 {
            // 0 号 chunk = 报文结束；trailer 一概忽略（我们没用到任何 trailer 头）
            return Some(body);
        }
        if body.len() + size > max {
            return None;
        }
        while pending.len() < size + 2 {
            let n = stream.read(&mut tmp).await.ok()?;
            if n == 0 {
                return None;
            }
            pending.extend_from_slice(&tmp[..n]);
        }
        body.extend_from_slice(&pending[..size]);
        pending.drain(..size + 2); // 丢掉 chunk 尾随的 CRLF
    }
}

async fn write_response(
    stream: &mut TcpStream,
    status: u16,
    extra: &[(&str, String)],
    body: &[u8],
    keep: bool,
) -> std::io::Result<()> {
    write_response_len(stream, status, extra, body, body.len(), keep).await
}

/// 带「声明长度」的响应。
///
/// ⛔ 必需的原因：HEAD 的实体必须是 0 字节，但 `Content-Length` 要报**真实文件大小**。
/// 如果调用方自己在 extra 里再塞一个 `Content-Length`，响应里就会同时出现
/// `Content-Length: 0` 和 `Content-Length: N` —— 同一个头出现两次且互相矛盾，
/// 属于 HTTP 协议违规，客户端可能直接报错或按 0 字节截断。
/// 所以长度**只从这里出**，调用方不许再写。
async fn write_response_len(
    stream: &mut TcpStream,
    status: u16,
    extra: &[(&str, String)],
    body: &[u8],
    declared_len: usize,
    keep: bool,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        207 => "Multi-Status",
        201 => "Created",
        404 => "Not Found",
        403 => "Forbidden",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        _ => "OK",
    };
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: {}\r\n",
        status,
        reason,
        declared_len,
        if keep { "keep-alive" } else { "close" }
    );
    for (k, v) in extra {
        head.push_str(&format!("{}: {}\r\n", k, v));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    if !body.is_empty() {
        stream.write_all(body).await?;
    }
    stream.flush().await
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// 目录列表（PROPFIND 用）。只列一层，足够文件管理器显示"有没有传上来"。
fn propfind_body(root: &Path, url_path: &str) -> String {
    let target = safe_join(root, url_path);
    let mut items: Vec<(String, bool, u64)> = Vec::new();
    if let Some(t) = target {
        if t.is_dir() {
            items.push((url_path.to_string(), true, 0));
            if let Ok(rd) = std::fs::read_dir(&t) {
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().to_string();
                    let p = e.path();
                    let is_dir = p.is_dir();
                    let size = e.metadata().map(|m| m.len()).unwrap_or(0);
                    let href = if url_path.ends_with('/') {
                        format!("{}{}", url_path, name)
                    } else {
                        format!("{}/{}", url_path, name)
                    };
                    items.push((href, is_dir, size));
                }
            }
        } else if t.is_file() {
            let size = std::fs::metadata(&t).map(|m| m.len()).unwrap_or(0);
            items.push((url_path.to_string(), false, size));
        }
    }
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:multistatus xmlns:D=\"DAV:\">\n",
    );
    for (href, is_dir, size) in items {
        xml.push_str(&format!(
            "<D:response><D:href>{}</D:href><D:propstat><D:prop><D:resourcetype>{}</D:resourcetype><D:getcontentlength>{}</D:getcontentlength></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>\n",
            xml_escape(&href),
            if is_dir { "<D:collection/>" } else { "" },
            size
        ));
    }
    xml.push_str("</D:multistatus>\n");
    xml
}

fn dav_headers() -> Vec<(&'static str, String)> {
    vec![
        ("DAV", "1,2".to_string()),
        ("Allow", "OPTIONS, GET, HEAD, PUT, DELETE, MKCOL, PROPFIND".to_string()),
        ("MS-Author-Via", "DAV".to_string()),
        ("Accept-Ranges", "none".to_string()),
    ]
}

/// 一个 TCP 连接上的请求循环。
/// （`app` 为 `None` 时只做 HTTP 语义、不发事件 —— 单元测试就是这么跑真端口的，
/// `AppHandle` 只能在 Tauri runtime 里构造，测试里拿不到。）
///
/// 为什么要循环：手机手柄每 30–50ms 发一个状态包。每包都新建 TCP，
/// 光建连（微信里还要先 createTCPSocket）就要 300ms+，真机实测 423ms，手柄没法用。
/// 客户端声明 `Connection: keep-alive` 时，回完响应继续在同一连接上读下一个请求。
/// 老客户端（HTTP/1.0 / `Connection: close` / chunked 上传）行为完全不变。
async fn handle(stream: &mut TcpStream, root: &Path, app: Option<&tauri::AppHandle>) {
    let mut pending: Vec<u8> = Vec::new();
    loop {
        // 空闲连接不能一直挂着：120s 没新请求就回收（手柄断开后不会漏掉任务）
        let next = tokio::time::timeout(Duration::from_secs(120), read_request(stream, &mut pending)).await;
        let Ok(Some(req)) = next else {
            return;
        };
        let keep = !req.close;
        serve(stream, root, app, &req, keep).await;
        if !keep {
            return;
        }
    }
}

async fn serve(
    stream: &mut TcpStream,
    root: &Path,
    app: Option<&tauri::AppHandle>,
    req: &Request,
    keep: bool,
) {
    // 验收用：GET /__info 直接报服务状态与已落盘文件，省得去翻目录
    if req.path == "/__info" {
        let body = info_json(root);
        let _ = write_response(
            stream,
            200,
            &[("Content-Type", "application/json".to_string())],
            body.as_bytes(),
            keep,
        )
        .await;
        return;
    }

    let Some(target) = safe_join(root, &req.path) else {
        let _ = write_response(stream, 403, &[], b"forbidden", keep).await;
        return;
    };

    match req.method.as_str() {
        "OPTIONS" => {
            let _ = write_response(stream, 204, &dav_headers(), b"", keep).await;
        }
        "PUT" => {
            if let Some(parent) = target.parent() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    let _ = write_response(stream, 500, &[], format!("mkdir: {e}").as_bytes(), keep).await;
                    return;
                }
            }
            match std::fs::write(&target, &req.body) {
                Ok(_) => {
                    // 通知前端：文件已落盘。快应用侧靠这条事件跳进游戏页
                    // （Android 上由插件 `upload_success` 发出，桌面端只能自己发）。
                    let name = target
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default();
                    if let Some(app) = app {
                        let _ = app.emit(
                            "webdav://upload",
                            serde_json::json!({
                                "path": target.to_string_lossy(),
                                "name": name,
                                "size": req.body.len(),
                            }),
                        );
                    }
                    let _ = write_response(stream, 201, &dav_headers(), b"", keep).await;
                }
                Err(e) => {
                    let _ = write_response(stream, 500, &[], format!("write: {e}").as_bytes(), keep).await;
                }
            }
        }
        "GET" | "HEAD" => match std::fs::read(&target) {
            Ok(bytes) => {
                let is_head = req.method == "HEAD";
                let body: &[u8] = if is_head { &[] } else { &bytes };
                let extra = vec![("Content-Type", "application/octet-stream".to_string())];
                // HEAD 实体空、声明真实长度；长度由 write_response_len 统一输出
                let _ = write_response_len(stream, 200, &extra, body, bytes.len(), keep).await;
            }
            Err(_) => {
                let _ = write_response(stream, 404, &dav_headers(), b"", keep).await;
            }
        },
        "DELETE" => {
            // 失败必须能看见原因：之前只回 404、把 io::Error 整个吞掉，
            // 现场只能看到"删不掉"，分不清是不存在、没权限还是被占用。
            let err = if target.is_dir() {
                std::fs::remove_dir_all(&target).err()
            } else {
                std::fs::remove_file(&target).err()
            };
            let (code, body) = match err {
                Some(e) if e.kind() == std::io::ErrorKind::NotFound => (404, b"".to_vec()),
                Some(e) => (500, format!("delete: {e}").into_bytes()),
                None => (204, b"".to_vec()),
            };
            let _ = write_response(stream, code, &dav_headers(), &body, keep).await;
        }
        "MKCOL" => {
            let ok = std::fs::create_dir_all(&target).is_ok();
            let _ = write_response(stream, if ok { 201 } else { 500 }, &dav_headers(), b"", keep).await;
        }
        "PROPFIND" => {
            let xml = propfind_body(root, &req.path);
            let mut extra = dav_headers();
            extra.push(("Content-Type", "application/xml; charset=utf-8".to_string()));
            let _ = write_response(stream, 207, &extra, xml.as_bytes(), keep).await;
        }
        _ => {
            let _ = write_response(stream, 405, &dav_headers(), b"method not allowed", keep).await;
        }
    }
}

fn info_json(root: &Path) -> String {
    let mut files: Vec<String> = Vec::new();
    fn walk(dir: &Path, base: &Path, out: &mut Vec<String>, depth: usize) {
        if depth > 3 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, base, out, depth + 1);
            } else if let Ok(rel) = p.strip_prefix(base) {
                out.push(rel.to_string_lossy().to_string());
            }
        }
    }
    walk(root, root, &mut files, 0);
    let cur = state().lock().ok().and_then(|g| g.as_ref().map(|i| i.info.clone()));
    serde_json::json!({
        "running": cur.is_some(),
        "url": cur.as_ref().map(|i| i.url.clone()).unwrap_or_default(),
        "port": cur.as_ref().map(|i| i.port).unwrap_or(0),
        "root": root.to_string_lossy(),
        "files": files,
    })
    .to_string()
}

fn lan_ip() -> String {
    crate::dlna::list_local_ipv4()
        .into_iter()
        .find(|ip| !ip.starts_with("127.") && !ip.starts_with("169.254."))
        .unwrap_or_else(|| "127.0.0.1".to_string())
}

/// 起服务。端口从 5990 往上找，被占就顺延（与 Android 的 AvailablePortFinder 行为一致）。
#[tauri::command]
pub async fn webdav_start(app: tauri::AppHandle, port: Option<u16>) -> Result<WebdavInfo, String> {
    if let Ok(g) = state().lock() {
        if let Some(ins) = g.as_ref() {
            return Ok(ins.info.clone());
        }
    }

    let root = default_root();
    std::fs::create_dir_all(root.join("es_files")).map_err(|e| format!("创建落盘目录失败: {e}"))?;

    let mut listener = None;
    let mut used = 0u16;
    let start = port.unwrap_or(PREFERRED_PORT);
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
    let listener = listener.ok_or_else(|| "没有可用端口（5990-6029 全被占）".to_string())?;

    let ip = lan_ip();
    let info = WebdavInfo {
        url: format!("http://{}:{}", ip, used),
        ip,
        port: used,
        root: root.to_string_lossy().to_string(),
        running: true,
    };

    let shutdown = std::sync::Arc::new(AtomicBool::new(false));
    let shutdown_task = shutdown.clone();
    let root_task = root.clone();
    let info_task = info.clone();
    let app_task = app.clone();

    tauri::async_runtime::spawn(async move {
        loop {
            if shutdown_task.load(Ordering::SeqCst) {
                break;
            }
            let accept = tokio::time::timeout(Duration::from_millis(300), listener.accept()).await;
            match accept {
                Ok(Ok((mut stream, _addr))) => {
                    let root = root_task.clone();
                    let app = app_task.clone();
                    tauri::async_runtime::spawn(async move {
                        // 单请求超时：手机断线/半包不能把 handler 永久挂住
                        let _ = tokio::time::timeout(
                            Duration::from_secs(60),
                            handle(&mut stream, &root, Some(&app)),
                        )
                        .await;
                        let _ = stream.shutdown().await;
                    });
                }
                Ok(Err(_)) => continue,
                Err(_) => continue, // timeout：回去检查 shutdown
            }
        }
        let _ = info_task;
    });

    if let Ok(mut g) = state().lock() {
        *g = Some(Instance {
            shutdown,
            info: info.clone(),
        });
    }
    Ok(info)
}

/// 停服务。**必须真的把端口释放**：restart 时旧 listener 不关，新起的会顺延到 5991，
/// 页面二维码就对不上了。
#[tauri::command]
pub async fn webdav_stop() -> Result<bool, String> {
    let mut old = None;
    if let Ok(mut g) = state().lock() {
        old = g.take();
    }
    let Some(ins) = old else {
        return Ok(false);
    };
    ins.shutdown.store(true, Ordering::SeqCst);

    // ✅ 不能只 sleep 一个拍脑袋的时长就宣布"已释放"：
    //    accept 循环 300ms 一轮，listener 要等那个任务真正结束才 drop。
    //    赌调度赌输了下一次 start 会顺延到 5991，页面二维码就和实际端口对不上。
    //    这里直接轮询「这个端口能不能重新绑上」—— 能绑上才算真的放了。
    //    探测用的 listener 是临时值，判完即 drop，不留占用。
    let port = ins.info.port;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if std::net::TcpListener::bind(("0.0.0.0", port)).is_ok() {
            return Ok(true);
        }
    }
    Ok(true)
}

#[tauri::command]
pub async fn webdav_info() -> Result<WebdavInfo, String> {
    if let Ok(g) = state().lock() {
        if let Some(ins) = g.as_ref() {
            return Ok(ins.info.clone());
        }
    }
    Ok(WebdavInfo {
        url: String::new(),
        ip: String::new(),
        port: 0,
        root: default_root().to_string_lossy().to_string(),
        running: false,
    })
}

/// 读本地文件为 base64 —— 快应用 `FileReaderModule.readBase64` 的桌面实现。
/// 没有它，「投上来的卡带」落盘了也打不开（web 端 `fetch('file://')` 必被 CORS 拦）。
#[tauri::command]
pub async fn es_read_file_base64(path: String) -> Result<String, String> {
    use base64::Engine as _;
    let p = PathBuf::from(&path);
    // 只允许读落盘根下的文件：这是个能读任意路径的接口，不做限制等于给 webview 开洞
    let root = default_root();
    let canon_root = std::fs::canonicalize(&root).unwrap_or(root.clone());
    let canon = std::fs::canonicalize(&p).map_err(|e| format!("文件不存在: {e}"))?;
    if !canon.starts_with(&canon_root) {
        return Err("拒绝读取落盘目录之外的文件".to_string());
    }
    let bytes = std::fs::read(&canon).map_err(|e| format!("读取失败: {e}"))?;
    Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
}

/// 列出落盘根下的文件（页面/验收脚本用来确认"真的存下来了"）
#[tauri::command]
pub async fn es_list_files() -> Result<Vec<String>, String> {
    let root = default_root();
    let mut out = Vec::new();
    fn walk(dir: &Path, base: &Path, out: &mut Vec<String>, depth: usize) {
        if depth > 4 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, base, out, depth + 1);
            } else if p.strip_prefix(base).is_ok() {
                out.push(p.to_string_lossy().to_string());
            }
        }
    }
    walk(&root, &root, &mut out, 0);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root(tag: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p = std::env::temp_dir().join(format!(
            "qad-webdav-{}-{}-{}",
            std::process::id(),
            tag,
            stamp
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// 起一个真的 TCP 端口，用生产同一份 `handle` 处理请求。
    async fn spawn_server(root: PathBuf) -> std::net::SocketAddr {
        let l = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = l.accept().await else {
                    continue;
                };
                let r = root.clone();
                tokio::spawn(async move {
                    handle(&mut s, &r, None).await;
                    let _ = s.shutdown().await;
                });
            }
        });
        addr
    }

    /// 发一段原始 HTTP 报文，返回 (状态码, 响应头原文, 响应体)
    async fn send(addr: std::net::SocketAddr, raw: Vec<u8>) -> (u16, String, Vec<u8>) {
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(&raw).await.unwrap();
        s.flush().await.unwrap();
        let mut buf = Vec::new();
        let _ = s.read_to_end(&mut buf).await;
        let pos = buf.windows(4).position(|w| w == b"\r\n\r\n");
        let (head, body) = match pos {
            Some(p) => (
                String::from_utf8_lossy(&buf[..p]).to_string(),
                buf[p + 4..].to_vec(),
            ),
            None => (String::from_utf8_lossy(&buf).to_string(), Vec::new()),
        };
        let status = head
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        (status, head, body)
    }

    fn count_header(head: &str, name: &str) -> usize {
        head.lines()
            .filter(|l| {
                l.split(':')
                    .next()
                    .map(|k| k.trim().eq_ignore_ascii_case(name))
                    .unwrap_or(false)
            })
            .count()
    }

    // ---------- 纯函数 ----------

    #[test]
    fn percent_decode_edge_cases() {
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("%2e%2e"), "..");
        // 不完整的转义原样保留，不能 panic、不能吞字符
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("a%zz"), "a%zz");
    }

    #[test]
    fn safe_join_rejects_traversal() {
        let root = Path::new("/tmp/root");
        assert!(safe_join(root, "/es_files/files/a.nes").is_some());
        assert!(safe_join(root, "/../etc/passwd").is_none());
        // ⛔ 这四条是曾经的洞 / 潜在洞：解码后才现形的 ..
        assert!(safe_join(root, "/%2e%2e/%2e%2e/etc/passwd").is_none());
        assert!(safe_join(root, "/..%2fetc%2fpasswd").is_none());
        assert!(safe_join(root, "/a%5c..%5cb").is_none());
        assert!(safe_join(root, "/..%00/x").is_none());
    }

    #[test]
    fn safe_join_keeps_chinese_names() {
        let p = safe_join(
            Path::new("/tmp/root"),
            "/es_files/files/%E5%8D%A1%E5%B8%A6.nes",
        )
        .unwrap();
        assert_eq!(p, Path::new("/tmp/root/es_files/files/卡带.nes"));
    }

    // ---------- 真端口 · 真 HTTP ----------

    #[tokio::test]
    async fn http_put_get_head_delete_propfind() {
        let root = tmp_root("verbs");
        let addr = spawn_server(root.clone()).await;
        let payload = b"NES\x1a\x02\x01fake-rom-bytes".to_vec();

        // OPTIONS：WebDAV 客户端探测能力
        let (st, head, _) = send(
            addr,
            b"OPTIONS / HTTP/1.1\r\nHost: x\r\n\r\n".to_vec(),
        )
        .await;
        assert_eq!(st, 204);
        assert!(
            head.to_ascii_uppercase().contains("DAV:"),
            "OPTIONS 必须带 DAV 头，否则 WebDAV 客户端不认"
        );

        // PUT
        let mut raw = format!(
            "PUT /es_files/files/a.nes HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n",
            payload.len()
        )
        .into_bytes();
        raw.extend_from_slice(&payload);
        let (st, _, _) = send(addr, raw).await;
        assert_eq!(st, 201, "PUT 应当回 201");

        let disk = root.join("es_files/files/a.nes");
        assert!(disk.is_file(), "文件必须真的落到磁盘上");
        assert_eq!(std::fs::read(&disk).unwrap(), payload);

        // GET
        let (st, head, body) = send(
            addr,
            b"GET /es_files/files/a.nes HTTP/1.1\r\nHost: x\r\n\r\n".to_vec(),
        )
        .await;
        assert_eq!(st, 200);
        assert_eq!(body, payload);
        assert_eq!(count_header(&head, "content-length"), 1);

        // HEAD：实体必须 0 字节，但声明的长度是真实文件大小
        let (st, head, body) = send(
            addr,
            b"HEAD /es_files/files/a.nes HTTP/1.1\r\nHost: x\r\n\r\n".to_vec(),
        )
        .await;
        assert_eq!(st, 200);
        assert!(body.is_empty(), "HEAD 不能有实体");
        assert_eq!(
            count_header(&head, "content-length"),
            1,
            "HEAD 只能有一个 Content-Length（不能 0 和真实大小两个都写）"
        );
        assert!(head.contains(&format!("Content-Length: {}", payload.len())));

        // PROPFIND：目录列表要能看到刚传的文件
        let (st, _, body) = send(
            addr,
            b"PROPFIND /es_files/files HTTP/1.1\r\nHost: x\r\nDepth: 1\r\n\r\n".to_vec(),
        )
        .await;
        assert_eq!(st, 207);
        assert!(
            String::from_utf8_lossy(&body).contains("a.nes"),
            "PROPFIND 结果里要列出刚传的文件"
        );

        // DELETE
        let (st, _, _) = send(
            addr,
            b"DELETE /es_files/files/a.nes HTTP/1.1\r\nHost: x\r\n\r\n".to_vec(),
        )
        .await;
        assert_eq!(st, 204);
        assert!(!disk.exists(), "DELETE 之后文件必须消失");
    }

    #[tokio::test]
    async fn blocks_encoded_path_traversal() {
        let root = tmp_root("traversal");
        let addr = spawn_server(root.clone()).await;
        let raw =
            b"PUT /%2e%2e/%2e%2e/evil.txt HTTP/1.1\r\nHost: x\r\nContent-Length: 4\r\n\r\npwn!"
                .to_vec();
        let (st, _, _) = send(addr, raw).await;
        assert_eq!(st, 403, "编码成 %2e%2e 的 .. 必须被拦");
        assert!(
            !root.parent().unwrap().join("evil.txt").exists(),
            "绝不能把文件写到落盘根之外"
        );
    }

    /// 从一条**没有 EOF** 的连接上读一个完整响应（按 Content-Length 切）
    async fn read_one(s: &mut TcpStream) -> (u16, String, Vec<u8>) {
        let mut buf: Vec<u8> = Vec::new();
        let mut tmp = [0u8; 4096];
        let pos = loop {
            if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break p;
            }
            let n = s.read(&mut tmp).await.unwrap();
            assert!(n > 0, "连接提前断开");
            buf.extend_from_slice(&tmp[..n]);
        };
        let head = String::from_utf8_lossy(&buf[..pos]).to_string();
        let len = head
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                if k.trim().eq_ignore_ascii_case("content-length") {
                    v.trim().parse::<usize>().ok()
                } else {
                    None
                }
            })
            .unwrap_or(0);
        while buf.len() < pos + 4 + len {
            let n = s.read(&mut tmp).await.unwrap();
            assert!(n > 0, "响应体没收全");
            buf.extend_from_slice(&tmp[..n]);
        }
        let body = buf[pos + 4..pos + 4 + len].to_vec();
        let status = head
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|x| x.parse().ok())
            .unwrap_or(0);
        (status, head, body)
    }

    #[tokio::test]
    async fn keep_alive_reuses_one_connection() {
        // 手柄包每 30–50ms 一个。每包新建 TCP 的话，光建连（微信里还要先
        // createTCPSocket）就 300ms+，真机实测 423ms，手柄没法用。
        // 这条测试盯死两件事：同一条连接能连续处理多个请求；老客户端行为不变。
        let root = tmp_root("keepalive");
        let addr = spawn_server(root.clone()).await;
        let mut s = TcpStream::connect(addr).await.unwrap();

        for i in 0..3u8 {
            let body = format!("{{\"seq\":{i}}}");
            let raw = format!(
                "PUT /es_files/files/pad.json HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{}",
                body.len(),
                body
            );
            s.write_all(raw.as_bytes()).await.unwrap();
            let (st, head, _) = read_one(&mut s).await;
            assert_eq!(st, 201);
            assert!(
                head.to_ascii_lowercase().contains("connection: keep-alive"),
                "keep-alive 请求必须回 keep-alive，否则客户端只能每包重连"
            );
        }
        assert_eq!(
            std::fs::read(root.join("es_files/files/pad.json")).unwrap(),
            b"{\"seq\":2}",
            "三次覆盖写只留一份，且是最后一包"
        );

        // 老客户端（明确 close）：回完就断，不能被 keep-alive 挂住
        let mut s2 = TcpStream::connect(addr).await.unwrap();
        s2.write_all(
            b"PUT /es_files/files/x.json HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
        )
        .await
        .unwrap();
        let (st, head, _) = read_one(&mut s2).await;
        assert_eq!(st, 201);
        assert!(head.to_ascii_lowercase().contains("connection: close"));
        let mut rest = Vec::new();
        let _ = s2.read_to_end(&mut rest).await;
        assert!(rest.is_empty(), "close 的响应之后连接必须结束");
    }

    #[tokio::test]
    async fn accepts_chunked_upload() {
        let root = tmp_root("chunked");
        let addr = spawn_server(root.clone()).await;
        let raw = b"PUT /es_files/files/c.nes HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nHELLO\r\n3\r\nEND\r\n0\r\n\r\n".to_vec();
        let (st, _, _) = send(addr, raw).await;
        assert_eq!(st, 201);
        let got = std::fs::read(root.join("es_files/files/c.nes")).unwrap();
        assert_eq!(
            got, b"HELLOEND",
            "chunked 上传必须拼出完整内容，绝不能是 0 字节"
        );
    }
}
