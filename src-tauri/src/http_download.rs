//! http_download.rs — Tauri 命令：Rust reqwest 流式下载（分块原始字节回传）
//!
//! 背景：web-runtime 去掉注入式 proxy（proxy_fetch.js）后，大文件下载
//! （es_pkg 加密 zip 等）由前端显式走本模块三命令：
//!   - http_download_open   建会话：reqwest GET 拉流，spawn task 把字节块推入 channel
//!   - http_download_read   按块回传原始字节（tauri::ipc::Response，无 base64 膨胀）
//!   - http_download_close  清理会话（rx drop → spawn task 退出）
//!
//! 安全：与 proxy_http 一致，拒绝私有/本地网段（防 SSRF）；命令仅本机 IPC 可达。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use futures_util::StreamExt;
use serde::Serialize;
use tokio::sync::mpsc;

/// 会话：url → 下载流的消费端（mpsc Receiver）。
/// Option 包装以便 read 时临时 take 出来跨 await 使用（MutexGuard 不能跨 await）。
pub struct DownloadState(pub Mutex<HashMap<String, Option<mpsc::Receiver<Result<Vec<u8>, String>>>>>);

impl Default for DownloadState {
    fn default() -> Self {
        Self(Mutex::new(HashMap::new()))
    }
}

/// 请求整体超时（对齐前端 COS_DOWNLOAD_TIMEOUT；大文件 5 分钟余量）
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
/// 连接超时
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// 内部 channel 容量（背压：上游快时最多缓存这么多块，避免内存暴涨）
const CHANNEL_CAPACITY: usize = 8;

const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/126.0.0.0 Safari/537.36";

/// 与 proxy.rs is_private_host 一致：拒绝私有/本地网段（防 SSRF / 开放代理）。
fn is_private_host(url_str: &str) -> bool {
    let parsed = match url::Url::parse(url_str) {
        Ok(u) => u,
        Err(_) => return true,
    };
    let host = match parsed.host_str() {
        Some(h) => h.to_lowercase(),
        None => return true,
    };
    if host == "localhost"
        || host == "127.0.0.1"
        || host == "::1"
        || host == "0.0.0.0"
    {
        return true;
    }
    if host.starts_with("10.") {
        return true;
    }
    if host.starts_with("192.168.") {
        return true;
    }
    if let Some(rest) = host.strip_prefix("172.") {
        if let Some((a, _)) = rest.split_once('.') {
            if let Ok(n) = a.parse::<u8>() {
                if (16..=31).contains(&n) {
                    return true;
                }
            }
        }
    }
    if host.starts_with("169.254.") {
        return true;
    }
    false
}

#[derive(Serialize)]
pub struct HttpOpenInfo {
    #[serde(rename = "sessionId")]
    session_id: String,
    status: u16,
    #[serde(default)]
    headers: HashMap<String, String>,
    #[serde(rename = "totalBytes")]
    total_bytes: Option<u64>,
}

fn gen_session_id() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    format!("dl-{:016x}", rng.gen::<u64>())
}

/// 建会话：reqwest GET（默认跟随重定向，透传请求头），流式拉取。
#[tauri::command]
pub async fn http_download_open(
    state: tauri::State<'_, DownloadState>,
    url: String,
    headers: Option<HashMap<String, String>>,
) -> Result<HttpOpenInfo, String> {
    if is_private_host(&url) {
        return Err(format!(
            "http_download blocked: private/internal host not allowed: {url}"
        ));
    }

    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .map_err(|e| format!("client build failed: {e}"))?;

    let mut builder = client.get(&url);
    if let Some(hs) = headers {
        for (k, v) in hs {
            let lk = k.to_lowercase();
            // host / content-length / accept-encoding 由 reqwest 自行控制
            if lk == "host" || lk == "content-length" || lk == "accept-encoding" {
                continue;
            }
            builder = builder.header(k, v);
        }
    }
    builder = builder.header("Accept-Encoding", "identity");
    builder = builder.header("User-Agent", USER_AGENT);

    let resp = builder
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    let status = resp.status().as_u16();
    let mut resp_headers = HashMap::new();
    for (k, v) in resp.headers().iter() {
        if let Ok(s) = v.to_str() {
            resp_headers.insert(k.as_str().to_string(), s.to_string());
        }
    }
    let total_bytes = resp.content_length();

    let session_id = gen_session_id();
    let (tx, rx) = mpsc::channel::<Result<Vec<u8>, String>>(CHANNEL_CAPACITY);
    state.0.lock().unwrap().insert(session_id.clone(), Some(rx));

    // spawn 读流推 channel；前端 close / 读完 drop rx 后 send 失败即退出。
    tauri::async_runtime::spawn(async move {
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(c) => {
                    if tx.send(Ok(c.to_vec())).await.is_err() {
                        break; // 消费端已关闭
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e.to_string())).await;
                    break;
                }
            }
        }
    });

    Ok(HttpOpenInfo {
        session_id,
        status,
        headers: resp_headers,
        total_bytes,
    })
}

/// 读下一块（凑够 ~size 字节或流结束）；返回原始字节，前端 invoke 收到 ArrayBuffer。
#[tauri::command]
pub async fn http_download_read(
    state: tauri::State<'_, DownloadState>,
    session_id: String,
    size: usize,
) -> Result<tauri::ipc::Response, String> {
    // 锁内 take 出 Receiver（MutexGuard 不能跨 await），处理完放回
    let mut rx = {
        let mut map = state.0.lock().unwrap();
        let slot = match map.get_mut(&session_id) {
            Some(s) => s,
            None => {
                // 会话不存在：可能上次 read 已读完整个流（小文件单块读完即清理）
                // 或被 close。幂等返回空块，前端按 byteLength == 0 收尾，避免
                // 「小文件下载被误判失败」（此前 506KB 的 es_pkg zip 即踩此坑）。
                return Ok(tauri::ipc::Response::new(Vec::new()));
            }
        };
        slot.take().ok_or_else(|| "http_download session busy".to_string())?
    };

    let chunk_size = size.clamp(64 * 1024, 8 * 1024 * 1024); // 64KB ~ 8MB
    let mut buf: Vec<u8> = Vec::with_capacity(chunk_size);
    let mut ended = false;
    while buf.len() < chunk_size {
        match rx.recv().await {
            Some(Ok(c)) => buf.extend_from_slice(&c),
            Some(Err(e)) => {
                let _ = rx.close();
                return Err(e);
            }
            None => {
                ended = true; // 流结束
                break;
            }
        }
    }

    // 流未结束则放回会话继续读；已结束则清理
    if ended {
        state.0.lock().unwrap().remove(&session_id);
    } else {
        let mut map = state.0.lock().unwrap();
        if let Some(slot) = map.get_mut(&session_id) {
            *slot = Some(rx);
        }
    }

    Ok(tauri::ipc::Response::new(buf))
}

/// 关闭会话（rx drop → spawn task 的 send 失败自动退出）。
#[tauri::command]
pub async fn http_download_close(
    state: tauri::State<'_, DownloadState>,
    session_id: String,
) -> Result<(), String> {
    state.0.lock().unwrap().remove(&session_id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_host_rejected() {
        assert!(is_private_host("http://localhost:3000/api"));
        assert!(is_private_host("http://192.168.1.10/x"));
        assert!(is_private_host("http://10.0.0.1/x"));
        assert!(is_private_host("http://172.16.5.1/x"));
        assert!(!is_private_host("https://www.chenddcoder.cn/x"));
        assert!(!is_private_host("https://run.quicktvui.com/x"));
        assert!(is_private_host("not a url"));
    }

    #[test]
    fn session_id_unique() {
        let a = gen_session_id();
        let b = gen_session_id();
        assert_ne!(a, b);
        assert!(a.starts_with("dl-"));
    }
}
