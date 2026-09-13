//! media_proxy.rs — 回环流式媒体代理
//!
//! 背景：抖音等 CDN 的投屏 URL（ott_cast，jump_ttl=1）是「302 一次性签名调度」链接：
//! 每次请求都从入口域名 302 到「新 host + 新签名」的边缘节点（bdcgslb.com + bdcdn_rkey）。
//! `<video>` 元素直连时，每次补 Range / 重连 / seek / 切集都会重新触发 302 链，
//! 解析出新的随机子域与签名 → 连续加载断裂 → 播放卡在某个进度（切集后新集起不来）。
//!
//! 方案：本模块在 127.0.0.1 起一个常驻 HTTP 代理（media_proxy.js 把 video.src 改写指向它）：
//!   1. 首次请求：手动跟随 302 链（每跳带 douyin Referer，否则 CDN 可能拒绝收敛），
//!      解析出最终稳定节点 URL 并缓存（带 TTL）。
//!   2. 后续按浏览器 Range 直连缓存节点流式回传（206 + Content-Range + Accept-Ranges），
//!      video 侧只见 127.0.0.1 的稳定响应，不再暴露 302 / 随机子域。
//!   3. 缓存过期（上游再 302 / 签名失效）→ 清缓存重新解析一次。
//!
//! 安全：只 bind 127.0.0.1（绝不 0.0.0.0，防开放代理）；仅处理 /media?url=<encoded>。
//! 依赖：reqwest(stream) + futures-util + tokio。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::dlna::trace::dlog;

/// 最终节点签名有效期兜底（抖音 bdcdn_rkey 一般几分钟到几十分钟）
const CACHE_TTL: Duration = Duration::from_secs(600);
/// 302 链最大跳数（jump_ttl=1 的场景两跳内收敛，给余量）
const MAX_REDIRECTS: usize = 4;
/// 每跳携带的 Referer（抖音 CDN 防盗链校验，缺省会拒绝/不收敛）
const DOUYIN_REFERER: &str = "https://www.douyin.com/";
const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/126.0.0.0 Safari/537.36";

// ---------------------------------------------------------------------------
// 缓存：URL(入口) → 最终稳定节点 URL
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct MediaCache(Arc<Mutex<HashMap<String, CachedTarget>>>);

struct CachedTarget {
    final_url: String,
    expires_at: Instant,
}

impl MediaCache {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(HashMap::new())))
    }
    fn get(&self, key: &str) -> Option<String> {
        let map = self.0.lock().unwrap();
        map.get(key)
            .filter(|c| c.expires_at > Instant::now())
            .map(|c| c.final_url.clone())
    }
    fn set(&self, key: &str, final_url: String) {
        let mut map = self.0.lock().unwrap();
        map.insert(
            key.to_string(),
            CachedTarget {
                final_url,
                expires_at: Instant::now() + CACHE_TTL,
            },
        );
    }
    fn remove(&self, key: &str) {
        self.0.lock().unwrap().remove(key);
    }
}

// ---------------------------------------------------------------------------
// 常驻服务
// ---------------------------------------------------------------------------

/// 启动 127.0.0.1:port 媒体代理（常驻，失败返回 io error）。
pub async fn run_media_proxy(port: u16, cache: MediaCache) -> std::io::Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    dlog!("[media_proxy] listening on 127.0.0.1:{port}");
    loop {
        let (sock, _) = listener.accept().await?;
        let cache = cache.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(sock, cache).await {
                dlog!("[media_proxy] conn error: {e}");
            }
        });
    }
}

// ---------------------------------------------------------------------------
// 连接处理（单请求，Connection: close 简化）
// ---------------------------------------------------------------------------

struct Request {
    target_url: Option<String>, // /media?url=<encoded> 解析出的原始媒体 URL
    range: Option<String>,      // 浏览器 Range 头（如有）
}

async fn handle_conn(mut sock: TcpStream, cache: MediaCache) -> Result<(), String> {
    // 读请求头（限制 16KB）
    let mut buf = Vec::with_capacity(1024);
    let mut tmp = [0u8; 2048];
    loop {
        let n = sock
            .read(&mut tmp)
            .await
            .map_err(|e| format!("read request: {e}"))?;
        if n == 0 {
            return Err("client closed before request".into());
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > 16 * 1024 {
            return Err("request header too large".into());
        }
    }
    let head = String::from_utf8_lossy(&buf).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut req = Request {
        target_url: None,
        range: None,
    };
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("range") {
                req.range = Some(v.trim().to_string());
            }
        }
    }
    // 解析请求行：GET /media?url=<encoded> HTTP/1.1
    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 2 || parts[0] != "GET" {
        write_simple(&mut sock, 400, "text/plain", "only GET supported").await?;
        return Ok(());
    }
    let path = parts[1];
    if let Some(q) = path.strip_prefix("/media?url=") {
        // url 参数可能被编码
        let decoded = urlencoding_decode(q);
        if decoded.starts_with("http://") || decoded.starts_with("https://") {
            req.target_url = Some(decoded);
        }
    }
    let Some(target) = req.target_url else {
        write_simple(&mut sock, 400, "text/plain", "invalid /media?url=").await?;
        return Ok(());
    };

    // 主流程：查缓存 → 命中直连；miss 解析 302 → 缓存 → 流式回传
    let client = reqwest_client();
    let final_url = match cache.get(&target) {
        Some(u) => u,
        None => {
            match resolve_final(&client, &target).await {
                Ok(u) => {
                    cache.set(&target, u.clone());
                    u
                }
                Err(e) => {
                    write_simple(&mut sock, 502, "text/plain", &format!("resolve failed: {e}"))
                        .await?;
                    return Ok(());
                }
            }
        }
    };

    // 向最终节点流式拉取（带 Range）
    match stream_final(&client, &final_url, req.range.as_deref()).await {
        Ok((status, headers, body)) => {
            // 上游又给 302（缓存签名过期）：清缓存重试一次
            if status.as_u16() == 302 || status.as_u16() == 301 {
                cache.remove(&target);
                let final_url = match resolve_final(&client, &target).await {
                    Ok(u) => {
                        cache.set(&target, u.clone());
                        u
                    }
                    Err(e) => {
                        write_simple(&mut sock, 502, "text/plain", &format!("re-resolve failed: {e}"))
                            .await?;
                        return Ok(());
                    }
                };
                let (status2, headers2, body2) =
                    stream_final(&client, &final_url, req.range.as_deref()).await?;
                return write_stream_response(&mut sock, status2, &headers2, body2).await;
            }
            write_stream_response(&mut sock, status, &headers, body).await
        }
        Err(e) => {
            write_simple(&mut sock, 502, "text/plain", &format!("upstream error: {e}")).await?;
            Ok(())
        }
    }
}

fn reqwest_client() -> reqwest::Client {
    // 关闭自动跟随重定向：302 链由 resolve_final 手动处理（每跳带 Referer 可控）
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// 手动跟随 302 链（每跳带 douyin Referer），返回最终非重定向 URL。
async fn resolve_final(client: &reqwest::Client, entry: &str) -> Result<String, String> {
    let mut current = entry.to_string();
    for _ in 0..MAX_REDIRECTS {
        let resp = client
            .get(&current)
            .header("Referer", DOUYIN_REFERER)
            .header("User-Agent", USER_AGENT)
            .send()
            .await
            .map_err(|e| format!("request {e}"))?;
        let status = resp.status();
        if status.is_redirection() {
            let loc = resp
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| format!("redirect {status} without location"))?
                .to_string();
            current = if loc.starts_with("http://") || loc.starts_with("https://") {
                loc
            } else {
                // 相对 Location → 基于当前 URL 解析
                let base = url::Url::parse(&current).map_err(|e| format!("parse url: {e}"))?;
                base.join(&loc)
                    .map_err(|e| format!("join location: {e}"))?
                    .to_string()
            };
            continue;
        }
        // 非重定向（200/206/403 等）→ 就是最终节点（即使是 403 也缓存，由流式阶段暴露错误）
        return Ok(current);
    }
    Err("too many redirects".into())
}

/// 带 Range 请求最终节点，返回 (status, 需要回传的 headers, body 流)。
/// 说明：reqwest 默认不下载 body（惰性），此处只读 headers 返回流式 body。
async fn stream_final(
    client: &reqwest::Client,
    final_url: &str,
    range: Option<&str>,
) -> Result<(reqwest::StatusCode, Vec<(String, String)>, reqwest::Response), String> {
    let mut builder = client
        .get(final_url)
        .header("Referer", DOUYIN_REFERER)
        .header("User-Agent", USER_AGENT);
    if let Some(r) = range {
        builder = builder.header("Range", r);
    }
    let resp = builder.send().await.map_err(|e| format!("upstream {e}"))?;
    let status = resp.status();
    // 透传关键响应头（Content-Range/Accept-Ranges/Content-Length/Content-Type 等）
    let mut headers: Vec<(String, String)> = Vec::new();
    for (k, v) in resp.headers() {
        let kl = k.as_str().to_ascii_lowercase();
        // 剔除 hop-by-hop / 由我们控制的头
        if matches!(
            kl.as_str(),
            "transfer-encoding" | "connection" | "keep-alive" | "content-encoding"
        ) {
            continue;
        }
        if let Ok(vs) = v.to_str() {
            headers.push((k.to_string(), vs.to_string()));
        }
    }
    Ok((status, headers, resp))
}

/// 把上游响应（含 body 流）回写客户端；Connection: close。
async fn write_stream_response(
    sock: &mut TcpStream,
    status: reqwest::StatusCode,
    headers: &[(String, String)],
    body: reqwest::Response,
) -> Result<(), String> {
    let reason = status
        .canonical_reason()
        .unwrap_or("OK")
        .replace(' ', "-");
    let mut out = format!("HTTP/1.1 {} {}\r\n", status.as_u16(), reason);
    out.push_str("Connection: close\r\n");
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    // 未指定 content-length 时由浏览器按 connection close 判定结束（流式场景 OK）
    sock.write_all(out.as_bytes())
        .await
        .map_err(|e| format!("write head: {e}"))?;
    let mut stream = body.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("body stream: {e}"))?;
        sock.write_all(&chunk)
            .await
            .map_err(|e| format!("write body: {e}"))?;
    }
    Ok(())
}

async fn write_simple(sock: &mut TcpStream, code: u16, ct: &str, msg: &str) -> Result<(), String> {
    let body = msg.as_bytes();
    let head = format!(
        "HTTP/1.1 {code} {}\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        if code == 200 { "OK" } else { "Error" },
        body.len()
    );
    sock.write_all(head.as_bytes())
        .await
        .map_err(|e| format!("write err head: {e}"))?;
    sock.write_all(body)
        .await
        .map_err(|e| format!("write err body: {e}"))?;
    Ok(())
}

/// 简单百分号解码（只处理 UTF-8 URL 编码，足够 /media?url= 场景）。
fn urlencoding_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let h = hex_val(bytes[i + 1]);
            let l = hex_val(bytes[i + 2]);
            if let (Some(h), Some(l)) = (h, l) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}
