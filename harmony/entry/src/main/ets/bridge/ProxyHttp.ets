import { http } from '@kit.NetworkKit';
import { util } from '@kit.ArkTS';
import { hilog } from '@kit.PerformanceAnalysisKit';

const DOMAIN = 0x0000;
const TAG = 'ToupingProxy';

/**
 * 跨域 HTTP 代理（`proxy_http`）的鸿蒙实现 —— 桌面端走 Rust + reqwest，这里走系统 HTTP 栈。
 *
 * 为什么不用 Rust 复刻：鸿蒙的系统证书链、代理设置、企业 CA 都在 `@ohos.net.http` 里，
 * 自建 TLS 客户端（rustls/ring 对 ohos 目标支持不稳）反而更差；而这个命令的语义就是
 * 「替页面发一个它自己发不了的请求」，系统栈最合适。
 *
 * 语义必须与桌面端 `proxy.rs` 完全一致（页面只认这套）：
 *   入参 `{url, method, headers, bodyBase64?}`
 *   出参 `{status, headers, bodyBase64}`，以 JSON 字符串返回
 * ⛔ SSRF 兜底同样要留：拒绝私有/本地网段，不做开放代理。
 */

interface ProxyReq {
  url: string;
  method: string;
  headers: Record<string, string>;
  bodyBase64?: string;
}

interface ProxyResp {
  status: number;
  headers: Record<string, string>;
  bodyBase64: string;
}

function hostOf(urlStr: string): string {
  // 只取 scheme 之后、第一个 / ? # 之前的部分，再去掉 userinfo 与端口
  const m = urlStr.match(/^[a-zA-Z][a-zA-Z0-9+.-]*:\/\/([^/?#]*)/);
  if (!m || m.length < 2) {
    return '';
  }
  let authority: string = m[1];
  const at = authority.lastIndexOf('@');
  if (at >= 0) {
    authority = authority.substring(at + 1);
  }
  // IPv6 字面量 [::1]:8080
  if (authority.startsWith('[')) {
    const end = authority.indexOf(']');
    return end > 0 ? authority.substring(1, end).toLowerCase() : authority.toLowerCase();
  }
  const colon = authority.indexOf(':');
  return (colon >= 0 ? authority.substring(0, colon) : authority).toLowerCase();
}

/** 与 proxy.rs 的 `is_private_host` 保持一致；解析不出 host 一律当私有（保守拒绝） */
export function isPrivateHost(urlStr: string): boolean {
  const host = hostOf(urlStr);
  if (!host) {
    return true;
  }
  if (host === 'localhost' || host === '127.0.0.1' || host === '::1' || host === '0.0.0.0') {
    return true;
  }
  if (host.startsWith('10.') || host.startsWith('192.168.') || host.startsWith('169.254.')) {
    return true;
  }
  if (host.startsWith('172.')) {
    const rest = host.substring(4);
    const dot = rest.indexOf('.');
    const second = Number(dot > 0 ? rest.substring(0, dot) : rest);
    if (second >= 16 && second <= 31) {
      return true;
    }
  }
  return false;
}

function methodOf(name: string): http.RequestMethod {
  switch (name.toUpperCase()) {
    case 'POST':
      return http.RequestMethod.POST;
    case 'PUT':
      return http.RequestMethod.PUT;
    case 'DELETE':
      return http.RequestMethod.DELETE;
    case 'HEAD':
      return http.RequestMethod.HEAD;
    case 'OPTIONS':
      return http.RequestMethod.OPTIONS;
    case 'TRACE':
      return http.RequestMethod.TRACE;
    case 'CONNECT':
      return http.RequestMethod.CONNECT;
    default:
      // ⛔ 系统枚举里没有 PATCH：es_pkg 解析与卡带下载都用不到它，
      // 真遇到就按 GET 发，别声称自己支持了 PATCH。
      return http.RequestMethod.GET;
  }
}

function headersToRecord(raw: Object | undefined): Record<string, string> {
  const out: Record<string, string> = {};
  if (!raw) {
    return out;
  }
  try {
    const src = raw as Record<string, string>;
    const keys = Object.keys(src);
    for (const k of keys) {
      const v = src[k];
      out[k.toLowerCase()] = v === undefined || v === null ? '' : String(v);
    }
  } catch (e) {
    // 拿不到就当空头，别让一次取头失败把整个代理打挂
  }
  return out;
}

/**
 * 发一个跨域请求。返回 `{status, headers, bodyBase64}` 的 JSON 字符串。
 * 任何失败都抛 Error，由 InvokeRouter 转成 invoke 的 reject。
 */
export async function proxyHttp(argsJson: string): Promise<string> {
  const req = JSON.parse(argsJson) as ProxyReq;
  const url = String(req.url || '');
  if (!url) {
    throw new Error('proxy_http: url is empty');
  }
  // SSRF 兜底：与桌面端一致，永不成为开放代理
  if (isPrivateHost(url)) {
    throw new Error(`proxy blocked: private/internal host not allowed: ${url}`);
  }

  // ⛔ 裸 `{}` 会被 ArkTS 判成 untyped object literal，必须显式标注类型
  const headerOut: Record<string, string> = req.headers ? req.headers : {};
  const options: http.HttpRequestOptions = {
    method: methodOf(String(req.method || 'GET')),
    header: headerOut,
    expectDataType: http.HttpDataType.ARRAY_BUFFER,
    connectTimeout: 15000,
    readTimeout: 30000,
    usingCache: false
  };

  if (req.bodyBase64) {
    const helper = new util.Base64Helper();
    const bytes = helper.decodeSync(req.bodyBase64);
    // ArrayBuffer 是 http 模块唯一能「原样上传」的二进制形态（string 会被当表单文本）
    options.extraData = bytes.buffer as ArrayBuffer;
  }

  const client = http.createHttp();
  try {
    const resp = await client.request(url, options);
    let body: ArrayBuffer = new ArrayBuffer(0);
    if (resp.result instanceof ArrayBuffer) {
      body = resp.result;
    } else if (resp.result !== undefined && resp.result !== null) {
      // 明确要了 ARRAY_BUFFER 却回了字符串：按 UTF-8 编码，别让调用方拿到空体
      const text = String(resp.result);
      const buf = new util.TextEncoder().encodeInto(text);
      body = buf.buffer as ArrayBuffer;
    }
    const out: ProxyResp = {
      status: resp.responseCode,
      headers: headersToRecord(resp.header as Object),
      bodyBase64: new util.Base64Helper().encodeToStringSync(new Uint8Array(body))
    };
    return JSON.stringify(out);
  } finally {
    // 每次 invoke 一个 client：keep-alive 复用在 es_pkg 首屏是负收益（并发少、连接状态难清）
    client.destroy();
  }
}

export function proxyLog(msg: string): void {
  hilog.debug(DOMAIN, TAG, '%{public}s', msg);
}
