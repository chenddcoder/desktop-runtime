import { hilog } from '@kit.PerformanceAnalysisKit';
import { NapiBridge } from './NapiBridge';
import { proxyHttp } from './ProxyHttp';
import { httpDownloadClose, httpDownloadOpen, httpDownloadRead } from './HttpDownload';
import { EndpointServer } from './EndpointServer';
import { PadStats, PadUdp } from './PadUdp';

const DOMAIN = 0x0000;
const TAG = 'ToupingRouter';

/**
 * invoke 结果：与 Tauri 的 invoke 语义对齐
 *   ok=true  -> value 为命令返回值的 JSON 字符串
 *   ok=false -> error 为错误文本
 */
export interface InvokeResult {
  ok: boolean;
  value: string;
  error: string;
}

/** bridge_ping 的返回体（ArkTS 不允许无类型对象字面量，必须显式声明） */
class PingResult {
  pong: boolean = true;
  platform: string = 'harmonyos';
  runtime: string = 'arkweb';
}

class VersionResult {
  version: string = '1.0.0';
  shell: string = 'harmony';
}

class EmptyArgs {
  port: number = 0;
}

function ok(value: string): InvokeResult {
  return { ok: true, value: value, error: '' };
}

function fail(error: string): InvokeResult {
  return { ok: false, value: '', error: error };
}

/**
 * Rust 桥返回的 JSON 里，错误统一是 `{"__error__":"..."}`。
 * 这里把它翻成 invoke 的 reject —— 页面要能像桌面端一样 try/catch 拿到真实原因，
 * 而不是拿到一个"成功但内容是错误"的对象。
 */
function fromRust(json: string): InvokeResult {
  if (!json) {
    return fail('bridge returned empty');
  }
  if (json.indexOf('"__error__"') >= 0) {
    try {
      const obj = JSON.parse(json) as Record<string, string>;
      const msg = obj['__error__'];
      if (msg !== undefined && msg !== null && msg.length > 0) {
        return fail(msg);
      }
    } catch (e) {
      // 解析失败就把原文丢回去，比吞掉强
    }
    return fail(json);
  }
  return ok(json);
}

function portOf(argsJson: string): number {
  try {
    const a = JSON.parse(argsJson) as EmptyArgs;
    const p = Number(a.port);
    return Number.isFinite(p) && p > 0 && p <= 65535 ? p : 0;
  } catch (e) {
    return 0;
  }
}

/**
 * 命令路由：把 web-runtime 前端发来的 invoke(cmd, args) 分发到具体实现。
 *
 * 设计原则（与桌面端对齐）：
 *   1. 一个命令要么明确实现，要么返回「未实现」结构化错误 —— 不允许静默成功，
 *      否则前端拿到 undefined 会当成成功，问题会被掩盖。
 *   2. 实现分三类：
 *      - ArkTS 原生能力（proxy_http / 手机手柄 UDP / 退出）—— 直接用系统 Kit
 *      - Rust 复用（WebDAV 服务端、落盘读取）—— 走 libquickapp_bridge.so
 *      - 尚未实现（DLNA 组播 / endpoint 服务端 / 流式下载）—— 显式 `__unsupported__`
 */
export class InvokeRouter {
  private pad: PadUdp;
  /** 局域网自起 HTTP 服务（留言板 host 模式）：桌面端 endpoint.rs 的鸿蒙等价物 */
  private endpoint: EndpointServer = new EndpointServer();
  /** 退出应用（由页面注入，ArkTS 里要用 UIAbilityContext.terminateSelf） */
  onExit: () => void = () => {};

  constructor(pad: PadUdp) {
    this.pad = pad;
  }

  /** 页面侧事件出口（endpoint://request 等），由 Index 注入 runJavaScript */
  setHostEmit(fn: (name: string, payloadJson: string) => void): void {
    this.endpoint.emit = fn;
  }

  async invoke(cmd: string, argsJson: string): Promise<InvokeResult> {
    hilog.debug(DOMAIN, TAG, 'invoke %{public}s args=%{public}s', cmd, argsJson);
    switch (cmd) {
      case 'bridge_ping':
        return ok(JSON.stringify(new PingResult()));
      case 'bridge_version':
        return ok(JSON.stringify(new VersionResult()));
      // Rust 桥（libquickapp_bridge.so）存活探测。ArkTS 侧只做转发，实现见 harmony-bridge/src/lib.rs。
      case 'bridge_rust_ping':
        return ok(NapiBridge.ping(argsJson));
      case 'bridge_rust_info':
        return ok(NapiBridge.info());

      /* ---------------- 应用生命周期 ---------------- */
      case 'app_exit':
        this.onExit();
        return ok('null');

      /* ---------------- 跨域 HTTP 代理 ---------------- */
      // es_pkg 解析、卡带下载、所有被 CORS 挡住的请求都走它
      case 'proxy_http':
        try {
          return ok(await proxyHttp(argsJson));
        } catch (e) {
          const err = e as Error;
          return fail(err.message);
        }

      /* ---------------- WebDAV（投屏收卡带） ---------------- */
      case 'webdav_start':
        return fromRust(NapiBridge.webdavStart(argsJson));
      case 'webdav_stop':
        return fromRust(NapiBridge.webdavStop());
      case 'webdav_info':
        return fromRust(NapiBridge.webdavInfo());
      case 'es_read_file_base64': {
        const path = pathOf(argsJson);
        if (!path) {
          return fail('es_read_file_base64: path is empty');
        }
        return fromRust(NapiBridge.readFileBase64(path));
      }
      case 'es_list_files':
        return fromRust(NapiBridge.listFiles());

      /* ---------------- 手机手柄 UDP 5991 ---------------- */
      case 'pad_start': {
        const info = await this.pad.start(portOf(argsJson) || 5991);
        return ok(JSON.stringify(info));
      }
      case 'pad_stats': {
        const stats: PadStats = this.pad.stats();
        return ok(JSON.stringify(stats));
      }
      case 'pad_target':
        return ok(JSON.stringify(this.pad.setTarget(targetOf(argsJson))));
      case 'pad_stop':
        return ok(JSON.stringify(this.pad.stop()));
      case 'pad_reset':
        return ok(JSON.stringify(this.pad.reset()));

      /* ---------------- DLNA 投屏接收（DMR，Rust 桥） ---------------- */
      // SSDP 用 MulticastSocket 语义（Rust 侧 socket2 join），只需 INTERNET 权限；
      // 事件（dlna://play 等）走 host 队列 + bridge_poll，与桌面端一致。
      case 'dlna_start':
        return fromRust(NapiBridge.dlnaStart(argsJson));
      case 'dlna_stop':
        return fromRust(NapiBridge.dlnaStop());
      case 'dlna_status':
        return fromRust(NapiBridge.dlnaStatus());
      case 'dlna_report_position':
        return fromRust(NapiBridge.dlnaReportPosition(argsJson));
      case 'dlna_send_remote_event':
        return fromRust(NapiBridge.dlnaSendRemoteEvent(argsJson));
      case 'dlna_debug_log':
        return fromRust(NapiBridge.dlnaDebugLog(argsJson));
      case 'get_dlna_name':
      case 'ProcessBridgeModule.getDlnaName':
        return ok(NapiBridge.dlnaGetName());
      case 'set_dlna_name':
      case 'ProcessBridgeModule.setDlnaName':
        return fromRust(NapiBridge.dlnaSetName(argsJson));
      case 'endpoint_start':
        try {
          return ok(await this.endpoint.start(argsJson));
        } catch (e) {
          const err = e as Error;
          return fail(err.message);
        }
      case 'endpoint_stop':
        return ok(JSON.stringify(this.endpoint.stop()));
      case 'endpoint_info':
        return ok(this.endpoint.infoJson());
      case 'endpoint_reply':
        return ok(JSON.stringify(this.endpoint.reply(argsJson)));
      case 'http_download_open':
        try {
          return ok(await httpDownloadOpen(argsJson));
        } catch (e) {
          const err = e as Error;
          return fail(err.message);
        }
      case 'http_download_read':
        try {
          return ok(httpDownloadRead(argsJson));
        } catch (e) {
          const err = e as Error;
          return fail(err.message);
        }
      case 'http_download_close':
        return ok(httpDownloadClose(argsJson));

      default:
        hilog.warn(DOMAIN, TAG, 'command not implemented: %{public}s', cmd);
        return fail(`__unsupported__:${cmd}`);
    }
  }
}

function pathOf(argsJson: string): string {
  try {
    const a = JSON.parse(argsJson) as Record<string, string>;
    const p = a['path'];
    return p === undefined || p === null ? '' : String(p);
  } catch (e) {
    return '';
  }
}

function targetOf(argsJson: string): string {
  try {
    const a = JSON.parse(argsJson) as Record<string, string>;
    const t = a['target'];
    return t === undefined || t === null ? '' : String(t);
  } catch (e) {
    return '';
  }
}
