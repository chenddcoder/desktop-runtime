// 零依赖静态文件服务器（跨平台，Node 内置模块）。
// 用法：node scripts/serve.mjs <port> <distDir>
// 开发态 Tauri 通过 devUrl(http://localhost:<port>) 加载 web-runtime/dist，
// 与生产态 tauri://localhost 行为接近，且绕开 file:// 的 CORS 限制。
//
// 额外承担 /proxy 端点：把 web-runtime 的 autoProxy 改写后的跨域请求
//   <origin>/proxy?url=<encoded-upstream>
// 转发到真实上游（method/headers/body 透传），从而代理 fetch/XHR/img/link/script
// 等所有通过浏览器自身加载的资源（这些是 proxy_fetch.js 覆盖不到的）
//
// SSRF 防护：禁止代理到 localhost / 127.0.0.1 / 私有网段。
import { createServer } from "node:http";
import { readFile, stat } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { dirname, join, extname, normalize } from "node:path";

const __dirname = dirname(fileURLToPath(import.meta.url));
const port = Number(process.argv[2] || "1420");
// 默认按脚本自身位置解析 dist（cwd 无关，避免 Tauri beforeDevCommand 工作目录歧义）；
// 如需覆盖可传第三个参数（此时相对 cwd）。
const argDist = process.argv[3];
const distDir = normalize(
  argDist
    ? argDist
    : join(__dirname, "..", "..", "quicktvui", "packages", "web-runtime", "dist")
);

const MIME = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".mjs": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".json": "application/json; charset=utf-8",
  ".wasm": "application/wasm",
  ".png": "image/png",
  ".jpg": "image/jpeg",
  ".svg": "image/svg+xml",
  ".ico": "image/x-icon",
  ".map": "application/json; charset=utf-8",
  ".gz": "application/gzip",
};

// 不应转发到上游的 hop-by-hop 与本机身份相关的请求头（避免把 dev server 的身份暴露给公网）
const HOP_BY_HOP_HEADERS = new Set([
  "host",
  "connection",
  "keep-alive",
  "proxy-authenticate",
  "proxy-authorization",
  "te",
  "trailers",
  "transfer-encoding",
  "upgrade",
  // 浏览器在 cross-origin fetch 时会带 origin/referer；这些若是 localhost:1420 也不该发给上游
  "origin",
  "referer",
]);

// SSRF 防护：禁止代理到本地 / 内网。环境变量 ALLOW_PRIVATE_HOSTS=true
// 可放宽（如测试场景代理到本地 mock 上游）。线上永远不要设这个。
const ALLOW_PRIVATE_HOSTS = process.env.ALLOW_PRIVATE_HOSTS === "true";

function isPrivateHost(host) {
  if (!host) return true;
  host = host.toLowerCase();
  if (host === "localhost" || host === "127.0.0.1" || host === "::1" || host === "0.0.0.0") return true;
  if (/^10\./.test(host)) return true;
  if (/^192\.168\./.test(host)) return true;
  if (/^172\.(1[6-9]|2[0-9]|3[01])\./.test(host)) return true;
  if (/^169\.254\./.test(host)) return true; // link-local
  return false;
}

async function readRequestBody(req) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    let total = 0;
    const MAX = 64 * 1024 * 1024; // 64MB 兜底；超过则拒绝（防止内存压力）
    req.on("data", (c) => {
      total += c.length;
      if (total > MAX) {
        reject(new Error("payload too large"));
        req.destroy();
        return;
      }
      chunks.push(c);
    });
    req.on("end", () => resolve(Buffer.concat(chunks)));
    req.on("error", reject);
  });
}

function writeProxyError(res, status, msg) {
  try {
    res.writeHead(status, { "Content-Type": "text/plain; charset=utf-8", "Access-Control-Allow-Origin": "*" });
    res.end(msg);
  } catch (_) {}
}

/**
 * 处理 /proxy 转发请求。
 *
 * 请求形态：<scheme>://<host>/proxy?url=<encoded-upstream-url>
 * 转发：method/headers/body 透传到 upstream，response 流回（status/headers/body 原样）。
 * 失败：返回 4xx/5xx + 简短文本（让调用方能感知原因，避免误以为成功）。
 */
async function handleProxy(req, res) {
  // CORS preflight：autoProxy 在 Content-Type:application/octet-stream 时会发 OPTIONS
  if (req.method === "OPTIONS") {
    res.writeHead(204, {
      "Access-Control-Allow-Origin": "*",
      "Access-Control-Allow-Methods": "GET, POST, PUT, DELETE, OPTIONS",
      "Access-Control-Allow-Headers": req.headers["access-control-request-headers"] || "*",
      "Access-Control-Max-Age": "86400",
    });
    res.end();
    return;
  }

  // 解析 ?url=
  let target;
  try {
    const u = new URL(req.url, `http://${req.headers.host || "localhost"}`);
    target = u.searchParams.get("url");
  } catch (_) {}
  if (!target) {
    writeProxyError(res, 400, "missing url query param");
    return;
  }

  let parsedTarget;
  try {
    parsedTarget = new URL(target);
  } catch (_) {
    writeProxyError(res, 400, "invalid target url");
    return;
  }

  if (parsedTarget.protocol !== "http:" && parsedTarget.protocol !== "https:") {
    writeProxyError(res, 400, "only http/https protocols allowed");
    return;
  }

  if (!ALLOW_PRIVATE_HOSTS && isPrivateHost(parsedTarget.hostname)) {
    writeProxyError(res, 403, "forbidden (private host)");
    return;
  }

  // 构造转发 headers（剥离 hop-by-hop 与本机身份相关的头）
  const fwdHeaders = {};
  for (const [k, v] of Object.entries(req.headers)) {
    if (HOP_BY_HOP_HEADERS.has(k.toLowerCase())) continue;
    fwdHeaders[k] = Array.isArray(v) ? v.join(", ") : v;
  }

  const init = {
    method: req.method,
    headers: fwdHeaders,
    redirect: "follow",
  };

  // GET/HEAD/DELETE 不带 body，其它方法尝试读取 body
  if (req.method !== "GET" && req.method !== "HEAD" && req.method !== "DELETE") {
    try {
      const bodyBuf = await readRequestBody(req);
      if (bodyBuf.length > 0) init.body = bodyBuf;
    } catch (e) {
      writeProxyError(res, 413, "payload too large or read failed: " + e.message);
      return;
    }
  }

  let upstream;
  try {
    upstream = await fetch(parsedTarget.toString(), init);
  } catch (e) {
    writeProxyError(res, 502, "upstream fetch failed: " + (e && e.message || e));
    return;
  }

  // 复制 upstream headers（剥离响应里的 hop-by-hop 头）
  const outHeaders = {};
  upstream.headers.forEach((v, k) => {
    const kl = k.toLowerCase();
    if (kl === "transfer-encoding" || kl === "connection" || kl === "keep-alive") return;
    outHeaders[k] = v;
  });
  outHeaders["Access-Control-Allow-Origin"] = "*";

  try {
    res.writeHead(upstream.status, outHeaders);
  } catch (_) {
    return; // 客户端已断开，写头失败
  }

  if (!upstream.body) {
    res.end();
    return;
  }

  // 流式回写（Node 18+ fetch 返回 web ReadableStream）
  try {
    const reader = upstream.body.getReader();
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      if (!res.write(Buffer.from(value))) {
        // 内核写缓冲满了，等 drain 再继续（避免内存暴涨）
        await new Promise((r) => res.once("drain", r));
      }
    }
    res.end();
  } catch (e) {
    try {
      res.destroy(e);
    } catch (_) {}
  }
}

const server = createServer(async (req, res) => {
  try {
    const urlPath = decodeURIComponent((req.url || "/").split("?")[0]);

    // /proxy 是 web-runtime autoProxy 的标准中转端点；处理优先级高于静态文件 fallback
    if (urlPath === "/proxy") {
      await handleProxy(req, res);
      return;
    }

    let filePath = join(distDir, urlPath === "/" ? "index.html" : urlPath);
    // 防目录穿越
    if (!filePath.startsWith(distDir)) {
      res.writeHead(403).end("forbidden");
      return;
    }
    let info;
    try {
      info = await stat(filePath);
    } catch {
      // SPA 兜底：回退到 index.html
      filePath = join(distDir, "index.html");
    }
    if (info && info.isDirectory()) {
      filePath = join(filePath, "index.html");
    }
    const data = await readFile(filePath);
    const type = MIME[extname(filePath)] || "application/octet-stream";
    res.writeHead(200, { "Content-Type": type, "Access-Control-Allow-Origin": "*" });
    res.end(data);
  } catch (e) {
    res.writeHead(500).end(String(e));
  }
});

server.listen(port, () => {
  // 端口 0 表示由 OS 分配；这里取真实端口号打印，避免调用方误以为 0 是端口。
  const actualPort = server.address().port;
  console.log(`[serve] web-runtime/dist -> http://localhost:${actualPort}  (dir: ${distDir})`);
});
