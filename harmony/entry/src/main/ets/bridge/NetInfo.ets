import { connection } from '@kit.NetworkKit';
import { hilog } from '@kit.PerformanceAnalysisKit';

const DOMAIN = 0x0000;
const TAG = 'ToupingNet';

function pickIpv4(props: connection.ConnectionProperties): string {
  const links = props.linkAddresses;
  if (!links) {
    return '';
  }
  for (let i = 0; i < links.length; i++) {
    const addr = links[i].address;
    if (!addr) {
      continue;
    }
    const ip = addr.address;
    if (addr.family === 1 && !ip.startsWith('127.') && !ip.startsWith('169.254.') && ip !== '0.0.0.0') {
      return ip;
    }
  }
  return '';
}

/**
 * 本机局域网 IPv4。
 *
 * 为什么放在 ArkTS：Rust 侧原来靠解析 `ifconfig` 输出（macOS 写法），鸿蒙沙箱里
 * 既没有这个命令也没 PATH，结果是空列表 → DLNA 设备名退化成「投屏显象_00.00」、
 * SSDP 也 join 不上组。`@ohos.net.connection` 是系统权威来源，两边共用这一份。
 *
 * 取址顺序：默认网络优先（电视一般只连一个 WiFi），拿不到再遍历全部网络 ——
 * 部分机型（WiFi + 有线、或默认网是VPN/蜂窝）默认网络的 linkAddresses 会是空的。
 *
 * 过滤：回环 / 链路本地（169.254）/ 0.0.0.0。都拿不到时返回空串，调用方降级到 getifaddrs。
 */
export async function resolveLanIp(): Promise<string> {
  try {
    const net = await connection.getDefaultNet();
    if (net && net.netId !== 0) {
      const ip = pickIpv4(await connection.getConnectionProperties(net));
      if (ip) {
        return ip;
      }
    }
  } catch (e) {
    const err = e as Error;
    hilog.warn(DOMAIN, TAG, 'default net probe failed: %{public}s', err.message);
  }

  try {
    const all = await connection.getAllNets();
    for (let i = 0; i < all.length; i++) {
      const ip = pickIpv4(await connection.getConnectionProperties(all[i]));
      if (ip) {
        return ip;
      }
    }
  } catch (e) {
    const err = e as Error;
    hilog.warn(DOMAIN, TAG, 'all nets probe failed: %{public}s', err.message);
  }

  return '';
}
