import { AbilityConstant, ConfigurationConstant, UIAbility, Want } from '@kit.AbilityKit';
import { hilog } from '@kit.PerformanceAnalysisKit';
import { window } from '@kit.ArkUI';
import { BusinessError, deviceInfo } from '@kit.BasicServicesKit';
import { preferences } from '@kit.ArkData';

const DOMAIN = 0x0000;
const TAG = 'ToupingEntry';

/** 窗口宽高的持久化位置（HAP 沙箱内 Preferences） */
const PREF_STORE = 'window_state';
const PREF_WIDTH = 'width';
const PREF_HEIGHT = 'height';
const PREF_LEFT = 'left';
const PREF_TOP = 'top';
/** 老版本没存过位置时的哨兵值：0 是合法坐标，不能用 0 表示「未知」 */
const POS_UNKNOWN = -100000;
/** 拖拽窗口时 windowSizeChange 会高频触发，攒一下再落盘 */
const SAVE_DEBOUNCE_MS = 400;
/** 手动纠正窗口比例的防抖：拖拽过程中先不动，松手后再对齐，避免和手指抢 */
const ASPECT_DEBOUNCE_MS = 260;
/** 舞台是 16:9（1920x1080），窗口跟着同比例缩放，页面才不会留黑边或被裁切 */
const ASPECT_W = 16;
const ASPECT_H = 9;
const ASPECT = ASPECT_W / ASPECT_H;
const PREF_OPTIONS: preferences.Options = { name: PREF_STORE };
/** 判定「已经恢复到目标尺寸」的容差（px） */
const RESTORE_TOLERANCE = 4;
/** 等不到窗口 SHOWN 事件时的兜底等待，超时就不再拦截写盘 */
const RESTORE_UNLOCK_MS = 3000;
/** resize 之后等系统真正应用尺寸，再回读落盘（自愈：存实际值而不是不可达的目标值） */
const SETTLE_MS = 320;

export default class EntryAbility extends UIAbility {
  private mainWin: window.Window | null = null;
  private saveTimer: number = -1;
  private aspectTimer: number = -1;
  /** 上一次回调的尺寸，用来判断用户主要拖的是宽还是高 */
  private prevWidth: number = 0;
  private prevHeight: number = 0;
  /** 连续纠正次数：resize 的尺寸语义若与预期不同，防止和系统来回拉扯 */
  private alignTries: number = 0;
  /**
   * 启动恢复窗口期：此时 windowSizeChange 只说明「默认窗口刚创建」，
   * 绝不是用户想要的尺寸。若这时落盘会把上次记住的宽高冲掉，
   * 表现就是「关掉再开还是默认大小，怎么拖都记不住」。
   */
  private restoring: boolean = false;
  private pendingRestoreW: number = 0;
  private pendingRestoreH: number = 0;
  private pendingRestoreL: number = POS_UNKNOWN;
  private pendingRestoreT: number = POS_UNKNOWN;
  /** 窗口是否已经上屏（SHOWN 之后 resize 才会真正生效） */
  private stageShown: boolean = false;
  /** 恢复重试次数：未上屏时 resize 常常「成功但没生效」，给它一到两次机会 */
  private restoreAttempt: number = 0;
  /** SHOWN 与 resize 回调可能同时进来，简单去重避免把重试次数烧光 */
  private lastEnsureTs: number = 0;

  onCreate(want: Want, launchParam: AbilityConstant.LaunchParam): void {
    try {
      this.context.getApplicationContext().setColorMode(ConfigurationConstant.ColorMode.COLOR_MODE_DARK);
    } catch (err) {
      hilog.error(DOMAIN, TAG, 'setColorMode failed: %{public}s', JSON.stringify(err));
    }
    hilog.info(DOMAIN, TAG, 'onCreate');
  }

  onWindowStageCreate(windowStage: window.WindowStage): void {
    try {
      const win = windowStage.getMainWindowSync();
      this.mainWin = win;
      win.on('windowSizeChange', (size: window.Size) => {
        this.onWindowSizeChange(size);
      });
      // 只拖动窗口（不改尺寸）不会触发 windowSizeChange，位置必须靠这个事件才存得到
      try {
        win.on('windowRectChange', (options: window.RectChangeOptions) => {
          this.onWindowRectChange(options);
        });
      } catch (e) {
        const err = e as BusinessError;
        hilog.warn(DOMAIN, TAG, 'on windowRectChange failed: %{public}s', err.message);
      }
    } catch (e) {
      const err = e as BusinessError;
      hilog.warn(DOMAIN, TAG, 'getMainWindowSync failed: %{public}s', err.message);
    }
    this.logWindowState('stage create');

    // 第一步只做恢复：别在这之前锁比例/改布局，否则 resize 会被系统按比例拒掉
    this.restoreWindowSize();

    // 窗口真正显示后再校验一次：窗口未上屏时的 resize 可能被系统直接丢掉
    try {
      windowStage.on('windowStageEvent', (event: window.WindowStageEventType) => {
        if (event === window.WindowStageEventType.SHOWN) {
          this.stageShown = true;
          // 刚 SHOWN 那一帧窗口还没完全就绪，resize 容易被丢掉，等一拍再来
          setTimeout(() => {
            this.ensureRestored();
          }, 80);
        }
      });
    } catch (e) {
      const err = e as BusinessError;
      hilog.warn(DOMAIN, TAG, 'on windowStageEvent failed: %{public}s', err.message);
    }
    // 兜底：SHOWN 事件没来也不能永远拦着写盘
    setTimeout(() => {
      if (this.restoring) {
        hilog.warn(DOMAIN, TAG, 'restore unlock by timeout');
        this.restoring = false;
      }
    }, RESTORE_UNLOCK_MS);

    windowStage.loadContent('pages/Index', (err) => {
      if (err.code) {
        hilog.error(DOMAIN, TAG, 'loadContent failed: %{public}s', JSON.stringify(err));
        return;
      }
      try {
        const win = windowStage.getMainWindowSync();
        win.setWindowKeepScreenOn(true);
        /*
         * 沉浸式全屏 + 隐藏系统栏只在「本来就是全屏」的设备（手机/电视）上做。
         * 2in1/平板的浮动窗口里调 setWindowLayoutFullScreen 会连带改写窗口外框尺寸
         * （实测启动时窗口会被从 1930x1149 改成 1893x1148），刚恢复的宽高当场被冲掉。
         */
        if (!this.isFloatingWindow()) {
          win.setWindowLayoutFullScreen(true);
          win.setSpecificSystemBarEnabled('status', false);
          win.setSpecificSystemBarEnabled('navigationIndicator', false);
        }
      } catch (e) {
        const err = e as BusinessError;
        hilog.warn(DOMAIN, TAG, 'window setup failed: %{public}s', err.message);
      }
      hilog.info(DOMAIN, TAG, 'content loaded');
      this.logWindowState('content loaded');
      // 首启没有保存过尺寸时没人触发收尾，这里补一次
      if (!this.restoring) {
        this.lockAspectRatio();
      }
    });
  }

  onWindowStageDestroy(): void {
    this.flushWindowSize();
    if (this.aspectTimer >= 0) {
      clearTimeout(this.aspectTimer);
      this.aspectTimer = -1;
    }
    if (this.mainWin) {
      try {
        this.mainWin.off('windowSizeChange');
        this.mainWin.off('windowRectChange');
      } catch (e) {
        const err = e as BusinessError;
        hilog.warn(DOMAIN, TAG, 'off windowSizeChange failed: %{public}s', err.message);
      }
      this.mainWin = null;
    }
    hilog.info(DOMAIN, TAG, 'onWindowStageDestroy');
  }

  onDestroy(): void {
    hilog.info(DOMAIN, TAG, 'onDestroy');
  }

  /**
   * 锁定窗口 16:9（只约束内容区，不含标题栏）。
   *
   * 必须在恢复尺寸之后调用：先锁比例再 resize 会被系统按比例拒绝，
   * 表现就是「restore 打印成功、窗口却还是原来的大小」。
   */
  private lockAspectRatio(): void {
    const win = this.mainWin;
    if (!win || !this.isFloatingWindow()) {
      return;
    }
    win.setAspectRatio(ASPECT).then(() => {
      hilog.info(DOMAIN, TAG, 'aspect ratio locked: 16:9');
    }).catch((e: BusinessError) => {
      hilog.warn(DOMAIN, TAG, 'setAspectRatio failed (%{public}s), fallback to manual', e.message);
    });
  }

  /** 窗口尺寸变化 → 防抖落盘，并顺手把窗口内容区对齐到 16:9 */
  private onWindowSizeChange(size: window.Size): void {
    const w = size.width;
    const h = size.height;
    if (w <= 0 || h <= 0) {
      return;
    }
    // 最大化/全屏/分屏下的尺寸不是用户要记住的「窗口大小」，也不该被改成 16:9
    if (!this.isFloatingWindow()) {
      return;
    }
    if (this.restoring) {
      // 恢复过程中的尺寸变化是系统在试错，别记录也别对齐
      return;
    }
    if (this.saveTimer >= 0) {
      clearTimeout(this.saveTimer);
    }
    this.saveTimer = setTimeout(() => {
      this.saveTimer = -1;
      this.flushWindowSize();
    }, SAVE_DEBOUNCE_MS);

    // 始终保留手动兜底：系统锁定生效时尺寸本来就是 16:9，alignToAspect 会命中容差直接返回；
    // 而某些窗口模式下 setAspectRatio 是「resolve 成功但并未真正约束」，这里得接住
    const changedW = Math.abs(w - this.prevWidth);
    const changedH = Math.abs(h - this.prevHeight);
    this.prevWidth = w;
    this.prevHeight = h;
    this.scheduleAspectCorrect(changedW >= changedH);
  }

  /** 窗口 rect（位置或尺寸）变化 → 防抖落盘 */
  private onWindowRectChange(options: window.RectChangeOptions): void {
    if (this.restoring || !this.isFloatingWindow()) {
      return;
    }
    if (this.saveTimer >= 0) {
      clearTimeout(this.saveTimer);
    }
    this.saveTimer = setTimeout(() => {
      this.saveTimer = -1;
      this.flushWindowSize();
    }, SAVE_DEBOUNCE_MS);
  }

  /** 当前是否浮动窗口（UNDEFINED 多出现在窗口刚创建时，一并当作可用） */
  private isFloatingWindow(): boolean {
    const win = this.mainWin;
    if (!win) {
      return true;
    }
    try {
      const status = win.getWindowStatus();
      return status === window.WindowStatusType.FLOATING || status === window.WindowStatusType.UNDEFINED;
    } catch (e) {
      const err = e as BusinessError;
      hilog.warn(DOMAIN, TAG, 'getWindowStatus failed: %{public}s', err.message);
      return true;
    }
  }

  private logWindowState(tag: string): void {
    const win = this.mainWin;
    if (!win) {
      return;
    }
    // getWindowStatus 在 tv 上不可用，单独包一层，别让日志把主流程带崩
    let status = -1;
    try {
      status = win.getWindowStatus() as number;
    } catch (e) {
      hilog.debug(DOMAIN, TAG, 'getWindowStatus unavailable on this device');
    }
    try {
      const p = win.getWindowProperties();
      hilog.info(DOMAIN, TAG,
        '%{public}s: rect %{public}d,%{public}d %{public}dx%{public}d, drawable %{public}dx%{public}d, status %{public}d, device %{public}s',
        tag, p.windowRect.left, p.windowRect.top, p.windowRect.width, p.windowRect.height,
        p.drawableRect.width, p.drawableRect.height, status, deviceInfo.deviceType);
    } catch (e) {
      const err = e as BusinessError;
      hilog.warn(DOMAIN, TAG, 'logWindowState failed: %{public}s', err.message);
    }
  }

  /**
   * 手动对齐 16:9（setAspectRatio 不可用时的兜底）。
   *
   * 以用户主要拖动的维度为准：横向拖就跟着调高度，纵向拖就跟着调宽度，
   * 否则「只想拉高」会被程序把高度改回去，感觉拖不动。
   */
  private scheduleAspectCorrect(widthDriven: boolean): void {
    if (this.aspectTimer >= 0) {
      clearTimeout(this.aspectTimer);
    }
    this.aspectTimer = setTimeout(() => {
      this.aspectTimer = -1;
      const win = this.mainWin;
      if (!win || this.restoring) {
        return;
      }
      this.alignToAspect(win, widthDriven);
    }, ASPECT_DEBOUNCE_MS);
  }

  /**
   * 按 16:9 对齐当前窗口。
   *
   * 注意 setAspectRatio 约束的是「内容区（drawableRect，不含标题栏）」的比例，
   * 而 resize() 吃的是整窗尺寸，所以要用 drawableRect 反推装饰高度再加回去，
   * 否则整窗 16:9 会让内容区变成 16:(9 - 标题栏)，页面又会留边。
   */
  private alignToAspect(win: window.Window, widthDriven: boolean): void {
    if (!this.isFloatingWindow() || this.restoring) {
      return;
    }
    let rectW = 0;
    let rectH = 0;
    let drawW = 0;
    let drawH = 0;
    try {
      const props = win.getWindowProperties();
      rectW = props.windowRect.width;
      rectH = props.windowRect.height;
      drawW = props.drawableRect.width;
      drawH = props.drawableRect.height;
    } catch (e) {
      const err = e as BusinessError;
      hilog.warn(DOMAIN, TAG, 'getWindowProperties failed: %{public}s', err.message);
      return;
    }
    // 窗口还没开始渲染时 drawableRect 是 0x0，此时按整窗比例算会把标题栏当内容，
    // 算出来的目标尺寸是错的（会白 resize 一次），直接等下一轮尺寸回调再对齐
    if (!(drawW > 0) || !(drawH > 0)) {
      return;
    }
    const decoW = Math.max(0, rectW - drawW);
    const decoH = Math.max(0, rectH - drawH);
    let targetW = drawW;
    let targetH = Math.round(drawW / ASPECT);
    if (!widthDriven) {
      targetW = Math.round(drawH * ASPECT);
      targetH = drawH;
    }
    if (Math.abs(targetW - drawW) < 3 && Math.abs(targetH - drawH) < 3) {
      this.alignTries = 0;
      return;
    }
    const winW = targetW + decoW;
    const winH = targetH + decoH;
    hilog.info(DOMAIN, TAG, 'align window aspect: content %{public}dx%{public}d -> %{public}dx%{public}d, window %{public}dx%{public}d',
      drawW, drawH, targetW, targetH, winW, winH);
    if (this.alignTries >= 3) {
      hilog.warn(DOMAIN, TAG, 'aspect align not converged, skip resize this round');
      this.alignTries = 0;
      return;
    }
    this.alignTries++;
    setTimeout(() => {
      this.alignTries = 0;
    }, 800);
    win.resize(winW, winH).catch((e: BusinessError) => {
      hilog.warn(DOMAIN, TAG, 'aspect align resize failed: %{public}s', e.message);
    });
  }

  /**
   * 移动窗口到绝对坐标。
   *
   * windowRect.left/top 是「整个显示区域」的坐标，多屏时可能跨屏；
   * moveWindowToGlobal 用的正是这套坐标系，优先用它。个别设备不支持（801）时
   * 退回 moveWindowTo（相对当前屏幕原点），单屏下两者等价。
   */
  private moveWindow(win: window.Window, x: number, y: number, tag: string): void {
    win.moveWindowToGlobal(x, y).catch((e: BusinessError) => {
      hilog.warn(DOMAIN, TAG, '%{public}s moveWindowToGlobal failed(%{public}s), fallback',
        tag, e.message);
      win.moveWindowTo(x, y).catch((e2: BusinessError) => {
        hilog.warn(DOMAIN, TAG, '%{public}s moveWindowTo failed: %{public}s', tag, e2.message);
      });
    });
  }

  /**
   * 同步写盘：存的是整窗 windowRect（px，与 resize 单位一致）。
   *
   * 记录的是「系统实际给到的尺寸」，而不是我们请求的目标值 ——
   * 如果目标超出屏幕可用区域被系统 clamp，下次就用实际值，不会每次都白试一遍。
   */
  private flushWindowSize(): void {
    if (this.saveTimer >= 0) {
      clearTimeout(this.saveTimer);
      this.saveTimer = -1;
    }
    const win = this.mainWin;
    if (!win) {
      return;
    }
    let w = 0;
    let h = 0;
    let left = 0;
    let top = 0;
    try {
      const rect = win.getWindowProperties().windowRect;
      w = rect.width;
      h = rect.height;
      left = rect.left;
      top = rect.top;
    } catch (e) {
      const err = e as BusinessError;
      hilog.warn(DOMAIN, TAG, 'save skipped, getWindowProperties failed: %{public}s', err.message);
      return;
    }
    if (w <= 0 || h <= 0) {
      return;
    }
    try {
      const store = preferences.getPreferencesSync(this.context, PREF_OPTIONS);
      store.putSync(PREF_WIDTH, w);
      store.putSync(PREF_HEIGHT, h);
      store.putSync(PREF_LEFT, left);
      store.putSync(PREF_TOP, top);
      store.flushSync();
      hilog.info(DOMAIN, TAG, 'window state saved: %{public}d,%{public}d %{public}dx%{public}d', left, top, w, h);
    } catch (e) {
      const err = e as BusinessError;
      hilog.warn(DOMAIN, TAG, 'save window size failed: %{public}s', err.message);
    }
  }

  /** 恢复上次退出时的窗口宽高，并在窗口 SHOWN 后补一次校验 */
  private restoreWindowSize(): void {
    const win = this.mainWin;
    if (!win || !this.isFloatingWindow()) {
      return;
    }
    let savedW = 0;
    let savedH = 0;
    let savedL = POS_UNKNOWN;
    let savedT = POS_UNKNOWN;
    try {
      const store = preferences.getPreferencesSync(this.context, PREF_OPTIONS);
      savedW = store.getSync(PREF_WIDTH, 0) as number;
      savedH = store.getSync(PREF_HEIGHT, 0) as number;
      savedL = store.getSync(PREF_LEFT, POS_UNKNOWN) as number;
      savedT = store.getSync(PREF_TOP, POS_UNKNOWN) as number;
    } catch (e) {
      const err = e as BusinessError;
      hilog.warn(DOMAIN, TAG, 'read saved window size failed: %{public}s', err.message);
      return;
    }
    if (savedW <= 0 || savedH <= 0) {
      hilog.info(DOMAIN, TAG, 'no saved window size, first launch');
      return;
    }
    this.restoring = true;
    this.restoreAttempt = 0;
    this.pendingRestoreW = savedW;
    this.pendingRestoreH = savedH;
    this.pendingRestoreL = savedL;
    this.pendingRestoreT = savedT;
    this.prevWidth = savedW;
    this.prevHeight = savedH;
    hilog.info(DOMAIN, TAG, 'window state restoring: pos %{public}d,%{public}d size %{public}dx%{public}d',
      savedL, savedT, savedW, savedH);
    if (savedL > POS_UNKNOWN && savedT > POS_UNKNOWN) {
      this.moveWindow(win, savedL, savedT, 'restore');
    }
    win.resize(savedW, savedH).then(() => {
      hilog.info(DOMAIN, TAG, 'resize to saved size requested');
      // 窗口未上屏时 resize 可能「返回成功但实际没生效」，交给 SHOWN 后统一校验
      if (this.stageShown) {
        this.ensureRestored();
      }
    }).catch((e: BusinessError) => {
      // 窗口还没上屏时常见，交给 SHOWN 事件后的 ensureRestored 重试
      hilog.warn(DOMAIN, TAG, 'resize(%{public}d, %{public}d) failed, will retry on shown: %{public}s',
        savedW, savedH, e.message);
    });
  }

  /** 窗口显示后校验：尺寸没到位就再 resize 一次，最多试两轮 */
  private ensureRestored(): void {
    const win = this.mainWin;
    const w = this.pendingRestoreW;
    const h = this.pendingRestoreH;
    if (!win || w <= 0 || h <= 0) {
      return;
    }
    const now = Date.now();
    if (now - this.lastEnsureTs < 150) {
      return;
    }
    this.lastEnsureTs = now;
    try {
      const rect = win.getWindowProperties().windowRect;
      if (Math.abs(rect.width - w) <= RESTORE_TOLERANCE && Math.abs(rect.height - h) <= RESTORE_TOLERANCE) {
        this.markRestored(win);
        return;
      }
      hilog.info(DOMAIN, TAG, 'restore retry(%{public}d) on shown: current %{public}dx%{public}d -> %{public}dx%{public}d',
        this.restoreAttempt, rect.width, rect.height, w, h);
    } catch (e) {
      const err = e as BusinessError;
      hilog.warn(DOMAIN, TAG, 'ensureRestored getWindowProperties failed: %{public}s', err.message);
    }
    if (this.restoreAttempt >= 2) {
      // 两轮都没到目标：说明这个尺寸在当前窗口/屏幕模式下不可达（被系统限制），
      // 收尾时会把实际尺寸写回，下次启动直接用它，不再每次白试
      hilog.warn(DOMAIN, TAG, 'restore gave up after retries, keep actual size');
      this.markRestored(win);
      return;
    }
    this.restoreAttempt++;
    if (this.pendingRestoreL > POS_UNKNOWN && this.pendingRestoreT > POS_UNKNOWN) {
      this.moveWindow(win, this.pendingRestoreL, this.pendingRestoreT, 'restore-retry');
    }
    win.resize(w, h).then(() => {
      // 回读校验：resize resolve 不代表尺寸已经落地
      setTimeout(() => {
        this.ensureRestored();
      }, 200);
    }).catch((e: BusinessError) => {
      hilog.warn(DOMAIN, TAG, 'restore retry failed: %{public}s', e.message);
      this.restoring = false;
      this.pendingRestoreW = 0;
      this.pendingRestoreH = 0;
      this.pendingRestoreL = POS_UNKNOWN;
      this.pendingRestoreT = POS_UNKNOWN;
      this.lockAspectRatio();
    });
  }

  /**
   * 恢复收尾：解除写盘拦截 → 锁 16:9 → 回读实际尺寸落盘。
   *
   * 回读落盘是「自愈」：如果保存的尺寸在当前屏幕/窗口模式下不可达，
   * 系统会 clamp 一个它能给的值，我们把实际值记下来，下次启动就一次到位。
   */
  private markRestored(win: window.Window): void {
    this.restoring = false;
    this.pendingRestoreW = 0;
    this.pendingRestoreH = 0;
    this.pendingRestoreL = POS_UNKNOWN;
    this.pendingRestoreT = POS_UNKNOWN;
    this.logWindowState('restored');
    this.lockAspectRatio();
    setTimeout(() => {
      if (this.mainWin) {
        this.flushWindowSize();
        this.logWindowState('settled');
      }
    }, SETTLE_MS);
  }
}
