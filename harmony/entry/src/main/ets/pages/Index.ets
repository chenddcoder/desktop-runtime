import { webview } from '@kit.ArkWeb';
import { hilog } from '@kit.PerformanceAnalysisKit';
import { BusinessError } from '@kit.BasicServicesKit';
import { common } from '@kit.AbilityKit';
import { InvokeRouter } from '../bridge/InvokeRouter';
import { NapiBridge } from '../bridge/NapiBridge';
import { PadUdp } from '../bridge/PadUdp';
import { TAURI_SHIM_JS } from '../bridge/TauriShim';

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

  aboutToAppear(): void {
    this.bridge.handler = (id: string, cmd: string, args: string) => {
      this.handleCall(id, cmd, args);
    };

    this.router.onExit = () => {
      const ctx = this.getUIContext().getHostContext() as common.UIAbilityContext;
      ctx.terminateSelf();
    };

    // 手柄按键：桌面端是 Rust 收包后 emit；鸿蒙侧由 ArkTS 直收再推事件（见 PadUdp.ets 顶部）
    this.pad.emit = (name: string, payloadJson: string) => {
      this.runJs('window.__tauriHostEmit(' + JSON.stringify(name) + ',' + JSON.stringify(payloadJson) + ')');
    };

    // 落盘根：鸿蒙沙箱路径推断不出来，必须由 Ability 上下文提供
    const ctx = this.getUIContext().getHostContext() as common.UIAbilityContext;
    const setRoot = NapiBridge.setRoot(ctx.filesDir);
    hilog.info(DOMAIN, TAG, 'webdav root: %{public}s', setRoot);

    this.startEventPump();
  }

  aboutToDisappear(): void {
    if (this.pumpTimer >= 0) {
      clearInterval(this.pumpTimer);
      this.pumpTimer = -1;
    }
    this.pad.stop();
  }

  build() {
    Stack() {
      Web({ src: `${WEB_ORIGIN}/index.html?resolver=${RESOLVER_BASE}`, controller: this.controller })
        .javaScriptAccess(true)
        .domStorageAccess(true)
        .fileAccess(false)
        .imageAccess(true)
        .zoomAccess(false)
        .multiWindowAccess(false)
        .horizontalScrollBarAccess(false)
        .verticalScrollBarAccess(false)
        .javaScriptProxy({
          object: this.bridge,
          name: '__ohosBridge',
          methodList: ['call'],
          controller: this.controller
        })
        .javaScriptOnDocumentStart([{ script: TAURI_SHIM_JS, scriptRules: ['*'] }])
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
