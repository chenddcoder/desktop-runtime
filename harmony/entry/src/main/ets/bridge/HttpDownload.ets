import { http } from '@kit.NetworkKit';
import { util } from '@kit.ArkTS';
import { hilog } from '@kit.PerformanceAnalysisKit';
import { isPrivateHost } from './ProxyHttp';

const DOMAIN = 0x0000;
const TAG = 'ToupingDownload';

/**
 * `http_download_open/read/close` 的鸿蒙实现 —— 桌面端走 Rust reqwest 流式下载，
 * 这里用系统 HTTP 栈一次性取回再按块吐给页面。
 *
 * 前端（web-renderer/src/core/tauriEnv.js 的 tauriDownload）只认这套协议：
 *   open  {url, headers}       → {sessionId, totalBytes, status, headers}
 *   read  {sessionId, size}    → base64 字符串；没有更多数据时返回空串
 *   close {sessionId}          → null
 *
 * ⛔ 为什么不是真流式：ArkTS 的 `requestInStream` 要等 dataEnd 才拿得到响应码，
 * 而我们必须在 open 里就把状态码交出去（前端据此判断 2xx）。快应用包目前是
 * 百 KB ~ 几 MB 级，整包驻留内存的代价远小于「响应码后置」带来的正确性风险。
 * 真遇到几十 MB 的包再换成 dataReceive 队列 + 轮询 read。
 */

const OPEN_TIMEOUT_CONNECT = 15000;
const OPEN_TIMEOUT_READ = 300000;

interface OpenArgs {
  url?: string;
  headers?: Record<string, string>;
  req?: OpenArgs;
}

interface ReadArgs {
  sessionId?: string;
  size?: number;
}

class Session {
  data: Uint8Array = new Uint8Array(0);
  offset: number = 0;

  constructor(data: Uint8Array) {
    this.data = data;
  }
}

class OpenResult {
  sessionId: string = '';
  totalBytes: number = -1;
  status: number = 0;
  headers: Record<string, string> = {};

  constructor(sessionId: string, totalBytes: number, status: number, headers: Record<string, string>) {
    this.sessionId = sessionId;
    this.totalBytes = totalBytes;
    this.status = status;
    this.headers = headers;
  }
}

const SESSIONS: Map<string, Session> = new Map<string, Session>();
let seq: number = 0;

function normalizeHeaders(raw: Object | undefined): Record<string, string> {
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
    // 取头失败不影响下载
  }
  return out;
}

export async function httpDownloadOpen(argsJson: string): Promise<string> {
  const parsed = JSON.parse(argsJson) as OpenArgs;
  const inner: OpenArgs | undefined = parsed.req;
  const url = String((inner ? inner.url : parsed.url) || '');
  if (!url) {
    throw new Error('http_download_open: url is empty');
  }
  if (isPrivateHost(url)) {
    throw new Error(`download blocked: private/internal host not allowed: ${url}`);
  }

  const rawHeaders: Object | undefined = inner ? inner.headers : parsed.headers;
  const options: http.HttpRequestOptions = {
    method: http.RequestMethod.GET,
    header: normalizeHeaders(rawHeaders),
    expectDataType: http.HttpDataType.ARRAY_BUFFER,
    connectTimeout: OPEN_TIMEOUT_CONNECT,
    readTimeout: OPEN_TIMEOUT_READ,
    usingCache: false
  };

  const client = http.createHttp();
  try {
    const resp = await client.request(url, options);
    let body: ArrayBuffer = new ArrayBuffer(0);
    if (resp.result instanceof ArrayBuffer) {
      body = resp.result;
    } else if (resp.result !== undefined && resp.result !== null) {
      const text = String(resp.result);
      const buf = new util.TextEncoder().encodeInto(text);
      body = buf.buffer.slice(buf.byteOffset, buf.byteOffset + buf.byteLength) as ArrayBuffer;
    }

    seq += 1;
    const sessionId = `dl${seq}-${Date.now()}`;
    SESSIONS.set(sessionId, new Session(new Uint8Array(body)));

    const out = new OpenResult(
      sessionId,
      body.byteLength,
      resp.responseCode,
      normalizeHeaders(resp.header as Object)
    );
    hilog.debug(DOMAIN, TAG, 'open %{public}s status=%{public}d size=%{public}d', url, out.status, out.totalBytes);
    return JSON.stringify(out);
  } catch (e) {
    const err = e as Error;
    hilog.error(DOMAIN, TAG, 'open failed: %{public}s', err.message);
    throw new Error(`http_download_open failed: ${err.message}`);
  } finally {
    client.destroy();
  }
}

export function httpDownloadRead(argsJson: string): string {
  const parsed = JSON.parse(argsJson) as ReadArgs;
  const sessionId = String(parsed.sessionId || '');
  const size = parsed.size && parsed.size > 0 ? Math.floor(parsed.size) : 1024 * 1024;
  const s = sessionId ? SESSIONS.get(sessionId) : undefined;
  if (!s) {
    throw new Error(`http_download_read: unknown session ${sessionId}`);
  }
  if (s.offset >= s.data.byteLength) {
    return ''; // 空串 = 读完；前端据此 break（返回 null 会被 shim 序列化成字符串 "null"）
  }
  const end = Math.min(s.offset + size, s.data.byteLength);
  const slice = s.data.subarray(s.offset, end);
  s.offset = end;
  return new util.Base64Helper().encodeToStringSync(slice);
}

export function httpDownloadClose(argsJson: string): string {
  const parsed = JSON.parse(argsJson) as ReadArgs;
  const sessionId = String(parsed.sessionId || '');
  if (sessionId) {
    SESSIONS.delete(sessionId);
  }
  return 'null';
}
