import { socket } from '@kit.NetworkKit';
import { util } from '@kit.ArkTS';
import { hilog } from '@kit.PerformanceAnalysisKit';
import { resolveLanIp } from './NetInfo';

const DOMAIN = 0x0000;
const TAG = 'ToupingEndpoint';

/**
 * 桌面端 `endpoint.rs` 的鸿蒙等价物 —— 补 Android 插件
 * `com.quicktvui.plugin.endpoint` 的能力（留言板 host 模式 / 大屏接手机）。
 *
 * 语义逐条对齐插件与桌面端实现，改任何一条都会让电视端与手机端漂移：
 *   endpoint_start {port?}  → {success, httpPort, ip, deviceUuid}
 *   endpoint_stop           → bool
 *   endpoint_info           → info | null
 *   endpoint_reply {requestId, status, body, contentType?} → bool
 *   收到请求 → 事件 `endpoint://request`
 *     { requestId, method, path, headers(JSON 数组字符串), body }
 *
 * 三条硬约束（照抄桌面端踩过的坑）：
 *   1. path 必须带 query（手机端 ?since= / ?room= 全靠它）
 *   2. 业务不在这里做，原样交给页面算，5s 不回就 504 并摘掉 pending
 *   3. 端口首选 39200（手机 core/pair.js 的 TV_PORT），被占顺延 40 个；
 *      二维码里的端口要用实际端口
 */

const MAX_BODY = 8 * 1024 * 1024;
const REPLY_TIMEOUT_MS = 5000;
const PREFERRED_PORT = 39200;
const PORT_SPAN = 40;

interface StartArgs {
  port?: number;
}

interface ReplyArgs {
  requestId?: number;
  status?: number;
  body?: string;
  contentType?: string;
}

class EndpointInfo {
  success: boolean = true;
  httpPort: number = 0;
  ip: string = '';
  deviceUuid: string = '';

  constructor(httpPort: number, ip: string, deviceUuid: string) {
    this.httpPort = httpPort;
    this.ip = ip;
    this.deviceUuid = deviceUuid;
  }
}

class Pending {
  done: boolean = false;
  timer: number = -1;
  respond: (status: number, body: string, contentType: string) => void =
    () => {};
}

class ConnState {
  buf: Uint8Array = new Uint8Array(0);
  answered: boolean = false;
}

function reason(status: number): string {
  switch (status) {
    case 200: return 'OK';
    case 201: return 'Created';
    case 204: return 'No Content';
    case 400: return 'Bad Request';
    case 403: return 'Forbidden';
    case 404: return 'Not Found';
    case 405: return 'Method Not Allowed';
    case 500: return 'Internal Server Error';
    case 503: return 'Service Unavailable';
    case 504: return 'Gateway Timeout';
    default: return 'OK';
  }
}

function concat(a: Uint8Array, b: Uint8Array): Uint8Array {
  const out = new Uint8Array(a.byteLength + b.byteLength);
  out.set(a, 0);
  out.set(b, a.byteLength);
  return out;
}

function indexOfCrlfCrlf(buf: Uint8Array): number {
  for (let i = 0; i + 3 < buf.byteLength; i++) {
    if (buf[i] === 13 && buf[i + 1] === 10 && buf[i + 2] === 13 && buf[i + 3] === 10) {
      return i;
    }
  }
  return -1;
}

export class EndpointServer {
  /** 页面侧事件出口：由 Index 注入（runJavaScript → __tauriHostEmit） */
  emit: (name: string, payloadJson: string) => void = () => {};

  private server: socket.TCPSocketServer | undefined = undefined;
  private info: EndpointInfo | undefined = undefined;
  private pending: Map<number, Pending> = new Map<number, Pending>();
  private nextId: number = 1;

  async start(argsJson: string): Promise<string> {
    if (this.info) {
      return JSON.stringify(this.info);
    }
    let startPort = PREFERRED_PORT;
    try {
      const a = JSON.parse(argsJson) as StartArgs;
      if (a && a.port && a.port > 0 && a.port <= 65535) {
        startPort = a.port;
      }
    } catch (e) {
      // 参数坏了就用默认端口
    }

    const srv = socket.constructTCPSocketServerInstance();
    let bound = 0;
    let lastErr = '';
    for (let i = 0; i < PORT_SPAN; i++) {
      const p = startPort + i;
      if (p > 65535) {
        break;
      }
      try {
        await srv.listen({ address: '0.0.0.0', port: p, family: 1 });
        bound = p;
        break;
      } catch (e) {
        const err = e as Error;
        lastErr = err.message;
      }
    }
    if (bound === 0) {
      throw new Error(`没有可用端口（${startPort}-${startPort + PORT_SPAN - 1} 全被占）：${lastErr}`);
    }

    srv.on('connect', (client: socket.TCPSocketConnection) => {
      this.handleClient(client);
    });

    this.server = srv;
    const ip = await resolveLanIp();
    this.info = new EndpointInfo(bound, ip, `harmony-${ip}-${bound}`);
    hilog.info(DOMAIN, TAG, 'endpoint started on %{public}d (%{public}s)', bound, ip);
    return JSON.stringify(this.info);
  }

  stop(): boolean {
    this.pending.forEach((p: Pending) => {
      if (p.timer >= 0) {
        clearTimeout(p.timer);
      }
      p.timer = -1;
    });
    this.pending.clear();
    if (this.server) {
      try {
        this.server.off('connect');
      } catch (e) {
        // 忽略
      }
      this.server = undefined;
      this.info = undefined;
      return true;
    }
    return false;
  }

  infoJson(): string {
    return this.info ? JSON.stringify(this.info) : 'null';
  }

  reply(argsJson: string): boolean {
    let args: ReplyArgs;
    try {
      args = JSON.parse(argsJson) as ReplyArgs;
    } catch (e) {
      return false;
    }
    const id = args.requestId ? args.requestId : 0;
    const p = this.pending.get(id);
    if (!p) {
      // 请求号已超时摘掉：与插件一致，静默丢弃（客户端早已拿到 504）
      return false;
    }
    this.pending.delete(id);
    if (p.timer >= 0) {
      clearTimeout(p.timer);
    }
    p.timer = -1;
    if (!p.done) {
      p.done = true;
      const ct = args.contentType ? args.contentType : 'application/json; charset=utf-8';
      p.respond(args.status ? args.status : 200, args.body ? args.body : '', ct);
    }
    return true;
  }

  /* ---------------- 连接处理 ---------------- */

  private handleClient(client: socket.TCPSocketConnection): void {
    const st = new ConnState();
    client.on('message', (value: socket.SocketMessageInfo) => {
      const chunk = new Uint8Array(value.message);
      st.buf = concat(st.buf, chunk);
      if (st.buf.byteLength > 64 * 1024 + MAX_BODY) {
        this.closeClient(client);
        return;
      }
      this.tryDispatch(client, st);
    });
    client.on('close', () => {
      // 客户端提前断开：迟到响应无处可写，直接丢
    });
  }

  private tryDispatch(client: socket.TCPSocketConnection, st: ConnState): void {
    if (st.answered) {
      return;
    }
    const headEnd = indexOfCrlfCrlf(st.buf);
    if (headEnd < 0) {
      return;
    }
    const headBytes = st.buf.subarray(0, headEnd);
    const head = new util.TextDecoder('utf-8').decodeWithStream(headBytes);
    const lines = head.split('\r\n');
    if (lines.length === 0) {
      return;
    }
    const first = lines[0].split(/\s+/);
    if (first.length < 2) {
      return;
    }
    const method = first[0].toUpperCase();
    const path = first[1]; // ⛔ 保留 query

    const headerNames: string[] = [];
    const headerValues: string[] = [];
    let contentLength = 0;
    for (let i = 1; i < lines.length; i++) {
      const line = lines[i];
      const colon = line.indexOf(':');
      if (colon <= 0) {
        continue;
      }
      const k = line.substring(0, colon).trim().toLowerCase();
      const v = line.substring(colon + 1).trim();
      headerNames.push(k);
      headerValues.push(v);
      if (k === 'content-length') {
        const n = Number(v);
        if (Number.isFinite(n) && n > 0) {
          contentLength = Math.min(n, MAX_BODY);
        }
      }
    }

    const bodyStart = headEnd + 4;
    if (st.buf.byteLength - bodyStart < contentLength) {
      return; // 还没收全
    }
    st.answered = true;
    const bodyBytes = st.buf.subarray(bodyStart, bodyStart + contentLength);
    const body = bodyBytes.byteLength > 0 ? new util.TextDecoder('utf-8').decodeWithStream(bodyBytes) : '';

    // 预检直接回，不进页面（页面没有 OPTIONS 路由）
    if (method === 'OPTIONS') {
      this.writeResponse(client, 204, 'text/plain', '');
      return;
    }

    const id = this.nextId;
    this.nextId += 1;

    const pairs: string[] = [];
    for (let i = 0; i < headerNames.length; i++) {
      pairs.push(`["${headerNames[i]}","${headerValues[i].replace(/"/g, '\\"')}"]`);
    }
    const headersJson = `[${pairs.join(',')}]`;

    const payload = `{"requestId":${id},"method":${JSON.stringify(method)},"path":${JSON.stringify(path)},"headers":${JSON.stringify(headersJson)},"body":${JSON.stringify(body)}}`;

    const pending = new Pending();
    pending.respond = (status: number, respBody: string, contentType: string) => {
      this.writeResponse(client, status, contentType, respBody);
    };
    this.pending.set(id, pending);
    pending.timer = setTimeout(() => {
      const p = this.pending.get(id);
      if (p) {
        this.pending.delete(id);
        p.done = true;
        this.writeResponse(client, 504, 'application/json; charset=utf-8',
          '{"ok":false,"error":"js handler timeout"}');
      }
    }, REPLY_TIMEOUT_MS);

    try {
      this.emit('endpoint://request', payload);
    } catch (e) {
      const err = e as Error;
      this.pending.delete(id);
      clearTimeout(pending.timer);
      this.writeResponse(client, 500, 'application/json; charset=utf-8',
        `{"ok":false,"error":"emit failed: ${err.message}"}`);
    }
  }

  private writeResponse(client: socket.TCPSocketConnection, status: number, contentType: string, body: string): void {
    const bodyBytes = new util.TextEncoder().encodeInto(body);
    const head = `HTTP/1.1 ${status} ${reason(status)}\r\n` +
      `Content-Type: ${contentType}\r\n` +
      `Content-Length: ${bodyBytes.byteLength}\r\n` +
      'Access-Control-Allow-Origin: *\r\n' +
      'Access-Control-Allow-Headers: *\r\n' +
      'Access-Control-Allow-Methods: GET,POST,PUT,DELETE,OPTIONS\r\n' +
      'Cache-Control: no-store\r\n' +
      'Connection: close\r\n\r\n';
    const headBytes = new util.TextEncoder().encodeInto(head);
    const all = concat(headBytes, bodyBytes);
    try {
      client.send({ data: all.buffer.slice(all.byteOffset, all.byteOffset + all.byteLength) as ArrayBuffer })
        .then(() => this.closeClient(client))
        .catch((e: Error) => {
          hilog.warn(DOMAIN, TAG, 'send failed: %{public}s', e.message);
          this.closeClient(client);
        });
    } catch (e) {
      this.closeClient(client);
    }
  }

  private closeClient(client: socket.TCPSocketConnection): void {
    try {
      client.close();
    } catch (e) {
      // 已关闭
    }
  }
}
