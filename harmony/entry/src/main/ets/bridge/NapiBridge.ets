import { hilog } from '@kit.PerformanceAnalysisKit';
import rawBridge from 'libquickapp_bridge.so';

const DOMAIN = 0x0000;
const TAG = 'ToupingNapi';

/**
 * .so 的导出形状。
 *
 * ⚠️ 为什么需要它：当前 SDK 对 NAPI 模块还不做类型校验（构建日志里那条
 * "Currently module for 'libquickapp_bridge.so' is not verified" 就是这意思），
 * `import` 进来的默认导出被推断成 any，ArkTS 的 arkts-no-any-unknown 会直接报错。
 * 显式声明接口并断言一次，比在每个调用点 as 一遍干净。
 *
 * 方法名必须与 harmony-bridge/src/lib.rs 的 `#[napi] pub fn` 逐字一致。
 */
interface RustBridge {
  ping(argsJson: string): string;
  bridge_info(): string;
  base64_roundtrip(input: string): string;
  bridge_poll(): string;
  webdav_set_root(path: string): string;
  webdav_start(argsJson: string): string;
  webdav_stop(): string;
  webdav_info(): string;
  es_read_file_base64(path: string): string;
  es_list_files(): string;
  dlna_set_root(path: string): string;
  dlna_set_local_ip(ip: string): string;
  dlna_start(argsJson: string): string;
  dlna_stop(): string;
  dlna_status(): string;
  dlna_report_position(argsJson: string): string;
  dlna_send_remote_event(argsJson: string): string;
  dlna_debug_log(argsJson: string): string;
  dlna_get_name(): string;
  dlna_set_name(argsJson: string): string;
}

const bridge: RustBridge = rawBridge as RustBridge;

/**
 * Rust 桥封装。
 *
 * 设计取舍：桌面端那 22 个命令里，只有「必须持有 socket / 需要跨进程状态」的才值得下沉到
 * Rust（WebDAV 服务端、手机手柄 UDP、endpoint 服务端）。纯 HTTP 转发（proxy_http）和
 * 文件选择，ArkTS 原生模块更省事也更贴合系统证书链，不要为了「复用」硬搬。
 *
 * 出错时返回带 `__error__` 的 JSON，而不是往 ArkTS 抛异常 —— 调用方统一按 JSON 处理，
 * 避免 ArkTS 与 NAPI 之间的异常类型差异把错误吞掉。
 */
export class NapiBridge {
  /** .so 是否真的加载成功（首次调用时探测，失败则永久降级） */
  private static available: boolean = true;
  private static probed: boolean = false;
  /** 首次探测失败的原因：页面直接看得见，省去抓 hilog 的一步 */
  private static loadError: string = '';

  static isAvailable(): boolean {
    if (NapiBridge.probed) {
      return NapiBridge.available;
    }
    NapiBridge.probed = true;
    try {
      const info = bridge.bridge_info();
      hilog.info(DOMAIN, TAG, 'rust bridge loaded: %{public}s', info);
      NapiBridge.available = true;
    } catch (err) {
      // ArkTS 不允许把 catch 参数当 any 用（arkts-no-any-unknown），先显式收窄
      const e = err as Error;
      hilog.error(DOMAIN, TAG, 'rust bridge unavailable: %{public}s', e.message);
      NapiBridge.loadError = e.message;
      NapiBridge.available = false;
    }
    return NapiBridge.available;
  }

  private static guard(fn: () => string): string {
    if (!NapiBridge.isAvailable()) {
      const detail = NapiBridge.loadError ? `: ${NapiBridge.loadError}` : '';
      return `{"__error__":"rust bridge not loaded${detail}"}`;
    }
    try {
      return fn();
    } catch (err) {
      const e = err as Error;
      return `{"__error__":${JSON.stringify(e.message)}}`;
    }
  }

  static ping(argsJson: string): string {
    return NapiBridge.guard(() => bridge.ping(argsJson));
  }

  static info(): string {
    return NapiBridge.guard(() => bridge.bridge_info());
  }

  /** 取走 Rust 侧待发给页面的全部事件（JSON 数组） */
  static poll(): string {
    return NapiBridge.guard(() => bridge.bridge_poll());
  }

  /* ---------------- WebDAV ---------------- */

  static setRoot(path: string): string {
    return NapiBridge.guard(() => bridge.webdav_set_root(path));
  }

  static webdavStart(argsJson: string): string {
    return NapiBridge.guard(() => bridge.webdav_start(argsJson));
  }

  static webdavStop(): string {
    return NapiBridge.guard(() => bridge.webdav_stop());
  }

  static webdavInfo(): string {
    return NapiBridge.guard(() => bridge.webdav_info());
  }

  static readFileBase64(path: string): string {
    return NapiBridge.guard(() => bridge.es_read_file_base64(path));
  }

  static listFiles(): string {
    return NapiBridge.guard(() => bridge.es_list_files());
  }

  /* ---------------- DLNA（投屏接收 DMR） ---------------- */

  static dlnaSetRoot(path: string): string {
    return NapiBridge.guard(() => bridge.dlna_set_root(path));
  }

  static dlnaSetLocalIp(ip: string): string {
    return NapiBridge.guard(() => bridge.dlna_set_local_ip(ip));
  }

  static dlnaStart(argsJson: string): string {
    return NapiBridge.guard(() => bridge.dlna_start(argsJson));
  }

  static dlnaStop(): string {
    return NapiBridge.guard(() => bridge.dlna_stop());
  }

  static dlnaStatus(): string {
    return NapiBridge.guard(() => bridge.dlna_status());
  }

  static dlnaReportPosition(argsJson: string): string {
    return NapiBridge.guard(() => bridge.dlna_report_position(argsJson));
  }

  static dlnaSendRemoteEvent(argsJson: string): string {
    return NapiBridge.guard(() => bridge.dlna_send_remote_event(argsJson));
  }

  static dlnaDebugLog(argsJson: string): string {
    return NapiBridge.guard(() => bridge.dlna_debug_log(argsJson));
  }

  static dlnaGetName(): string {
    return NapiBridge.guard(() => bridge.dlna_get_name());
  }

  static dlnaSetName(argsJson: string): string {
    return NapiBridge.guard(() => bridge.dlna_set_name(argsJson));
  }
}
