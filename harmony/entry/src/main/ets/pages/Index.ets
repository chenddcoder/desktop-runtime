import { webview } from '@kit.ArkWeb';
import { hilog } from '@kit.PerformanceAnalysisKit';
import { BusinessError } from '@kit.BasicServicesKit';
import { common } from '@kit.AbilityKit';
import { InvokeRouter } from '../bridge/InvokeRouter';
import { NapiBridge } from '../bridge/NapiBridge';
import { PadUdp } from '../bridge/PadUdp';
import { resolveLanIp } from '../bridge/NetInfo';
import { TAURI_SHIM_JS } from '../bridge/TauriShim';
import { DLNA_OVERLAY_JS } from '../bridge/DlnaOverlay';
import { WEB_SCALE_JS } from '../bridge/WebScale';

const DOMAIN = 0x0000;
const TAG = 'ToupingPage';

/**
 * 本地资源虚拟域。
 *
 * 为什么不用 $rawfile() 直接当 src：
 *   rawfile 协议下 location.origin 是 resource://rawfile，webpack 的
 *   __webpack_public_path__='/' 会把 chunk 请求打到根路径，并且 fetch 本地文件容易被
 *   同源策略拦。自建一个 https 域 + onInterceptRequest 全量拦截，可以让页面以为自己在
 *   一个正常的 https 站点上，chunk / wasm / fetch 行为与线上一致。
 */
const WEB_ORIGIN = 'https://appassets.local';

/**
 * 快应用 resolve 的后台地址。
 *
 * 桌面端由 src-tauri/ui_scale.js 注入 https://runtime.chenddcoder.cn；这里通过
 * index.html 支持的 ?resolver= 参数覆盖 —— index.html 内部会无条件用
 * location.origin 覆盖 __CROSS_APP_RESOLVER_BASE__，所以只能用 query 覆盖，
 * document-start 里改全局变量会被它冲掉（踩过）。
 */
const RESOLVER_BASE = 'https://runtime.chenddcoder.cn';

/**
 * 启动即进入「投屏显象」快应用，与桌面端 `src-tauri/es-app.config.json` 的
 * `esPackage` 保持一致。不带 es_pkg 时会停在 web-runtime 的应用目录页。
 *
 * 调试面板（上传 ZIP / 强制刷新）由 `TauriShim` 注入 CSS 整块隐藏，不看收起条；
 * 需要它时在启动 URL 上加 `debug=1`。
 */
const ES_PKG = 'cn.chenddcoder.tvcast';

const START_QUERY = `resolver=${RESOLVER_BASE}&es_pkg=${ES_PKG}`;

/** Rust 事件队列的拉取间隔。落盘通知这类一次性事件，80ms 抖动无所谓。 */
const EVENT_POLL_MS = 80;

class OhosBridge {
  handler: (id: string, cmd: string, args: string) => void = () => {};

  call(id: string, cmd: string, args: string): void {
    this.handler(id, cmd, args);
  }
}

@Entry
@Component
struct Index {
  private controller: webview.WebviewController = new webview.WebviewController();
  private bridge: OhosBridge = new OhosBridge();
  private pad: PadUdp = new PadUdp();
  private router: InvokeRouter = new InvokeRouter(this.pad);
  private pumpTimer: number = -1;
  /** onAreaChange 是逐帧触发的，节流后再通知页面重算 */
  private resizeTimer: number = -1;
  /** 启动时解析一次本机局域网 IP，DLNA 与定时器共用同一个结果 */
  private lanIp: Promise<string> = Promise.resolve('');

  aboutToAppear(): void {
    this.bridge.handler = (id: string, cmd: string, args: string) => {
      this.handleCall(id, cmd, args);
    };

    this.router.onExit = () => {
      const ctx = this.getUIContext().getHostContext() as common.UIAbilityContext;
      ctx.terminateSelf();
    };

    // endpoint 服务收到的请求要原样交给页面算（留言板 host 模式），与手柄事件同一条下行通道
    this.router.setHostEmit((name: string, payloadJson: string) => {
      this.runJs('window.__tauriHostEmit(' + JSON.stringify(name) + ',' + JSON.stringify(payloadJson) + ')');
    });

    // 手柄按键：桌面端是 Rust 收包后 emit；鸿蒙侧由 ArkTS 直收再推事件（见 PadUdp.ets 顶部）
    this.pad.emit = (name: string, payloadJson: string) => {
      this.runJs('window.__tauriHostEmit(' + JSON.stringify(name) + ',' + JSON.stringify(payloadJson) + ')');
    };

    // 落盘根：鸿蒙沙箱路径推断不出来，必须由 Ability 上下文提供
    const ctx = this.getUIContext().getHostContext() as common.UIAbilityContext;
    const setRoot = NapiBridge.setRoot(ctx.filesDir);
    hilog.info(DOMAIN, TAG, 'webdav root: %{public}s', setRoot);

    // DLNA 的日志/设备名配置也落在沙箱里；桌面端在 setup 阶段就拉起 DMR(5001)，
    // 这里等首帧后再起，避免 bind 端口/建 tokio 任务拖慢启动。
    hilog.info(DOMAIN, TAG, 'dlna root: %{public}s', NapiBridge.dlnaSetRoot(ctx.filesDir));

    // 本机局域网 IP 尽早解析并喂给 Rust：鸿蒙沙箱里 getifaddrs 常只回环，Rust 拿不到
    // 真实网卡时设备名会退化成「投屏显象_00.00」、SSDP 也 join 不上组，手机搜不到本机。
    // 页面可能在 dlnaStart 之前就调 get_dlna_name 拿标题，所以这里不能等定时器。
    // 只是 hint：Rust 自己枚举成功时仍以内核结果为准。
    this.lanIp = resolveLanIp();
    this.lanIp.then((ip: string) => {
      if (ip) {
        hilog.info(DOMAIN, TAG, 'dlna local ip: %{public}s', NapiBridge.dlnaSetLocalIp(ip));
      } else {
        hilog.warn(DOMAIN, TAG, 'lan ip unresolved, fall back to getifaddrs');
      }
    }).catch((e: BusinessError) => {
      hilog.warn(DOMAIN, TAG, 'resolveLanIp failed: %{public}s', e.message);
    });

    setTimeout(() => {
      // 等 IP 落定再起 DMR，保证设备名/SSDP LOCATION 一次就是对的
      this.lanIp.then(() => {
        const r = NapiBridge.dlnaStart('{"port":5001}');
        hilog.info(DOMAIN, TAG, 'dlna_start: %{public}s', r);
      });
    }, 1200);

    this.startEventPump();
  }

  aboutToDisappear(): void {
    if (this.pumpTimer >= 0) {
      clearInterval(this.pumpTimer);
      this.pumpTimer = -1;
    }
    if (this.resizeTimer >= 0) {
      clearTimeout(this.resizeTimer);
      this.resizeTimer = -1;
    }
    this.pad.stop();
  }

  /** Web 组件区域变化 → 节流通知页面派发 resize，WebScale 会在回调里重算铺满比例 */
  private scheduleResizeNotify(): void {
    if (this.resizeTimer >= 0) {
      return;
    }
    this.resizeTimer = setTimeout(() => {
      this.resizeTimer = -1;
      this.runJs('window.dispatchEvent(new Event("resize"))');
    }, 80);
  }

  build() {
    Stack() {
      Web({ src: `${WEB_ORIGIN}/index.html?${START_QUERY}`, controller: this.controller })
        .javaScriptAccess(true)
        .domStorageAccess(true)
        .fileAccess(false)
        .imageAccess(true)
        .zoomAccess(false)
        .multiWindowAccess(false)
        /*
         * 放行 https 页面里的 http 资源。
         *
         * 页面自己的 origin 是 https://appassets.local（onInterceptRequest 虚拟域），而手机
         * 推过来的媒体地址是 http://192.168.x.x:port/xxx —— 默认混合内容策略会把这类
         * 子资源直接拦掉，表现是：手机显示投屏成功、电视端 video 报错、画面永远不出来。
         */
        .mixedMode(MixedMode.All)
        /*
         * 关掉「必须用户手势才能播放」。
         *
         * 投屏是程序化调 play()，电视遥控器没有任何点击动作；默认策略下 <video> 会被
         * 静音/挂起，手机端 GetTransportInfo 一直是 STOPPED。
         */
        .mediaPlayGestureAccess(false)
        .horizontalScrollBarAccess(false)
        .verticalScrollBarAccess(false)
        .javaScriptProxy({
          object: this.bridge,
          name: '__ohosBridge',
          methodList: ['call'],
          controller: this.controller
        })
        // 顺序有意义：先把 __TAURI__ 补出来，再挂 DLNA → 快应用的事件桥。
        // 少了第二段，手机会显示"投屏成功"而电视端毫无反应（事件没有监听者）。
        // WEB_SCALE_JS 负责让 1920x1080 舞台跟窗口尺寸走（内置 scaleApp 缓存了首次视口）。
        .javaScriptOnDocumentStart([
          { script: WEB_SCALE_JS, scriptRules: ['*'] },
          { script: TAURI_SHIM_JS, scriptRules: ['*'] },
          { script: DLNA_OVERLAY_JS, scriptRules: ['*'] }
        ])
        .onInterceptRequest((event) => this.intercept(event))
        .onConsole((event) => {
          hilog.info(DOMAIN, TAG, '[web] %{public}s', event.message.getMessage());
          return false;
        })
        .onPageEnd(() => {
          hilog.info(DOMAIN, TAG, 'page end: %{public}s', this.controller.getUrl());
        })
        .onErrorReceive((event) => {
          hilog.error(DOMAIN, TAG, 'resource error: %{public}s %{public}s',
            event.request.getRequestUrl(), event.error.getErrorInfo());
        })
        /*
         * 窗口被拉大/拉小时，ArkWeb 的视口更新是异步的，某些版本不会派发 DOM resize。
         * 这里在组件区域变化后主动通知一次，让 WebScale 注入脚本重算缩放，避免画面停在旧尺寸。
         */
        .onAreaChange(() => this.scheduleResizeNotify())
        .width('100%')
        .height('100%')
    }
    .width('100%')
    .height('100%')
    .backgroundColor(Color.Black)
  }

  /**
   * Rust → 页面的单向事件泵。
   *
   * 桌面端 Rust 能直接 `app.emit`；鸿蒙侧 Rust 不能回调 JS（要用 ThreadsafeFunction），
   * 于是改成队列 + 轮询：Rust 把事件堆进 VecDeque，这里定期取空并逐条 `runJavaScript`。
   * 页面侧看到的仍然是 `desktopListen('webdav://upload', ...)`，与桌面完全一致。
   */
  private startEventPump(): void {
    this.pumpTimer = setInterval(() => {
      const json = NapiBridge.poll();
      if (!json || json === '[]') {
        return;
      }
      try {
        const list = JSON.parse(json) as Array<Record<string, Object>>;
        for (const item of list) {
          const name = String(item['name']);
          const payload = item['payload'];
          const payloadJson = JSON.stringify(payload === undefined ? null : payload);
          this.runJs('window.__tauriHostEmit(' + JSON.stringify(name) + ',' + JSON.stringify(payloadJson) + ')');
        }
      } catch (e) {
        const err = e as BusinessError;
        hilog.error(DOMAIN, TAG, 'event pump failed: %{public}s', err.message);
      }
    }, EVENT_POLL_MS);
  }

  /** invoke 分发：ArkTS 处理完用 runJavaScript 把结果回调给页面（不依赖 Promise 序列化） */
  private handleCall(reqId: string, cmd: string, argsJson: string): void {
    this.router.invoke(cmd, argsJson)
      .then((r) => {
        const payload = r.ok ? r.value : r.error;
        this.runJs(`window.__tauriBridgeResolve(${JSON.stringify(reqId)}, ${r.ok}, ${JSON.stringify(payload)})`);
      })
      .catch((e: BusinessError) => {
        this.runJs(`window.__tauriBridgeResolve(${JSON.stringify(reqId)}, false, ${JSON.stringify(e.message)})`);
      });
  }

  private runJs(js: string): void {
    try {
      this.controller.runJavaScript(js);
    } catch (e) {
      const err = e as BusinessError;
      hilog.error(DOMAIN, TAG, 'runJavaScript failed: %{public}s', err.message);
    }
  }

  /** 把 appassets.local 的请求映射到 rawfile/web，带正确的 MIME 与 CORS 头 */
  private intercept(event: OnInterceptRequestEvent): WebResourceResponse {
    const url = event.request.getRequestUrl();
    if (!url.startsWith(WEB_ORIGIN)) {
      // 运行时语义：返回 null 表示放行到网络。但 ArkWeb 的类型声明是
      // Callback<OnInterceptRequestEvent, WebResourceResponse>（非空），直接 `null as` 会被
      // ArkTS 判为「类型不重叠」。这里经 ESObject 中转，把类型检查让过去，运行时仍是 null。
      const pass: ESObject = null;
      return pass as WebResourceResponse;
    }
    let rel = url.substring(WEB_ORIGIN.length);
    const q = rel.indexOf('?');
    if (q >= 0) {
      rel = rel.substring(0, q);
    }
    if (rel === '' || rel === '/') {
      rel = '/index.html';
    }
    const rawPath = `web${rel}`;

    const ctx = this.getUIContext().getHostContext() as common.UIAbilityContext;
    try {
      const content = ctx.resourceManager.getRawFileContentSync(rawPath);
      const resp = new WebResourceResponse();
      resp.setResponseCode(200);
      resp.setReasonMessage('OK');
      resp.setResponseMimeType(mimeOf(rel));
      resp.setResponseData(content.buffer as ArrayBuffer);
      resp.setResponseHeader([
        { headerKey: 'Access-Control-Allow-Origin', headerValue: '*' },
        { headerKey: 'Cache-Control', headerValue: 'no-cache' }
      ]);
      return resp;
    } catch (e) {
      const err = e as BusinessError;
      hilog.error(DOMAIN, TAG, 'rawfile miss: %{public}s (%{public}s)', rawPath, err.message);
      const resp = new WebResourceResponse();
      resp.setResponseCode(404);
      resp.setReasonMessage('Not Found');
      resp.setResponseMimeType('text/plain');
      resp.setResponseData('not found');
      return resp;
    }
  }
}

function mimeOf(path: string): string {
  const p = path.toLowerCase();
  if (p.endsWith('.html') || p.endsWith('.htm')) return 'text/html';
  if (p.endsWith('.js') || p.endsWith('.mjs')) return 'application/javascript';
  if (p.endsWith('.css')) return 'text/css';
  if (p.endsWith('.json')) return 'application/json';
  if (p.endsWith('.wasm')) return 'application/wasm';
  if (p.endsWith('.svg')) return 'image/svg+xml';
  if (p.endsWith('.png')) return 'image/png';
  if (p.endsWith('.jpg') || p.endsWith('.jpeg')) return 'image/jpeg';
  if (p.endsWith('.webp')) return 'image/webp';
  if (p.endsWith('.zip')) return 'application/zip';
  if (p.endsWith('.gz')) return 'application/gzip';
  if (p.endsWith('.woff') || p.endsWith('.woff2')) return 'font/woff';
  return 'application/octet-stream';
}
