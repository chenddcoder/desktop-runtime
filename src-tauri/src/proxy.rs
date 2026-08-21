// 跨域 HTTP 代理：绕过浏览器 CORS，让 web-runtime 的 es_pkg 加载链路在桌面可用。
//
// 背景：
// web-runtime 的 es_pkg 加载（CrossAppResolver）需要访问 https://run.quicktvui.com
// 的 /api/app/resolve（POST octet-stream，非简单请求 → 触发 CORS 预检）。
// 该接口不返回 Access-Control-Allow-Origin，且桌面页面 origin 是 localhost:1420
// （或 tauri://localhost），浏览器预检失败 → 拦截实际请求 → getPackageInfo 抛错 →
// "无法加载应用：本地无缓存且服务器解析失败"。
//
// 而本地 web-cli dev 能跑通，是因为 DevServer 自带 /proxy 同域转发端点 + 全代理模式；
// 桌面用的是 production 构建的 web-runtime/dist，既无全代理、autoProxy 也不会把
// run.quicktvui.com 改写成 /proxy。所以单纯给静态服务器加 /proxy 没用。
//
// 修法：注入脚本（proxy_fetch.js）把到 run.quicktvui.com 的 fetch/XHR 转交给本命令，
// 用 reqwest 发出（不受同源策略约束），响应以 base64 回传。web-runtime 本体零改动。

use std::collections::HashMap;

use base64::engine::general_purpose::STANDARD as BASE64_STD;
use base64::Engine;
use serde::Deserialize;
use serde::Serialize;

#[derive(Deserialize)]
pub struct ProxyReq {
    url: String,
    method: String,
    #[serde(default)]
    headers: HashMap<String, String>,
    #[serde(default)]
    body_base64: Option<String>,
}

#[derive(Serialize)]
pub struct ProxyResp {
    status: u16,
    #[serde(default)]
    headers: HashMap<String, String>,
    #[serde(rename = "bodyBase64")]
    body_base64: String,
}

/// 判断 host 是否为私有/本地网段（防 SSRF / 内网探测）。
/// 与前端 proxy_fetch.js 的 isPrivateHost 保持一致；前端即便被绕过，
/// Rust 侧仍作为最终兜底拒绝内网请求。
fn is_private_host(url_str: &str) -> bool {
    let parsed = match url::Url::parse(url_str) {
        Ok(u) => u,
        Err(_) => return true, // 解析失败 → 保守拒绝
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
    // 172.16.0.0/12
    if let Some(rest) = host.strip_prefix("172.") {
        if let Some((a, _)) = rest.split_once('.') {
            if let Ok(n) = a.parse::<u8>() {
                if (16..=31).contains(&n) {
                    return true;
                }
            }
        }
    }
    // 169.254.0.0/16 link-local
    if host.starts_with("169.254.") {
        return true;
    }
    false
}

#[tauri::command]
pub async fn proxy_http(req: ProxyReq) -> Result<String, String> {
    // SSRF 兜底：拒绝到私有/本地网段的请求，绝不成为开放代理。
    if is_private_host(&req.url) {
        return Err(format!("proxy blocked: private/internal host not allowed: {}", req.url));
    }

    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| format!("client build failed: {e}"))?;

    let method = reqwest::Method::from_bytes(req.method.to_uppercase().as_bytes())
        .unwrap_or(reqwest::Method::GET);

    let mut builder = client.request(method, &req.url);

    for (k, v) in &req.headers {
        let lk = k.to_lowercase();
        // host / content-length 由 reqwest 自行计算；accept-encoding 强制 identity
        // 避免服务端返回 gzip 后把压缩字节直接丢给浏览器端解码。
        if lk == "host" || lk == "content-length" || lk == "accept-encoding" {
            continue;
        }
        builder = builder.header(k, v);
    }
    builder = builder.header("Accept-Encoding", "identity");
    builder = builder.header(
        "User-Agent",
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36",
    );

    if let Some(b64) = &req.body_base64 {
        let bytes = BASE64_STD
            .decode(b64)
            .map_err(|e| format!("body base64 decode failed: {e}"))?;
        builder = builder.body(bytes);
    }

    let resp = client
        .execute(builder.build().map_err(|e| format!("request build: {e}"))?)
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    let status = resp.status().as_u16();
    let mut headers = HashMap::new();
    for (k, v) in resp.headers().iter() {
        if let Ok(s) = v.to_str() {
            headers.insert(k.as_str().to_string(), s.to_string());
        }
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("read body failed: {e}"))?;
    let body_base64 = BASE64_STD.encode(&bytes);

    let out = ProxyResp {
        status,
        headers,
        body_base64,
    };
    serde_json::to_string(&out).map_err(|e| format!("serialize failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // 非法 base64 请求体应在解码阶段返回 Err（不发网络请求），
    // 验证 proxy_http 的参数校验路径。
    #[test]
    fn proxy_http_rejects_invalid_base64() {
        tauri::async_runtime::block_on(async {
            let req = ProxyReq {
                url: "https://example.com".to_string(),
                method: "GET".to_string(),
                headers: HashMap::new(),
                body_base64: Some("!!!not valid base64!!!".to_string()),
            };
            let res = proxy_http(req).await;
            assert!(res.is_err());
            assert!(res.unwrap_err().contains("base64"));
        });
    }

    // 验证 ProxyResp 序列化为 JSON 时使用 bodyBase64（前端 proxy_fetch.js 读取该字段）。
    #[test]
    fn proxy_resp_serializes_body_base64() {
        let out = ProxyResp {
            status: 200,
            headers: HashMap::new(),
            body_base64: "aGVsbG8=".to_string(),
        };
        let s = serde_json::to_string(&out).unwrap();
        assert!(s.contains("\"bodyBase64\""), "序列化应包含 bodyBase64: {s}");
    }
}
