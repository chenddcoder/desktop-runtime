// 快应用桌面运行时 · Tauri v2 入口
// P0：初始化窗口 + 加载前端（web-runtime/dist），插件先挂好。
// 窗口比例锁定为 TV 端 1920×1080 = 16:9（Tauri v2 无原生 aspect-ratio 锁定，这里手动钳制）。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod dlna;
pub mod proxy;

use tauri::{
    Emitter, LogicalPosition, LogicalSize, WebviewUrl, WebviewWindowBuilder, WindowEvent,
};

// TV 端比例：1920 / 1080 = 16:9
const TV_ASPECT: f64 = 16.0 / 9.0;

// DevTools 开关：默认关闭。需要开启时，在项目根目录（或可执行文件旁）放 devtools.json：
//   { "enabled": true }
// 仅在 debug 构建生效；release 构建不含 devtools feature，open_devtools 不会编译进包。
fn devtools_enabled() -> bool {
    // 以「项目根目录（current_dir，npm run dev 时为 desktop-runtime 根）」为权威：
    // 根目录有明确 enabled 声明即以它为准 —— 这样即使 target/debug 里遗留了 enabled=true，
    // 也不会覆盖根目录的关闭意图（之前 devtools 关不掉的坑就源于此）。
    // 仅当根目录没有 devtools.json / 无 enabled 字段时，才兜底读可执行文件旁（打包态）。
    if let Ok(d) = std::env::current_dir() {
        let p = d.join("devtools.json");
        if let Ok(text) = std::fs::read_to_string(&p) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(b) = v.get("enabled").and_then(|x| x.as_bool()) {
                    return b; // 根目录有明确声明 → 以此为最终值
                }
            }
        }
    }
    // 兜底：根目录无 devtools.json 或无 enabled 字段时，读可执行文件旁（打包态）。
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let p = parent.join("devtools.json");
            if let Ok(text) = std::fs::read_to_string(&p) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                    return v.get("enabled").and_then(|x| x.as_bool()).unwrap_or(false);
                }
            }
        }
    }
    false
}

// 投屏播放叠层脚本（注入到 web-runtime 页面，监听 dlna://play 全屏播放）。
// 通过 WebviewWindowBuilder.initialization_script 注入，不改 web-runtime 本体。
const DLNA_OVERLAY_JS: &str = include_str!("../dlna_overlay.js");
// 缩放自适应 + es_pkg 预加载：经 initialization_script 注入，不改 web-runtime 本体。
const UI_SCALE_JS: &str = include_str!("../ui_scale.js");
// 跨域代理注入（绕过浏览器 CORS，让 es_pkg 的 resolve / zip 下载走 Rust reqwest）。
const PROXY_FETCH_JS: &str = include_str!("../proxy_fetch.js");

// 默认加载的快应用配置（es-app.config.json，编译期注入）。
// 想打包成另一个桌面应用（如天气），只改这个文件：{"esPackage":"cn.chenddcoder.weather","appName":"天气"}
// 后重新 cargo build 即可；dev 与 release 都通过 ui_scale.js 读 window.__ES_DEFAULT_PKG__ 生效。
const ES_APP_CONFIG_JSON: &str = include_str!("../es-app.config.json");

/// 解析默认快应用配置；字段缺失时兜底 tvcast（投屏显象）。
fn es_app_config() -> (String, String) {
    let (pkg_default, name_default) = ("cn.chenddcoder.tvcast".to_string(), "投屏显象".to_string());
    let v: serde_json::Value =
        serde_json::from_str(ES_APP_CONFIG_JSON).unwrap_or(serde_json::Value::Null);
    let pkg = v
        .get("esPackage")
        .and_then(|x| x.as_str())
        .map(String::from)
        .unwrap_or(pkg_default);
    let name = v
        .get("appName")
        .and_then(|x| x.as_str())
        .map(String::from)
        .unwrap_or(name_default);
    (pkg, name)
}

fn main() {
    tauri::Builder::default()
        // 文件选择 / 保存对话框（P1 本地导入 sideload 用）
        .plugin(tauri_plugin_dialog::init())
        // 受控文件系统访问（P1 读取本地 bundle / 写应用数据）
        .plugin(tauri_plugin_fs::init())
        // 打开外部文件 / 目录 / URL（P2 系统能力用）
        .plugin(tauri_plugin_shell::init())
        .setup(|app| {
            // 用 Rust 建窗口（而非仅 config），以便注入初始化脚本 + 锁定 16:9。
            // 开发态（debug）走 tauri.conf.json 的 devUrl（本地 serve.mjs:1420 服务本地 web-runtime/dist，
            //   自带 /proxy 端点，与 web-cli dev 行为一致；es_pkg 由 ui_scale.js 注入默认包）；
            // 发布态（release）加载内置 asset（frontendDist 打包进二进制），
            //   es_pkg 同样由 ui_scale.js 的 ensureEsPkg 用 history.replaceState 原地注入（不导航、不循环）。
            // 注意：绝不能再 External 到 runtime.chenddcoder.cn —— 那是 web-cli 的 dev runtime，
            //   AutoProxy 全代理模式会把 Tauri 自身的 ipc:// IPC 也代理，造成递归 → 内存爆炸（7GB）。
            // 默认加载哪个快应用由 es-app.config.json 的 esPackage 决定（改配置 + 重新构建即换应用）。
            let url = WebviewUrl::App("index.html".into());
            // 把默认 es_pkg 提前注入为全局变量，供 ui_scale.js 的 ensureEsPkg 读取（须先于 UI_SCALE_JS 执行）
            let (es_pkg, app_name) = es_app_config();
            let es_pkg_js = format!(
                "window.__ES_DEFAULT_PKG__ = {};",
                serde_json::to_string(&es_pkg).unwrap_or_else(|_| "\"cn.chenddcoder.tvcast\"".into())
            );
            eprintln!("[desktop-runtime] default es_pkg={es_pkg} appName={app_name}");
            let window = WebviewWindowBuilder::new(app, "main", url)
                .title(&app_name)
                .inner_size(1600.0, 900.0)
                .min_inner_size(640.0, 360.0)
                .resizable(true)
                .maximizable(false)
                .fullscreen(false)
                .center()
                .initialization_script(DLNA_OVERLAY_JS)
                .initialization_script(&es_pkg_js)
                .initialization_script(UI_SCALE_JS)
                .initialization_script(PROXY_FETCH_JS)
                .build()
                .expect("failed to build main window");

            // 记录上次校正后的逻辑尺寸，供拖动缩放时判断「用户拖的是哪条边」（见下方 Resized 处理）。
            let last_size = std::sync::Arc::new(std::sync::Mutex::new((0.0f64, 0.0f64)));

            // 启动即放大到「当前屏幕可用区域内最大的 16:9 内切尺寸」并居中。
            // 不调用 window.maximize()：真正最大化会把窗口拉成屏幕比例（如 16:10），
            // 导致 16:9 的 TV 内容被裁切、两侧/上下丢内容。
            // 用 scale_factor 把 PhysicalSize 换算回逻辑像素（Retina 下不可直接用物理像素）。
            if let Ok(Some(monitor)) = window.current_monitor() {
                if let Ok(scale) = window.scale_factor() {
                    if scale > 0.0 {
                        // 用工作区域（去掉菜单栏 / Dock 占用），并在该显示器内手动居中，
                        // 避免 Tauri center() 在多显示器 / 带系统栏时用合并区域算错、导致窗口偏出屏幕。
                        let work = monitor.work_area();
                        let avail_x = work.position.x as f64 / scale;
                        let avail_y = work.position.y as f64 / scale;
                        let avail_w = work.size.width as f64 / scale;
                        let avail_h = work.size.height as f64 / scale;
                        let mut w = avail_w;
                        let mut h = w / TV_ASPECT;
                        if h > avail_h {
                            h = avail_h;
                            w = h * TV_ASPECT;
                        }
                        let px = avail_x + (avail_w - w) / 2.0;
                        let py = avail_y + (avail_h - h) / 2.0;
                        let _ = window.set_size(LogicalSize::new(w, h));
                        let _ = window.set_position(LogicalPosition::new(px, py));
                        *last_size.lock().unwrap() = (w, h);
                    }
                }
            }

            // DevTools：默认关闭，由 devtools.json 的 enabled 控制（见 devtools_enabled）。
            // 仅在 debug 构建生效（release 构建不含 devtools feature）。
            #[cfg(debug_assertions)]
            if devtools_enabled() {
                window.open_devtools();
            }

            // 拖动缩放时把窗口钳回 16:9。
            // 必须以「用户拖动的那条边」所在维度为基准，否则会出现
            // 「拖右边能缩小、拖下边缩不动」的不对称——因为只改高度的拖动里，
            // 高度偏差总小于宽度偏差，旧逻辑永远把高度拉回原值。
            // 解法：记录上次校正后的尺寸，比较本次哪个维度变化更大，就以它为准
            // （拖左右边/角 → 高度跟随宽度；拖上下边 → 宽度跟随高度）。
            // Tauri v2 的 Resized 会在 set_size 后再触发一次，用 0.5px 阈值收敛，避免无限抖动。
            // 用 clone() 当接收者避免「借用+move」冲突（E0505）。
            window.clone().on_window_event({
                let last_size = last_size.clone();
                move |event| {
                if let WindowEvent::Resized(_) = event {
                    // 全屏 / 最大化时不强制比例，避免与显示器分辨率打架
                    if window.is_fullscreen().unwrap_or(false)
                        || window.is_maximized().unwrap_or(false)
                    {
                        return;
                    }
                    // inner_size() 在 Retina 下返回物理像素，必须用 scale_factor() 换算回逻辑尺寸，
                    // 否则把物理尺寸当逻辑 set 回去会让窗口尺寸指数爆炸 → NSWindow abort。
                    if let (Ok(physical), Ok(scale)) = (window.inner_size(), window.scale_factor()) {
                        if scale <= 0.0 {
                            return;
                        }
                        let w = physical.width as f64 / scale; // 逻辑宽
                        let h = physical.height as f64 / scale; // 逻辑高
                        let (pw, ph) = *last_size.lock().unwrap();
                        // 判断主导维度：变化更大的维度即用户拖动的那条边
                        let (nw, nh) = if pw <= 0.0 && ph <= 0.0 {
                            (w, w / TV_ASPECT) // 首次校正：以宽为基准维持 16:9
                        } else if (w - pw).abs() >= (h - ph).abs() {
                            (w, w / TV_ASPECT) // 宽度变化主导 → 高度跟随宽度
                        } else {
                            (h * TV_ASPECT, h) // 高度变化主导 → 宽度跟随高度
                        };
                        if (nw - w).abs() > 0.5 || (nh - h).abs() > 0.5 {
                            let _ = window.set_size(LogicalSize::new(nw, nh));
                            *last_size.lock().unwrap() = (nw, nh);
                        } else {
                            *last_size.lock().unwrap() = (w, h);
                        }
                    }
                }
            }});

            // ========== P0: 自动启动 DLNA DMR 服务端 ==========
            // DLNA 是后台服务（UDP 多播发现 + HTTP SOAP），不依赖任何 UI 页面。
            // 一旦 app 启动就让 SSDP 始终在广播，手机扫码/电视搜索等任意入口都能找到这台桌面。
            // 失败不致命（端口被占/无权限时给提示，不影响 web-runtime 加载）。
            //
            // 关键：整个 spawn 流程 100% 兜底，**任何**卡住 / 错误都会 emit 一条 status 给前端，
            // 否则错误会被静默吞掉——这是用户上一轮 DLNA 搜不到的根本原因。
            eprintln!("[desktop-runtime] setup: about to spawn dlna_start");
            let dlna_emit = app.handle().clone();
            // 1) 立即发一条 starting 标记——证明 Rust 路径到了、spawn 在跑
            let _ = dlna_emit.emit(
                "dlna://status",
                serde_json::json!({
                    "ok": null, // 特殊值：表示"启动中"，前端用琥珀色徽章
                    "msg": "DLNA 启动中…",
                }),
            );
            // 2) 整体 8s 兜底：dlna_start 内部任何卡住（get_local_ip 无外网 / 端口被占 / 防火墙）都会超时退出
            tauri::async_runtime::spawn(async move {
                eprintln!("[desktop-runtime] dlna spawn: entered");
                let result = tokio::time::timeout(
                    std::time::Duration::from_secs(8),
                    dlna::dlna_start(dlna_emit.clone(), Some(5001)),
                )
                .await;
                eprintln!("[desktop-runtime] dlna spawn: finished, result.is_ok={}", result.is_ok());
                let payload = match result {
                    Ok(Ok(info)) => serde_json::json!({
                        "ok": true,
                        "port": info.port,
                        "uuid": info.uuid,
                        "msg": format!("DLNA 已启动 → http://*:{}/device-desc.xml", info.port),
                    }),
                    Ok(Err(e)) => {
                        eprintln!("[desktop-runtime] dlna_start error: {e}");
                        serde_json::json!({ "ok": false, "error": e })
                    }
                    Err(_) => {
                        eprintln!("[desktop-runtime] dlna_start TIMEOUT after 8s");
                        serde_json::json!({
                            "ok": false,
                            "error": "DLNA 启动 8s 超时（可能 1900/5001 端口被占、缺少多播路由、或防火墙拦截 UDP 239.255.255.250:1900）",
                        })
                    }
                };
                let _ = dlna_emit.emit("dlna://status", payload);
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![dlna::dlna_start, dlna::dlna_stop, dlna::dlna_status, dlna::dlna_report_position, proxy::proxy_http])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
