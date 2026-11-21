import { socket } from '@kit.NetworkKit';
import { hilog } from '@kit.PerformanceAnalysisKit';

const DOMAIN = 0x0000;
const TAG = 'ToupingPad';

/**
 * 手机手柄 · UDP 快通道 —— `src-tauri/src/pad.rs` 的 ArkTS 实现。
 *
 * 协议与小程序 `utils/cast/channel.js` 逐字一致（改了手机端就收不到）：
 *   手机 → 大屏  `seq,mask,t`   ASCII
 *   大屏 → 手机  `ack,seq`      原样回传序号
 *   探活         `ping` → `ack,-1`
 * mask 位：bit0=A bit1=B bit2=Select bit3=Start bit4=Up bit5=Down bit6=Left bit7=Right
 *
 * ## 为什么这一层放在 ArkTS 而不是 Rust
 * 桌面端 Rust 收包后必须把按键**推给 JS**（内核跑在 JS 里的 WebWasmBackend），
 * 走的是 Tauri 的 emit。鸿蒙侧要等价，Rust 就得用 ThreadsafeFunction 回调，
 * 每包一次跨线程调度；而 ArkTS 自己收包可以直接 `runJavaScript` 推事件，
 * 少一层跳转、少一个待同步的生命周期。UDP 逻辑只有百来行，重复一次比引桥更便宜。
 *
 * ## ⛔ 与落盘通道的关系（和桌面端一致，别搞反）
 * UDP 只负责把「位图」交给页面，**不直接写内核**：页面按「位图变了才下发」写进模拟器
 * （`pad-udp.ts` 的桌面分支）。电视上是内核直收、JS 要收手，两者相反。
 */

/** 手机端写死的端口。被占就顺延（与桌面端 PORT_SCAN 一致） */
const PREFERRED_PORT: number = 5991;
const PORT_SCAN: number = 10;
/** 单包上限：手柄包最多几十字节 */
const MAX_PACKET: number = 64;

export interface PadInfo {
  ok: boolean;
  port: number;
  target: string;
  error: string;
}

export interface PadStats {
  running: boolean;
  port: number;
  recv: number;
  applied: number;
  seqGap: number;
  dropped: number;
  /** 距离最近一次收包的毫秒数；**从没收到过是 0**（JS 侧判活先要求 recv > 0） */
  lastRecvAgoMs: number;
  mask: number;
  target: string;
  error: string;
}

/** 位图 → 8 个键。顺序与 `pad-core.ts` 的 `b[]` 一致：上、下、左、右、A、B、SELECT、START */
function bitsOf(mask: number): number[] {
  return [
    (mask >> 4) & 1,
    (mask >> 5) & 1,
    (mask >> 6) & 1,
    (mask >> 7) & 1,
    mask & 1,
    (mask >> 1) & 1,
    (mask >> 2) & 1,
    (mask >> 3) & 1
  ];
}

/** 解析 `seq,mask[,t]`；任何一段不合法整包判死（宁可丢一包，也不要把错位数据当按键） */
function parsePacket(text: string): number[] | null {
  const parts = text.trim().split(',');
  if (parts.length < 2) {
    return null;
  }
  const seq = Number(parts[0].trim());
  const mask = Number(parts[1].trim());
  if (!Number.isFinite(seq) || !Number.isFinite(mask)) {
    return null;
  }
  if (!Number.isInteger(seq) || !Number.isInteger(mask)) {
    return null;
  }
  return [seq, mask];
}

function asciiOf(buf: ArrayBuffer): string | null {
  const view = new Uint8Array(buf);
  if (view.length === 0 || view.length > MAX_PACKET) {
    return null;
  }
  // 协议是纯 ASCII；出现高位字节说明不是手柄包，直接丢
  let out = '';
  for (let i = 0; i < view.length; i++) {
    const b = view[i];
    if (b === 0 || b > 127) {
      return null;
    }
    out += String.fromCharCode(b);
  }
  return out;
}

export class PadUdp {
  private udp: socket.UDPSocket | null = null;
  private port: number = 0;
  private running: boolean = false;
  private mask: number = 0;
  private recv: number = 0;
  private applied: number = 0;
  private seqGap: number = 0;
  private dropped: number = 0;
  private lastRecvAt: number = 0;
  /** 上一包序号；-1 = 还没收过（手机重启 seq 归零时不能算成大丢包） */
  private lastSeq: number = -1;
  private target: string = '';
  private error: string = '';
  /**
   * 事件出口：由页面注入。
   * ⛔ 不要在这里直接持有 WebviewController —— 这一层要能在没有 UI 的单测里跑。
   */
  emit: (name: string, payloadJson: string) => void = () => {};

  info(): PadInfo {
    return { ok: this.running, port: this.port, target: this.target, error: this.error };
  }

  stats(): PadStats {
    return {
      running: this.running,
      port: this.port,
      recv: this.recv,
      applied: this.applied,
      seqGap: this.seqGap,
      dropped: this.dropped,
      // ⛔ 没收到过包一律 0（不是 -1）：JS 侧判活先要求 recv > 0
      lastRecvAgoMs: this.lastRecvAt > 0 ? Date.now() - this.lastRecvAt : 0,
      mask: this.mask,
      target: this.target,
      error: this.error
    };
  }

  /** 起监听（幂等）。已在听时直接返回当前信息，不重复 bind。 */
  async start(preferred: number): Promise<PadInfo> {
    if (this.running) {
      return this.info();
    }
    const first = preferred > 0 ? preferred : PREFERRED_PORT;
    let lastErr = '';
    for (let p = first; p < first + PORT_SCAN; p++) {
      const udp = socket.constructUDPSocketInstance();
      try {
        // 绑 0.0.0.0：手机在局域网里从任意网卡进来，绑回环就收不到
        await udp.bind({ address: '0.0.0.0', port: p, family: 1 });
      } catch (e) {
        const err = e as Error;
        lastErr = `bind ${p}: ${err.message}`;
        try {
          udp.off('message');
        } catch (ignore) {
          // 没挂过监听，忽略
        }
        continue;
      }
      udp.on('message', (info: socket.SocketMessageInfo) => {
        this.onPacket(udp, info);
      });
      this.udp = udp;
      this.port = p;
      this.running = true;
      this.error = '';
      hilog.info(DOMAIN, TAG, 'pad udp listening on %{public}d', p);
      return this.info();
    }
    this.error = lastErr || 'no port available';
    this.running = false;
    return { ok: false, port: 0, target: this.target, error: this.error };
  }

  private onPacket(udp: socket.UDPSocket, info: socket.SocketMessageInfo): void {
    const text = asciiOf(info.message);
    if (text === null) {
      this.dropped += 1;
      return;
    }
    /*
     * ⛔ 不能把 `info.remoteInfo` 直接当 NetAddress 用：SocketRemoteInfo.family 是字符串
     * ('IPv4'/'IPv6')，而 send() 要的 NetAddress.family 是数字（1/2）。类型不兼容，
     * 编译期就报错 —— 必须显式转换一次。
     */
    const peer: socket.NetAddress = {
      address: info.remoteInfo.address,
      port: info.remoteInfo.port,
      family: info.remoteInfo.family === 'IPv6' ? 2 : 1
    };
    // 探活：只回执，不碰按键（手机用它判「这台电视在不在听」）
    if (text.trim().toLowerCase() === 'ping') {
      this.reply(udp, 'ack,-1', peer);
      return;
    }
    const parsed = parsePacket(text);
    if (parsed === null) {
      this.dropped += 1;
      return;
    }
    const seq = parsed[0];
    const mask = parsed[1];
    // 先回执再记状态：手机侧 800ms 超时回收，回执越早越好
    this.reply(udp, `ack,${seq}`, peer);

    this.recv += 1;
    this.lastRecvAt = Date.now();
    // 序号缺口：只在确实前进时算，手机重启（seq 归零）不能算丢包
    if (this.lastSeq >= 0 && seq > this.lastSeq) {
      const jump = seq - this.lastSeq - 1;
      if (jump > 0 && jump < 1000) {
        this.seqGap += jump;
      }
    }
    this.lastSeq = seq;
    const changed = this.mask !== mask;
    this.mask = mask;
    if (!changed) {
      // 松手后手机仍按 20Hz 发中立包（mask=0），逐包推会把 webview 事件队列喂成噪音
      return;
    }
    this.applied += 1;
    this.emit('pad://input', JSON.stringify({ mask: mask, bits: bitsOf(mask), seq: seq }));
  }

  private reply(udp: socket.UDPSocket, data: string, peer: socket.NetAddress): void {
    try {
      udp.send({ data: data, address: peer }).catch((err: Error) => {
        hilog.debug(DOMAIN, TAG, 'ack send failed: %{public}s', err.message);
      });
    } catch (e) {
      // send 失败不影响按键：手机侧重发即可
    }
  }

  /** 目标实例（桌面端没有 wasm 实例概念，只记录给页面显示） */
  setTarget(target: string): boolean {
    this.target = target;
    return true;
  }

  /** 收端口。下次 `start` 会重新 bind。 */
  stop(): boolean {
    const was = this.running;
    if (this.udp) {
      try {
        this.udp.off('message');
      } catch (e) {
        // 没挂过监听
      }
      try {
        this.udp.close();
      } catch (e) {
        // 已关闭
      }
      this.udp = null;
    }
    this.running = false;
    this.port = 0;
    return was;
  }

  /** 归零读数（换一局时调；不清 target/port） */
  reset(): boolean {
    this.recv = 0;
    this.applied = 0;
    this.seqGap = 0;
    this.dropped = 0;
    this.lastSeq = -1;
    this.mask = 0;
    this.lastRecvAt = 0;
    return true;
  }
}
