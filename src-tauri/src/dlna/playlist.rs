// 抖音播放列表 TCP 通道 —— 移植 EsApp/esapp-xiaoyoucast/docs/dy_dlna_playlist/dlna_demo 的 :playlist 库
//
// 职责（与 demo 模块定位一致）：
//   - 监听控制端口（45165 优先，被占退化为系统端口），按 uint32LE + UTF-8 JSON 拆帧
//   - 处理 GetDeviceInfo 握手与可选 X25519 / AES-128-GCM 加密会话（encrypt=1）
//   - 接收 Play / AddDramaList 等命令，按 dramaId 合并列表（LinkedHashMap 语义）
//   - 回 ACK，并在需要时主动发 PushMediaInfo（手机端列表/进度 UI 跟随）
//   - 支持 PlayPreDrama / PlayNextDrama / PlayDramaId / PlayDramaList2 本地选集
// 不做的事：SSDP/description.xml 发现（宿主做）、视频解码/拉流（宿主播）。
//
// 与宿主（dlna::mod）的衔接：
//   - dlna_start 先 start() 拿真实 control_port，再发布 SSDP/HTTP（启动顺序不能改）
//   - 列表当前项变化（Play/AddDramaList/选集命令）→ on_play_request 回调 → 宿主 emit dlna://play
//   - 播完自动切集由宿主调用 next_and_get()（move(1) + 返回新当前项），再 emit 新 url
//   - PushMediaInfo 广播到所有已连接客户端（手机重连后仍能同步）

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use aes_gcm::aead::consts::U16;
use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes128Gcm, AesGcm, Nonce};
use rand::rngs::OsRng;
use rand::RngCore;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use x25519_dalek::{PublicKey, StaticSecret};

// ---------------------------------------------------------------------------
// 协议常量（对齐 demo WireCompatibility，不可改名）
// ---------------------------------------------------------------------------
pub mod wire {
    /// SSDP / description.xml 响应中的列表通道端口头
    pub const CONTROL_PORT_HEADER: &str = "BDLEPORT";
    /// 能力位：0x800（增强控制，与 SERVER/BDLEPORT 一起参与发送端识别）
    pub const FEATURE_BITMAP: u32 = 0x800;
    /// 协议版权/版本（GetDeviceInfo 响应字段）
    pub const PROTOCOL_COPYRIGHT: &str = "BDLE/1.0";
    pub const PROTOCOL_VERSION: &str = "39512";
    /// 抖音打开列表通道的 SERVER 兼容指纹（demo 已 A/B 实测必要：
    /// 改为普通 DLNA 值时抖音只走 SetAVTransportURI/Play，从不连列表端口）
    pub const DOUYIN_SERVER: &str = "Linux/6.0 HTTP/1.0 BDLE+DLNA/1.1 HPPlay/1.0";
    /// 标准 DLNA SERVER（NOTIFY 广播 / 普通客户端，避免指纹污染发现）
    pub const STD_SERVER: &str = "Linux/6.0 UPnP/1.1 QuickApp-DLNA/1.0";
    /// 兼容响应标识（demo 未单独 A/B，保留）
    pub const X_USER_AGENT: &str = "redsonic";
    /// 首选控制端口：被占时退化为系统端口
    pub const PREFERRED_PORT: u16 = 45165;
    /// 单帧上限（512 KiB）
    pub const MAX_FRAME: usize = 512 * 1024;
}

/// 播放列表设备身份（宿主注入，GetDeviceInfo 里的宿主字段）
#[derive(Clone)]
pub struct PlaylistDeviceIdentity {
    pub ip: String,
    pub name: String,
    pub package_name: String,
    pub device_id: String,
    pub os_version: String,
    pub device_model: String,
    pub device_brand: String,
}

/// 列表中的单条视频（从 dramaBeans[].urlBeans 提取）
#[derive(Clone, Debug)]
pub struct PlaylistItem {
    pub drama_id: String,
    pub title: String,
    /// 单集时长（毫秒；宿主集数/进度显示用）
    #[allow(dead_code)]
    pub duration_ms: u64,
    /// 媒体 HTTP UA（宿主起播时透传，防抖音 CDN 鉴权拒绝）
    #[allow(dead_code)]
    pub user_agent: String,
    pub url: String,
    /// 原始 episode JSON（PushMediaInfo 需要原样回传 dramaBeans）
    pub raw: Value,
}

/// 稳定的数字字符串 deviceId：存 JSON 文件，同一安装不改变（对齐 demo UID 要求）。
const DEVICE_ID_FILE: &str = "dlna-device-id.json";
/// 列表功能配置文件（与 dlna-device-id.json 同目录）：{"playlistEnabled": true}
const CONFIG_FILE: &str = "dlna-config.json";

/// 构造 SSDP / description.xml 响应需要附加的扩展头
/// （BITMAP / BDLEPORT / UID / SERVICEID / X-User-Agent，对齐 demo 增强发现头）。
pub fn discovery_headers(control_port: Option<u16>, device_id: &str, service_id: &str) -> Vec<(String, String)> {
    let mut v = vec![
        // demo 实测格式为 "0x800"（DlnaServer extraHeaders），勿改成十进制：
        // 抖音若按 16 进制解析 "2048" 得 0x2048，& 0x800 == 0 会判不支持列表能力。
        (
            "BITMAP".to_string(),
            format!("0x{:x}", wire::FEATURE_BITMAP),
        ),
        ("UID".to_string(), device_id.to_string()),
        ("SERVICEID".to_string(), service_id.to_string()),
        ("X-User-Agent".to_string(), wire::X_USER_AGENT.to_string()),
    ];
    if let Some(p) = control_port {
        v.push((wire::CONTROL_PORT_HEADER.to_string(), p.to_string()));
    }
    v
}

/// 配置文件路径：优先 cwd（dev 调试），文件不存在则回退可执行文件目录（发布形态）。
/// 语义与原 device_id 持久化一致：cwd 已有目标文件 → 用 cwd；否则用 exe 目录（含创建）。
fn config_file(name: &str) -> Option<std::path::PathBuf> {
    if let Ok(d) = std::env::current_dir() {
        let p = d.join(name);
        if p.exists() {
            return Some(p);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            return Some(parent.join(name));
        }
    }
    None
}

fn config_path() -> Option<std::path::PathBuf> {
    config_file(DEVICE_ID_FILE)
}

/// 抖音播放列表功能总开关（2026-08-25 陈兄要求）：控制 BDLE TCP 列表通道 +
/// SSDP/HTTP 发现层抖音指纹/扩展头是否启用。**默认关闭**——抖音极速版对
/// PushMediaInfo 存在 `IllegalStateException: must not be null` 闪退风险
/// （崩溃堆栈已实锤，未根治前先提供开关规避；关闭后抖音投屏退回普通 DLNA
/// 单集链路，短视频 5s 伪装等非列表行为不受影响）。
/// 开启方式（任一优先）：
///  1) 环境变量 DOUYIN_PLAYLIST=1|true|yes|on（临时调试最方便）
///  2) 配置文件 dlna-config.json（与 dlna-device-id.json 同目录）: {"playlistEnabled": true}
pub fn playlist_enabled() -> bool {
    if let Ok(v) = std::env::var("DOUYIN_PLAYLIST") {
        let v = v.trim().to_ascii_lowercase();
        if !v.is_empty() {
            return v == "1" || v == "true" || v == "yes" || v == "on";
        }
    }
    if let Some(p) = config_file(CONFIG_FILE) {
        if let Ok(text) = std::fs::read_to_string(&p) {
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                if let Some(b) = v.get("playlistEnabled").and_then(|x| x.as_bool()) {
                    return b;
                }
            }
        }
    }
    false
}

/// 判断投屏 URL 是否为抖音系源——**addOn 激活判据**：只有抖音源 URL 才启用
/// 播放列表模式行为（如实上报 / auto_next 自动切集 / TrackURI 跟随），
/// 非抖音 URL 一律保持公版 DLNA 伪装链路（playlist_mode=false）。
/// 规则：host 精确后缀匹配（`host == suffix || host.ends_with("." + suffix)`，
/// 防 `evil-douyinvod.com.attacker.net` 子串误伤）+ `cast_type=ott_cast` 参数。
pub fn is_douyin_url(url: &str) -> bool {
    if url.contains("ott_cast") {
        return true;
    }
    let host = url
        .split("://")
        .nth(1)
        .unwrap_or(url)
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("");
    const DOUYIN_HOSTS: [&str; 9] = [
        "douyinvod.com",
        "douyincdn.com",
        "iesdouyin.com",
        "bytecdn.cn",
        "pstatp.com",
        "byteimg.com",
        "toutiaoimg.com",
        "toutiaovod.com",
        "ixigua.com",
    ];
    DOUYIN_HOSTS
        .iter()
        .any(|h| host == *h || host.ends_with(&format!(".{h}")))
}

/// 读取或生成稳定的数字 deviceId（重启保留；失败时退化为时间派生值，不阻断启动）。
pub fn device_id() -> String {
    if let Some(p) = config_path() {
        if let Ok(text) = std::fs::read_to_string(&p) {
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                if let Some(id) = v.get("deviceId").and_then(|x| x.as_str()) {
                    if !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()) {
                        return id.to_string();
                    }
                }
            }
        }
        let id = generate_device_id();
        let _ = std::fs::write(&p, json!({ "deviceId": id }).to_string());
        return id;
    }
    generate_device_id()
}

fn generate_device_id() -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // 纯数字字符串（demo 要求 deviceId 保持数字形式字符串）
    format!("{}{}", n % 100_000_000_000_000_000u128, random_digits(6))
}

fn random_digits(len: usize) -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| char::from(b'0' + rng.gen_range(0..10)))
        .collect()
}

// ---------------------------------------------------------------------------
// JSON 辅助（对齐 demo JsonValues：深度优先递归查找，容忍字段多包一层）
// ---------------------------------------------------------------------------
fn deep_find<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    match value {
        Value::Object(map) => {
            if let Some(v) = map.get(key) {
                return Some(v);
            }
            for v in map.values() {
                if let Some(found) = deep_find(v, key) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(arr) => {
            for v in arr {
                if let Some(found) = deep_find(v, key) {
                    return Some(found);
                }
            }
            None
        }
        _ => None,
    }
}

/// 顺序取第一个存在的字符串字段（deep_find + as_str）
fn first_str(obj: &Value, keys: &[&str]) -> Option<String> {
    for k in keys {
        if let Some(v) = deep_find(obj, k) {
            if let Some(s) = v.as_str() {
                if !s.is_empty() {
                    return Some(s.to_string());
                }
            }
        }
    }
    None
}

/// 只在 object **顶层**找字段（不递归）：列表命令的 startDramaId/dramaId 位于 body
/// 顶层，递归会误伤 dramaBeans 内部的同名键（把列表第一项当当前项）。
fn first_top_level_str(obj: &Value, keys: &[&str]) -> String {
    if let Some(map) = obj.as_object() {
        for k in keys {
            if let Some(v) = map.get(*k) {
                if let Some(s) = v.as_str() {
                    if !s.is_empty() {
                        return s.to_string();
                    }
                }
            }
        }
    }
    String::new()
}

/// 顺序取第一个存在的数字（deep_find + as_f64 / 字符串数字）
fn first_num(obj: &Value, keys: &[&str]) -> Option<f64> {
    for k in keys {
        if let Some(v) = deep_find(obj, k) {
            if let Some(n) = v.as_f64() {
                return Some(n);
            }
            if let Some(s) = v.as_str() {
                if let Ok(n) = s.parse::<f64>() {
                    return Some(n);
                }
            }
        }
    }
    None
}

/// PushMediaInfo 回传前规范化 dramaBeans 条目：抖音端（aweme.lite）处理
/// PushMediaInfo 时对条目做必填字段检查（顶层 dramaId / urlBeans），缺失即
/// `IllegalStateException: must not be null` 崩溃（demo 原样回传同样闪退，
/// 2026-08-25 用户抓堆栈实锤）。确保顶层必填字段存在（用 PlaylistItem 提取值
/// 兜底），同时保留原始全部字段不破坏其它结构。
fn normalize_bean(i: &PlaylistItem) -> Value {
    let mut bean = i.raw.clone();
    // 顶层 dramaId：缺失/空串 → 用提取值覆盖
    let top_did = bean
        .get("dramaId")
        .and_then(|v| v.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    if !top_did {
        bean["dramaId"] = json!(i.drama_id);
    }
    // 顶层 urlBeans：缺失/空数组 → 用提取 URL 构造标准条目（raw 顶层 url 优先，
    // PlaylistItem.url 兜底——extract_item 只查 urlBeans/urls，顶层 url 提取不到）
    let has_ub = bean
        .get("urlBeans")
        .and_then(|v| v.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    if !has_ub {
        let fallback_url = bean
            .get("url")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| (!i.url.is_empty()).then(|| i.url.clone()));
        if let Some(u) = fallback_url {
            bean["urlBeans"] = json!([{ "url": u, "isDefault": true }]);
        }
    }
    bean
}

/// 从 episode JSON 提取 PlaylistItem；url 选 isDefault 优先，没有则取第一条。
fn extract_item(v: &Value, fallback_index: usize) -> PlaylistItem {
    let drama_id = first_str(v, &["dramaId"]).unwrap_or_else(|| format!("item-{}", fallback_index));
    let title = first_str(v, &["title", "name", "episodeTitle"]).unwrap_or_default();
    let user_agent = first_str(v, &["userAgent", "ua"]).unwrap_or_default();
    let duration_ms = first_num(v, &["durationMs", "duration"])
        .map(|n| n.max(0.0) as u64)
        .unwrap_or(0);
    // 清晰度/线路：urlBeans 优先，兼容 urls
    let mut url = String::new();
    for bean_key in ["urlBeans", "urls"] {
        if let Some(beans) = deep_find(v, bean_key) {
            if let Some(arr) = beans.as_array() {
                for b in arr {
                    let is_default = deep_find(b, "isDefault")
                        .map(|d| d.as_bool().unwrap_or(false) || d.as_i64() == Some(1))
                        .unwrap_or(false);
                    if let Some(u) = first_str(b, &["url", "playUrl", "src"]) {
                        if is_default || url.is_empty() {
                            url = u;
                        }
                        if is_default {
                            break;
                        }
                    }
                }
                if !url.is_empty() {
                    break;
                }
            }
        }
    }
    PlaylistItem {
        drama_id,
        title,
        duration_ms,
        user_agent,
        url,
        raw: v.clone(),
    }
}

// ---------------------------------------------------------------------------
// 列表状态机（对齐 demo PlaylistState：LinkedHashMap 语义，dramaId 为 key）
// ---------------------------------------------------------------------------
#[derive(Default)]
pub struct PlaylistState {
    /// 保序去重：dramaId 到达顺序
    order: Vec<String>,
    /// dramaId -> item
    items: HashMap<String, PlaylistItem>,
    pub current_episode_id: String,
    pub status: String, // PLAYING / PAUSED / STOPPED
    pub speed: f64,
    pub volume: u32,
    /// 当前集播放进度（毫秒）——前端经 dlna_send_remote_event 上报同步，供
    /// TCP GetStatusInfo 返回实时值（抖音端双通道一致性校验用，写死 0 会被
    /// 判定设备状态异常而退出投屏，对齐 demo mediaState.duration/position）。
    pub duration: u64,
    pub position: u64,
    /// 上次 position 上报/重置时刻：status_info() 在 PLAYING 状态下按流逝时间
    /// 推进 position（1x 速度、封顶 duration），保证抖音端轮询永远看到"进度在走"
    /// ——前端上报链路可能断续/卡顿（实测 position 卡死在 559 被判定异常），
    /// 服务端推进作为兜底；前端每次 update_progress/切集重置基准。
    last_position_update: Option<std::time::Instant>,
}

impl PlaylistState {
    fn new() -> Self {
        Self {
            order: Vec::new(),
            items: HashMap::new(),
            current_episode_id: String::new(),
            status: "STOPPED".to_string(),
            speed: 1.0,
            volume: 50,
            duration: 0,
            position: 0,
            last_position_update: None,
        }
    }

    fn clear(&mut self) {
        self.order.clear();
        self.items.clear();
        self.current_episode_id.clear();
        self.status = "STOPPED".to_string();
        self.speed = 1.0;
        self.volume = 50;
        self.duration = 0;
        self.position = 0;
        self.last_position_update = None;
    }

    /// 当前列表项自带时长（毫秒，来自 dramaBeans 条目 durationMs/duration）。
    /// 切集/选集/起播后 GetStatusInfo **立即**返回它而不是 0：抖音端收到
    /// PushMediaInfo（播单当前集已切到新集）后会立刻轮询 GetStatusInfo 做
    /// "状态时长 vs 播单当前集时长"一致性校验，若返回 0 而播单新集时长非 0
    /// （前端起播新集、上报实时值之前有个窗口期），校验失败直接退出投屏
    /// （实测 client disconnected）。对齐 demo：播放器换源后 mediaState.duration
    /// 即时恢复实时值，不存在 0 窗口。
    fn current_duration(&self) -> u64 {
        self.items
            .get(&self.current_episode_id)
            .map(|i| i.duration_ms)
            .unwrap_or(0)
    }

    /// 前端进度上报同步（等价 demo 播放器回调更新 mediaState.duration/position）
    fn update_progress(&mut self, duration: u64, position: u64) {
        if duration > 0 {
            self.duration = duration;
        }
        self.position = position;
        // 前端实时上报 → 重置推进基准（status_info 从此刻起按流逝时间推进）
        self.last_position_update = Some(std::time::Instant::now());
    }

    fn has_items(&self) -> bool {
        !self.order.is_empty()
    }

    fn len(&self) -> usize {
        self.order.len()
    }

    fn current(&self) -> Option<&PlaylistItem> {
        let id = self.current_episode_id.as_str();
        if id.is_empty() {
            return None;
        }
        self.items.get(id)
    }

    /// Play / AddDramaList 合并：已存在更新、不存在按到达顺序追加；单项 Play 不清列表。
    /// apply_play(body, append)：
    ///  - `append=false`（Play 命令/首次起播）：current 切到 startDramaId/dramaId
    ///    （demo 语义——Play 是新播单，当前集由手机端指定）。
    ///  - `append=true`（AddDramaList 命令/预加载追加）：**不覆盖 current**——
    ///    current 由"当前在播集"决定（Play 或宿主 TV 切集已设定）。demo 覆盖
    ///    是因为其场景 startDramaId 恒等于 current（纯命令驱动、无外部切集）；
    ///    本场景若覆盖会把 TV 已切的新集打回旧集，前端被 emit 换源回退（实测乱）。
    fn apply_play(&mut self, body: &Value, append: bool) {
        let prev = self.current_episode_id.clone();
        for key in ["dramaBeans", "playlist"] {
            if let Some(beans) = deep_find(body, key).and_then(|v| v.as_array()) {
                for (i, ep) in beans.iter().enumerate() {
                    let item = extract_item(ep, i);
                    let id = item.drama_id.clone();
                    if !self.items.contains_key(&id) {
                        self.order.push(id.clone());
                    }
                    self.items.insert(id, item);
                }
                break;
            }
        }
        if !append {
            // 当前项：startDramaId > dramaId（**只在顶层找**——deep_find 会递归进
            // dramaBeans 内部把列表第一项的 dramaId 误当"当前项"。协议字段位置在
            // body 顶层，顶层查找足够）。
            let requested = first_top_level_str(body, &["startDramaId", "dramaId"]);
            if !requested.is_empty() && self.items.contains_key(&requested) {
                self.current_episode_id = requested;
            } else if self.current_episode_id.is_empty() && !self.order.is_empty() {
                self.current_episode_id = self.order[0].clone();
            }
        } else if self.current_episode_id.is_empty() && !self.order.is_empty() {
            self.current_episode_id = self.order[0].clone();
        }
        if let Some(speed) = first_num(body, &["speed"]) {
            self.speed = speed;
        }
        self.status = "PLAYING".to_string();
        // 当前集变化（起播/换集）→ position 归零，duration 预填列表项自带值
        if prev != self.current_episode_id {
            self.position = 0;
            self.duration = self.current_duration();
            self.last_position_update = Some(std::time::Instant::now());
        }
    }

    /// 对端推送的当前媒体信息同步
    fn apply_media_info(&mut self, body: &Value) {
        if let Some(info) = deep_find(body, "mediaInfo") {
            if let Some(speed) = first_num(info, &["speed"]) {
                self.speed = speed;
            }
            if let Some(id) = first_str(info, &["dramaId"]) {
                if self.items.contains_key(&id) {
                    self.current_episode_id = id;
                }
            }
        }
    }

    /// DeleteDramaList：dramaIds 数组或 dramaId
    fn delete(&mut self, body: &Value) {
        let mut removed: Vec<String> = Vec::new();
        if let Some(ids) = deep_find(body, "dramaIds").and_then(|v| v.as_array()) {
            for id in ids {
                if let Some(s) = id.as_str() {
                    removed.push(s.to_string());
                }
            }
        } else if let Some(id) = first_str(body, &["dramaId"]) {
            removed.push(id);
        }
        for id in removed {
            self.order.retain(|x| x != &id);
            self.items.remove(&id);
        }
        if !self.items.contains_key(&self.current_episode_id) {
            self.current_episode_id = self.order.first().cloned().unwrap_or_default();
        }
    }

    /// 按 dramaId 选集
    fn select(&mut self, id: &str) -> bool {
        if !id.is_empty() && self.items.contains_key(id) {
            self.current_episode_id = id.to_string();
            self.status = "PLAYING".to_string();
            // 选集 → position 归零，duration 预填列表项自带值
            self.position = 0;
            self.duration = self.current_duration();
            self.last_position_update = Some(std::time::Instant::now());
            true
        } else {
            false
        }
    }

    /// 上一项/下一项（PlayPreDrama=-1 / PlayNextDrama=1）
    fn move_by(&mut self, delta: i32) -> bool {
        if self.order.is_empty() {
            return false;
        }
        let current_index = self
            .order
            .iter()
            .position(|x| x == &self.current_episode_id)
            .unwrap_or(0);
        let target = current_index as i32 + delta;
        if target < 0 || target >= self.order.len() as i32 {
            return false;
        }
        let id = self.order[target as usize].clone();
        self.current_episode_id = id;
        self.status = "PLAYING".to_string();
        // 切集：position 归零（新集起点），duration 预填列表项自带值
        // （消除 0 窗口，见 current_duration 注释）
        self.position = 0;
        self.duration = self.current_duration();
        self.last_position_update = Some(std::time::Instant::now());
        true
    }

    /// 当前项完整 URL（供宿主起播/换源）
    #[allow(dead_code)]
    fn current_url(&self) -> Option<String> {
        self.current().map(|i| i.url.clone()).filter(|u| !u.is_empty())
    }

    /// 完整 mediaInfo（PushMediaInfo / GetMediaInfo 响应）。⚠️ 2026-08-25 崩溃堆栈实锤：
    /// 抖音端（aweme.lite）处理 PushMediaInfo 时 `IllegalStateException: must not
    /// be null`（ByteCastSourceController$excuteBdleMessage）——**demo 原样回传同样闪退**，
    /// 极速版对 dramaBeans 条目做必填字段检查（顶层 dramaId / urlBeans）。原样回传
    /// 抖音自己下发的条目不可靠（AddDramaList 追加的条目可能结构不完整/字段嵌套）。
    /// 因此**规范化每条**（见 normalize_bean）：确保顶层 dramaId 与 urlBeans 存在，
    /// 用 PlaylistItem 提取值兜底，同时保留原始全部字段。
    fn media_info(&self) -> Value {
        let beans: Vec<Value> = self
            .order
            .iter()
            .filter_map(|id| self.items.get(id))
            .map(|i| normalize_bean(i))
            .collect();
        json!({
            "dramaId": self.current_episode_id,
            "dramaBeans": beans,
            "uri": self.current_episode_id,
            "uuid": self.current_episode_id,
            "speed": self.speed,
            "speeds": [0.5, 0.75, 1.0, 1.25, 1.5, 2.0, 3.0],
            "loopMode": 0,
            "stretch": 0,
            "skip": 0
        })
    }

    /// GetStatusInfo 回包——**严格对齐 demo PlaylistState.statusInfo() 形态**：
    /// `duration` 恒 0、`position` 恒 0（demo 中 duration 无任何更新源、position
    /// 仅 Seek 命令更新），只有 status/speed 是实时值。2026-08-25 真机实测教训：
    /// 曾返回前端实时 duration（198167/300600），抖音端对 `duration>0` 会做
    /// GetStatusInfo 回包——**返回真实播放进度与时长**（陈兄 2026-08-25 17:29
    /// 明确要求，撤销"恒 0"形态）：duration 取内部字段（前端上报毫秒值）；
    /// position 取内部字段 + **PLAYING 状态下按流逝时间推进**（1x、封顶
    /// duration）——前端上报链路可能断续/卡顿（实测 position 卡死 559 被判定
    /// 异常），服务端推进保证抖音端轮询永远看到进度在走；前端每次
    /// update_progress / 切集重置基准。
    fn status_info(&self) -> Value {
        let mut pos = self.position;
        if self.status == "PLAYING" {
            if let Some(t) = self.last_position_update {
                let elapsed = t.elapsed().as_millis() as u64;
                if elapsed > 0 {
                    pos = (pos + elapsed).min(if self.duration > 0 { self.duration } else { u64::MAX });
                }
            }
        }
        json!({
            "status": self.status,
            "duration": self.duration,
            "position": pos,
            "speed": self.speed
        })
    }
}

// ---------------------------------------------------------------------------
// 会话加密（对齐 demo SessionCrypto：X25519 协商 + AES-128-GCM）
// 密文布局：0xBC | 0x10 | IV(16) | ciphertext | GCM tag(16)，整体 Base64
// ---------------------------------------------------------------------------
struct SessionCrypto {
    secret: StaticSecret,
    public_b64: String,
}

/// 协议密文布局为 0xBC | 0x10 | IV(16) | ciphertext | tag(16)，IV 是 **16 字节**——
/// 标准 Aes128Gcm 的 Nonce 是 12 字节，必须用 AesGcm<Aes128, U16> 泛型对齐。
type Aes128Gcm16 = AesGcm<aes_gcm::aes::Aes128, U16>;

impl SessionCrypto {
    fn new() -> Self {
        let secret = StaticSecret::random_from_rng(&mut OsRng);
        let public = PublicKey::from(&secret);
        Self {
            secret,
            public_b64: base64::encode(public.as_bytes()),
        }
    }

    /// shared secret 取第 0,2,4,...,30 字节构成 16 字节 AES key
    fn derive_aes_key(&self, peer_public_b64: &str) -> Option<[u8; 16]> {
        let peer_bytes = base64::decode(peer_public_b64).ok()?;
        let arr: [u8; 32] = peer_bytes.as_slice().try_into().ok()?;
        let peer_public = PublicKey::from(arr);
        let shared = self.secret.diffie_hellman(&peer_public);
        let mut key = [0u8; 16];
        for i in 0..16 {
            key[i] = shared.as_bytes()[i * 2];
        }
        Some(key)
    }

    fn decrypt(&self, key: &[u8; 16], encoded: &str) -> Result<Vec<u8>, String> {
        let data = base64::decode(encoded).map_err(|e| format!("base64: {e}"))?;
        if data.len() < 34 || data[0] != 0xBC || data[1] != 0x10 {
            return Err("invalid encrypted playlist message".into());
        }
        let iv = &data[2..18];
        let ciphertext = &data[18..];
        let cipher = Aes128Gcm16::new_from_slice(key).map_err(|e| format!("key: {e:?}"))?;
        cipher
            .decrypt(Nonce::<U16>::from_slice(iv), ciphertext)
            .map_err(|e| format!("aes-gcm: {e:?}"))
    }

    /// 加密（当前回包统一 encrypt=0 明文，仅测试/未来 encrypt=1 回包用）
    #[allow(dead_code)]
    fn encrypt(&self, key: &[u8; 16], plaintext: &str) -> Result<String, String> {
        let mut iv = [0u8; 16];
        OsRng.fill_bytes(&mut iv);
        let cipher = Aes128Gcm16::new_from_slice(key).map_err(|e| format!("key: {e:?}"))?;
        let ciphertext = cipher
            .encrypt(Nonce::<U16>::from_slice(&iv), plaintext.as_bytes())
            .map_err(|e| format!("aes-gcm: {e:?}"))?;
        let mut formatted = Vec::with_capacity(2 + 16 + ciphertext.len());
        formatted.push(0xBC);
        formatted.push(0x10);
        formatted.extend_from_slice(&iv);
        formatted.extend_from_slice(&ciphertext);
        Ok(base64::encode(&formatted))
    }
}

// ---------------------------------------------------------------------------
// 列表通道（对齐 demo PlaylistLinkServer：拆帧 / 握手 / 命令分发 / PushMediaInfo）
// ---------------------------------------------------------------------------
/// 列表当前项变化时的宿主回调（Play/AddDramaList/选集命令处理后触发，携带新 URL+标题）
pub type PlayRequestCallback = Box<dyn Fn(&PlaylistItem) + Send + Sync>;

pub struct PlaylistChannel {
    inner: Arc<ChannelInner>,
}

struct ChannelInner {
    state: Mutex<PlaylistState>,
    /// 设备身份（ip 会随 WiFi 切换热更新，见 set_ip；其余字段生命周期内不变）
    device: Mutex<PlaylistDeviceIdentity>,
    crypto: SessionCrypto,
    /// 已连接客户端的写半（响应 + PushMediaInfo 广播共用；每连接一把锁保证帧不交错）
    clients: tokio::sync::Mutex<Vec<(std::net::SocketAddr, Arc<tokio::sync::Mutex<OwnedWriteHalf>>)>>,
    /// 已通知过宿主的 url（防重复 emit；URL 变化才再次通知）
    last_notified_url: Mutex<String>,
    /// 上次自动切集时刻（防抖：position 连续上报 dur+1000 只触发一次）
    last_auto_next: Mutex<Option<std::time::Instant>>,
    running: AtomicBool,
    port: Mutex<Option<u16>>,
    on_play_request: PlayRequestCallback,
}

impl PlaylistChannel {
    /// 构造并启动：先 bind（45165 优先，被占退化系统端口），再起 accept 循环。
    /// 启动成功后调用方须把 control_port 写入 SSDP/HTTP 的 BDLEPORT 头（顺序不能反）。
    pub async fn start(
        device: PlaylistDeviceIdentity,
        shutdown: broadcast::Receiver<()>,
        on_play_request: PlayRequestCallback,
    ) -> std::io::Result<Arc<Self>> {
        let listener = match TcpListener::bind(("0.0.0.0", wire::PREFERRED_PORT)).await {
            Ok(l) => l,
            Err(_) => {
                eprintln!(
                    "[dlna_playlist] preferred port {} occupied, fallback to ephemeral",
                    wire::PREFERRED_PORT
                );
                TcpListener::bind(("0.0.0.0", 0)).await?
            }
        };
        let port = listener.local_addr()?.port();
        let channel = Arc::new(PlaylistChannel {
            inner: Arc::new(ChannelInner {
                state: Mutex::new(PlaylistState::new()),
                device: Mutex::new(device),
                crypto: SessionCrypto::new(),
                clients: tokio::sync::Mutex::new(Vec::new()),
                last_notified_url: Mutex::new(String::new()),
                last_auto_next: Mutex::new(None),
                running: AtomicBool::new(true),
                port: Mutex::new(Some(port)),
                on_play_request,
            }),
        });
        let ch = channel.clone();
        tokio::spawn(async move {
            ch.accept_loop(listener, shutdown).await;
        });
        eprintln!(
            "[dlna_playlist] channel started host={} port={} frame=uint32LE+JSON crypto=X25519/AES-128-GCM",
            channel.inner.device.lock().unwrap().ip, port
        );
        Ok(channel)
    }

    pub fn control_port(&self) -> u16 {
        self.inner.port.lock().unwrap().unwrap_or(0)
    }

    /// WiFi 切换后热更新设备 IP（宿主在 SSDP 重绑定成功后回调）。
    /// 后续 GetDeviceInfo 握手 / PushMediaInfo 上报即携带新 IP，避免手机端
    /// 列表通道拿着旧 IP 连不上。
    pub fn set_ip(&self, ip: String) {
        self.inner.device.lock().unwrap().ip = ip;
    }

    /// 停止：关闭监听与所有客户端连接，清空列表。
    pub fn stop(&self) {
        self.inner.running.store(false, Ordering::SeqCst);
        *self.inner.port.lock().unwrap() = None;
        self.inner.state.lock().unwrap().clear();
    }

    #[allow(dead_code)]
    pub fn is_running(&self) -> bool {
        self.inner.running.load(Ordering::SeqCst)
    }

    /// 列表当前状态快照（宿主 UI / 诊断）
    #[allow(dead_code)]
    pub fn snapshot(&self) -> Value {
        let st = self.inner.state.lock().unwrap();
        let items: Vec<Value> = st
            .order
            .iter()
            .filter_map(|id| st.items.get(id))
            .map(|i| {
                json!({
                    "dramaId": i.drama_id,
                    "title": i.title,
                    "durationMs": i.duration_ms,
                    "url": i.url,
                })
            })
            .collect();
        json!({
            "currentEpisodeId": st.current_episode_id,
            "status": st.status,
            "count": st.len(),
            "items": items,
        })
    }

    /// 当前列表是否非空（宿主判断"列表模式"：手机已通过通道下发过列表）
    pub fn has_playlist(&self) -> bool {
        self.inner.state.lock().unwrap().has_items()
    }

    /// 前端进度上报同步 → TCP GetStatusInfo 返回实时 duration/position
    ///（抖音端双通道一致性校验：写死 0 判定设备状态异常退出投屏）。
    pub fn update_progress(&self, duration: u64, position: u64) {
        self.inner.state.lock().unwrap().update_progress(duration, position);
    }

    /// 当前项 URL（宿主起播/比较用）
    #[allow(dead_code)]
    pub fn current_url(&self) -> Option<String> {
        self.inner.state.lock().unwrap().current_url()
    }

    /// 播完/手动切集：切到下一项并返回新当前项（无下一项返回 None）。
    /// 防抖：1s 内不重复切（前端播完兜底 position=dur+1000 可能连报，且换集 STOP 兜底也走这里）。
    /// `notify=true` 时内部触发 on_play_request 回调（供普通通道切集路径用）；
    /// `notify=false` 时只切集不回调（供 auto_next 用——它需要先做 addOn URL
    /// 守卫再自行 set_uri/emit，避免守卫前回调已把非抖音 URL 播出去、且双 emit）。
    /// 播完/手动切集：切到下一项并返回新当前项（无下一项返回 None）。
    /// `notify=true` 时内部触发 on_play_request 回调（供普通通道切集路径用）；
    /// `notify=false` 时只切集不回调（供 auto_next 用——它需要先做 addOn URL
    /// 守卫再自行 set_uri/emit，避免守卫前回调已把非抖音 URL 播出去、且双 emit）。
    /// `force=true` 跳过 1s 防抖（**手动切集**：TV 遥控 next 是用户明确意图，
    /// 必须即时生效，否则会被播完自动切集刚触发过的防抖窗口吞掉——实测
    /// "按了没反应、再按才切"就是防抖误吞）；`force=false` 保留防抖（播完
    /// 自动切集：前端 position 连报可能重复触发，需 1s 内去重）。
    pub fn next_and_get(&self, notify: bool, force: bool) -> Option<PlaylistItem> {
        if !force {
            let now = std::time::Instant::now();
            let mut last = self.inner.last_auto_next.lock().unwrap();
            if let Some(t) = *last {
                if t.elapsed() < std::time::Duration::from_millis(1000) {
                    return None;
                }
            }
            *last = Some(now);
        }
        let mut st = self.inner.state.lock().unwrap();
        if !st.move_by(1) {
            return None;
        }
        let item = st.current().cloned();
        drop(st);
        if notify {
            if let Some(i) = &item {
                self.notify_play_request(i);
            }
        }
        item
    }

    /// 上一项（PlayPreDrama 命令 / 宿主上键切集）
    #[allow(dead_code)]
    pub fn prev(&self) -> Option<PlaylistItem> {
        let mut st = self.inner.state.lock().unwrap();
        if !st.move_by(-1) {
            return None;
        }
        let item = st.current().cloned();
        drop(st);
        if let Some(i) = &item {
            self.notify_play_request(i);
        }
        item
    }

    fn notify_play_request(&self, item: &PlaylistItem) {
        let mut last = self.inner.last_notified_url.lock().unwrap();
        if *last == item.url {
            return;
        }
        *last = item.url.clone();
        (self.inner.on_play_request)(item);
    }

    /// 主动推送 PushMediaInfo 到所有客户端（手机重连后仍能拿到最新列表）
    pub async fn push_media_info(&self) {
        let info = self.inner.state.lock().unwrap().media_info();
        let drama_id = info["dramaId"].as_str().unwrap_or("").to_string();
        let beans = info["dramaBeans"].as_array().map(|a| a.len()).unwrap_or(0);
        // 回包结构日志（2026-08-25 崩溃堆栈后加）：抖音端 excuteBdleMessage 对
        // dramaBeans 条目做必填字段检查，缺失 → IllegalStateException: must not be null
        // 闪退。打印当前集条目（dramaId 匹配）与首条目的结构，确认规范化后顶层
        // dramaId/urlBeans 是否就位。
        let cur_bean = info["dramaBeans"]
            .as_array()
            .and_then(|a| {
                a.iter().find(|b| {
                    b.get("dramaId").and_then(|d| d.as_str()) == Some(drama_id.as_str())
                })
            })
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let first_bean = info["dramaBeans"].get(0).cloned().unwrap_or(serde_json::Value::Null);
        let summarize = |b: &Value| -> String {
            let did = b.get("dramaId").and_then(|v| v.as_str()).unwrap_or("<null>");
            let ub = b
                .get("urlBeans")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            let url = b
                .get("urlBeans")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|u| u.get("url"))
                .and_then(|u| u.as_str())
                .map(|u| u.chars().take(48).collect::<String>())
                .unwrap_or("<null>".into());
            format!("dramaId={} urlBeans={} url0={}", did, ub, url)
        };
        eprintln!(
            "[dlna_playlist] push_media_info → dramaId={} beans={} curBean[{}] firstBean[{}]",
            drama_id,
            beans,
            if cur_bean.is_null() { "NOT-FOUND".to_string() } else { summarize(&cur_bean) },
            summarize(&first_bean)
        );
        let body = json!({ "cmd": "PushMediaInfo", "mediaInfo": info });
        let frame = build_frame(&json!({ "encrypt": 0, "content": json!({
            "version": 1,
            "messageId": new_message_id(),
            "body": body,
        })}));
        let clients = self.inner.clients.lock().await;
        let mut failed: Vec<std::net::SocketAddr> = Vec::new();
        for (peer, wh) in clients.iter() {
            let mut guard = wh.lock().await;
            if guard.write_all(&frame).await.is_err() {
                failed.push(*peer);
            } else {
                let _ = guard.flush().await;
            }
        }
        drop(clients);
        if !failed.is_empty() {
            let mut clients = self.inner.clients.lock().await;
            clients.retain(|(p, _)| !failed.contains(p));
        }
    }

    async fn accept_loop(&self, listener: TcpListener, mut shutdown: broadcast::Receiver<()>) {
        loop {
            tokio::select! {
                _ = shutdown.recv() => break,
                res = listener.accept() => {
                    match res {
                        Ok((stream, peer)) => {
                            let ch = self.clone();
                            tokio::spawn(async move {
                                let _ = ch.handle_client(stream, peer).await;
                            });
                        }
                        Err(e) => {
                            eprintln!("[dlna_playlist] accept failed: {e}");
                            break;
                        }
                    }
                }
            }
        }
        eprintln!("[dlna_playlist] channel stopped");
    }

    async fn handle_client(&self, stream: TcpStream, peer: std::net::SocketAddr) -> std::io::Result<()> {
        let _ = stream.set_nodelay(true);
        let (mut read_half, write_half) = stream.into_split();
        // 写半同时用于「本连接响应」与「PushMediaInfo 广播」，每连接一把锁保证帧不交错
        let write_guard = Arc::new(tokio::sync::Mutex::new(write_half));
        let remote = format!("{peer}");
        eprintln!("[dlna_playlist] client connected {remote}");
        self.inner.clients.lock().await.push((peer, write_guard.clone()));

        let mut aes_key: Option<[u8; 16]> = None;
        let result: std::io::Result<()> = async {
            loop {
                let frame = match read_frame(&mut read_half).await {
                    Ok(f) => f,
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                    Err(e) => return Err(e),
                };
                let envelope: Value = match serde_json::from_slice(&frame) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("[dlna_playlist] bad json from {remote}: {e}");
                        continue;
                    }
                };
                let encrypted = envelope.get("encrypt").and_then(|x| x.as_i64()).unwrap_or(0) == 1;
                let content: Value = if encrypted {
                    let key = match aes_key {
                        Some(k) => k,
                        None => {
                            eprintln!("[dlna_playlist] encrypted frame before key established {remote}");
                            continue;
                        }
                    };
                    let encoded = envelope.get("content").and_then(|x| x.as_str()).unwrap_or("");
                    let plain = match self.inner.crypto.decrypt(&key, encoded) {
                        Ok(p) => p,
                        Err(e) => {
                            eprintln!("[dlna_playlist] decrypt failed {remote}: {e}");
                            continue;
                        }
                    };
                    match serde_json::from_slice(&plain) {
                        Ok(v) => v,
                        Err(e) => {
                            eprintln!("[dlna_playlist] bad decrypted json {remote}: {e}");
                            continue;
                        }
                    }
                } else {
                    envelope.get("content").cloned().unwrap_or(Value::Null)
                };

                // 手机 X25519 公钥在 content.sourceInfo（可能多包一层），递归查找
                if let Some(peer_key) = first_str(&content, &["preSharedKey"]) {
                    aes_key = self.inner.crypto.derive_aes_key(&peer_key);
                    eprintln!(
                        "[dlna_playlist] session key established remote={remote} peerKeyLength={}",
                        peer_key.len()
                    );
                }

                let message_id = content.get("messageId").and_then(|x| x.as_str()).unwrap_or("").to_string();
                let version = content.get("version").and_then(|x| x.as_i64()).unwrap_or(1);
                let body = content.get("body").cloned().unwrap_or(Value::Null);
                let command = body.get("cmd").and_then(|x| x.as_str()).unwrap_or("").to_string();

                // 对端带 code 的包是响应，只记录不再回 ACK（防回包循环）
                if content.get("code").is_some() {
                    eprintln!("[dlna_playlist] recv response cmd={command} messageId={message_id}");
                    continue;
                }
                eprintln!("[dlna_playlist] recv cmd={command} messageId={message_id} encrypted={encrypted}");

                let (response_body, need_push) = self.handle_command(&command, &body, &content, &remote);
                // 写回 ACK（同 messageId + code=0）
                let resp_content = json!({ "version": version, "messageId": message_id, "code": 0, "body": response_body });
                let resp_frame = build_frame(&json!({ "encrypt": 0, "content": resp_content }));
                {
                    let mut guard = write_guard.lock().await;
                    guard.write_all(&resp_frame).await?;
                    guard.flush().await?;
                }
                // 命令自身需要推（Play/AddDramaList/切集命令）→ 回包后推一帧。
                // 切集命令（PlayNextDrama 等）由抖音端自己发起 → 天然命令背书，
                // 抖音端接受 PushMediaInfo 播单更新（2026-08-25 实测）。
                if need_push {
                    self.push_media_info().await;
                }
            }
            Ok(())
        }
        .await;

        self.inner.clients.lock().await.retain(|(p, _)| p != &peer);
        eprintln!("[dlna_playlist] client disconnected {remote} result={result:?}");
        result
    }

    fn handle_command(&self, command: &str, body: &Value, content: &Value, remote: &str) -> (Value, bool) {
        let response = json!({ "cmd": command });
        let need_push = matches!(
            command,
            "Play" | "AddDramaList" | "DeleteDramaList" | "PlayPreDrama" | "PlayNextDrama" | "PlayDramaId" | "PlayDramaList2" | "SetSpeed"
        );
        // GetDeviceInfo 不需要 state 锁，最先处理
        if command == "GetDeviceInfo" {
            self.log_sender(content, remote);
            return (self.device_info(), false);
        }
        let mut st = self.inner.state.lock().unwrap();
        match command {
            "GetStatusInfo" => {
                let si = st.status_info();
                // 回包内容日志：抖音端断投前最后一次轮询看到的值是退出判定依据
                eprintln!(
                    "[dlna_playlist] GetStatusInfo resp → {}",
                    si.to_string().replace(' ', "")
                );
                (json!({ "cmd": command, "statusInfo": si }), false)
            }
            "GetMediaInfo" => (json!({ "cmd": command, "mediaInfo": st.media_info() }), false),
            "GetStatusAndMediaInfo" => (
                json!({ "cmd": command, "statusInfo": st.status_info(), "mediaInfo": st.media_info() }),
                false,
            ),
            "GetVolume" => (json!({ "cmd": command, "volume": st.volume }), false),
            "Play" => {
                // 首次起播：current 切到 startDramaId/dramaId（demo 语义）
                st.apply_play(body, false);
                let item = st.current().cloned();
                drop(st);
                if let Some(i) = &item {
                    self.notify_play_request(i);
                }
                (response, need_push)
            }
            "AddDramaList" => {
                // 追加列表（预加载）：**不覆盖 current**——current 由"当前在播集"
                // 决定（Play 命令或宿主 TV 切集已设定）。demo 覆盖是因为其场景
                // startDramaId 恒等于 current（命令驱动、无外部切集）；本场景若
                // 覆盖会把 TV 已切的新集打回旧集，前端被 emit 换源回退（实测乱）。
                st.apply_play(body, true);
                let item = st.current().cloned();
                drop(st);
                if let Some(i) = &item {
                    self.notify_play_request(i);
                }
                (response, need_push)
            }
            "DeleteDramaList" => {
                st.delete(body);
                (response, need_push)
            }
            "ClearDramaList" => {
                st.clear();
                eprintln!("[dlna_playlist] 手机清空了视频列表");
                (response, false)
            }
            "PlayPreDrama" | "PlayNextDrama" => {
                // 检查 move_by 返回值：到列表边界（无上一项/下一项）时**不得**用
                // current() 回退项触发 notify——否则 last_notified_url 去重会把
                // 边界按下静默吞掉（看起来"没反应"），且客户端收到的仍是当前项。
                let moved = if command == "PlayPreDrama" {
                    st.move_by(-1)
                } else {
                    st.move_by(1)
                };
                let item = if moved { st.current().cloned() } else { None };
                drop(st);
                if let Some(i) = &item {
                    self.notify_play_request(i);
                }
                (response, need_push)
            }
            "PlayDramaId" | "PlayDramaList2" => {
                let id = first_str(body, &["dramaId", "startDramaId"]).unwrap_or_default();
                st.select(&id);
                let item = st.current().cloned();
                drop(st);
                if let Some(i) = &item {
                    self.notify_play_request(i);
                }
                (response, need_push)
            }
            "SetSpeed" => {
                if let Some(speed) = first_num(body, &["speed"]) {
                    st.speed = speed;
                }
                (response, need_push)
            }
            "Seek" => {
                // 列表通道的 Seek 仅同步状态（对齐 demo mediaState.position 更新）；
                // 真正的播放器 Seek 由 DLNA SOAP 处理。
                if let Some(p) = first_num(body, &["position", "seekPosition"]) {
                    st.position = p.max(0.0) as u64;
                }
                (response, false)
            }
            "Pause" => {
                st.status = "PAUSED".to_string();
                (response, false)
            }
            "Resume" => {
                st.status = "PLAYING".to_string();
                (response, false)
            }
            "Stop" => {
                st.status = "STOPPED".to_string();
                (response, false)
            }
            "SetVolume" => {
                if let Some(v) = first_num(body, &["volume"]) {
                    st.volume = v.max(0.0).min(100.0) as u32;
                }
                (response, false)
            }
            "AddVolume" => {
                st.volume = (st.volume + 1).min(100);
                (response, false)
            }
            "SubVolume" => {
                st.volume = st.volume.saturating_sub(1);
                (response, false)
            }
            "PushMediaInfo" => {
                st.apply_media_info(body);
                (response, false)
            }
            // 展示参数 / 保活等：记录后通用 ACK
            "SetDanmaku" | "SetSubtitle" | "SetResolution" | "SetLoopMode" | "SetStretchMode"
            | "SetStretch" | "SetInheritConfig" | "SetSkipInfo" | "PushStatusInfo"
            | "PushRuntimeInfo" | "Heartbeat" => (response, false),
            _ => {
                eprintln!("[dlna_playlist] 未知命令 {command}（已记录并返回通用确认）");
                (response, false)
            }
        }
    }

    fn device_info(&self) -> Value {
        let d = self.inner.device.lock().unwrap();
        json!({
            "deviceInfo": {
                "ip": d.ip,
                "cpu": "arm64",
                "width": 1920,
                "height": 1080,
                "fps": 30,
                "supportedCodecs": [
                    { "mimeType": "video/avc", "codecs": "H264", "width": 1920, "height": 1080, "fps": 30 }
                ],
                "name": d.name,
                "platform": "android",
                "packageName": d.package_name,
                "deviceId": d.device_id,
                "appName": d.name,
                "appVersion": "0.1.0",
                "osVersion": d.os_version,
                "deviceModel": d.device_model,
                "deviceBrand": d.device_brand,
                "bitmap": wire::FEATURE_BITMAP,
                "encryptVersion": "1.0",
                "preSharedKey": self.inner.crypto.public_b64,
                "copyright": wire::PROTOCOL_COPYRIGHT,
                "version": wire::PROTOCOL_VERSION,
            },
            "cmd": "GetDeviceInfo"
        })
    }

    fn log_sender(&self, content: &Value, remote: &str) {
        let source = content
            .get("sourceInfo")
            .filter(|v| v.is_object())
            .cloned()
            .unwrap_or_else(|| content.clone());
        let name = first_str(&source, &["name", "deviceName"]).unwrap_or_else(|| "未知手机".into());
        let platform = first_str(&source, &["platform"]).unwrap_or_default();
        let package = first_str(&source, &["packageName"]).unwrap_or_default();
        eprintln!("[dlna_playlist] 发送端={name} ip={remote} platform={platform} package={package}");
    }
}

impl Clone for PlaylistChannel {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// 帧编解码（uint32LE + UTF-8 JSON）
// ---------------------------------------------------------------------------
fn build_frame(value: &Value) -> Vec<u8> {
    let payload = value.to_string().into_bytes();
    let len = payload.len();
    let mut frame = Vec::with_capacity(len + 4);
    frame.push((len & 0xff) as u8);
    frame.push(((len >> 8) & 0xff) as u8);
    frame.push(((len >> 16) & 0xff) as u8);
    frame.push(((len >> 24) & 0xff) as u8);
    frame.extend_from_slice(&payload);
    frame
}

/// 读一帧：readExact(4) → readExact(length)
async fn read_frame(reader: &mut OwnedReadHalf) -> std::io::Result<Vec<u8>> {
    let mut header = [0u8; 4];
    reader.read_exact(&mut header).await?;
    let length = u32::from_le_bytes(header) as usize;
    if length == 0 || length > wire::MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid frame length {length}"),
        ));
    }
    let mut buf = vec![0u8; length];
    reader.read_exact(&mut buf).await?;
    Ok(buf)
}

fn new_message_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{n:x}{}", random_digits(6))
}

// ---------------------------------------------------------------------------
// 单元测试
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    // addOn 激活判据：抖音源 URL 命中（域名/ott_cast），非抖音 URL 拒绝。
    // 这是"公版零污染"的总闸——规则变更必须同步这里。
    #[test]
    fn is_douyin_url_judges_by_source() {
        // 域名命中（douyinvod.com 直链带 cast_type=ott_cast 参数）
        assert!(is_douyin_url("http://v27-wha.douyinvod.com/abc/video/tos/x.mp4?cast_type=ott_cast&a=1128"));
        // 纯 ott_cast 参数（未知域名但走抖音投屏协议）
        assert!(is_douyin_url("http://cdn.example.com/x.mp4?cast_type=ott_cast"));
        // 其他抖音系域名
        assert!(is_douyin_url("http://a.iesdouyin.com/video/tos/x.mp4"));
        assert!(is_douyin_url("https://v3.toutiaovod.com/x.mp4"));
        // 公版源：非抖音域名、无 ott_cast → 拒绝（不激活列表模式）
        assert!(!is_douyin_url("http://192.168.40.64:8000/movies/foo.mp4"));
        assert!(!is_douyin_url("https://cdn.qq.com/video/x.mp4"));
        assert!(!is_douyin_url("https://www.bilibili.com/video/av1"));
        assert!(!is_douyin_url("http://example.com/x.mp4?cast_type=whatever"));
        // 边界：子串不能误伤（douyinvod 域名完整匹配）
        assert!(!is_douyin_url("http://evil-douyinvod.com.attacker.net/x.mp4"));
    }

    fn item_json(id: &str, url: &str) -> Value {
        json!({
            "dramaId": id,
            "title": format!("视频 {id}"),
            "duration": 8000,
            "urlBeans": [
                { "url": url, "resolution": "超清", "isDefault": 1 },
                { "url": format!("{url}?hd=0"), "resolution": "标清" }
            ]
        })
    }

    #[test]
    fn discovery_headers_bitmap_uses_demo_hex_format() {
        // demo 实测 HTTP 头格式为 "0x800"（非十进制），防回归。
        let hs = discovery_headers(Some(45165), "12345", "svc-1");
        let bitmap = hs.iter().find(|(k, _)| k == "BITMAP").unwrap();
        assert_eq!(bitmap.1, "0x800");
    }

    #[test]
    #[test]
    fn normalize_bean_ensures_top_level_required_fields() {
        // 防回归（2026-08-25 崩溃堆栈）：PushMediaInfo 的 dramaBeans 条目必须
        // 有顶层 dramaId / urlBeans，否则抖音端（aweme.lite）excuteBdleMessage
        // checkNotNull 崩溃。原样回传不可靠（demo 同样闪退）。
        // ① 顶层 dramaId 缺失 → 用提取值补齐
        let raw = json!({ "video": { "dramaId": "x1" }, "urlBeans": [{ "url": "http://x/1.mp4", "isDefault": true }] });
        let item = extract_item(&raw, 0);
        let bean = normalize_bean(&item);
        assert_eq!(bean["dramaId"], "x1", "顶层 dramaId 补齐");
        assert!(bean.get("urlBeans").is_some(), "urlBeans 保留");
        // ② urlBeans 缺失 → 用提取 URL 构造标准条目
        let raw2 = json!({ "dramaId": "x2", "url": "http://x/2.mp4" });
        let item2 = extract_item(&raw2, 1);
        let bean2 = normalize_bean(&item2);
        assert_eq!(bean2["dramaId"], "x2");
        assert_eq!(bean2["urlBeans"][0]["url"], "http://x/2.mp4");
        assert_eq!(bean2["urlBeans"][0]["isDefault"], true);
        // ③ 完整条目 → 原样保留不破坏
        let raw3 = json!({ "dramaId": "x3", "urlBeans": [{ "url": "http://x/3.mp4", "isDefault": true }], "title": "t3" });
        let item3 = extract_item(&raw3, 2);
        let bean3 = normalize_bean(&item3);
        assert_eq!(bean3["title"], "t3");
        assert_eq!(bean3["dramaId"], "x3");
        assert_eq!(bean3["urlBeans"][0]["url"], "http://x/3.mp4");
    }

    #[test]
    fn extract_item_picks_default_url() {
        let item = extract_item(&item_json("a", "http://x/a.mp4"), 0);
        assert_eq!(item.drama_id, "a");
        assert_eq!(item.url, "http://x/a.mp4");
        assert_eq!(item.duration_ms, 8000);
        assert_eq!(item.title, "视频 a");
    }

    #[test]
    fn apply_play_merges_by_drama_id_keeping_order() {
        let mut st = PlaylistState::new();
        st.apply_play(&json!({ "dramaId": "1", "dramaBeans": [item_json("1", "http://x/1.mp4")] }), false);
        assert_eq!(st.len(), 1);
        assert_eq!(st.current_episode_id, "1");

        // AddDramaList 追加
        let add = json!({ "dramaBeans": [item_json("2", "http://x/2.mp4"), item_json("3", "http://x/3.mp4")] });
        st.apply_play(&add, true);
        assert_eq!(st.len(), 3);
        assert_eq!(st.current_episode_id, "1", "追加列表不清当前项");

        // 单项 Play 更新已存在 id 不重复计数
        let re_play = json!({ "dramaId": "2", "dramaBeans": [item_json("2", "http://x/2b.mp4")] });
        st.apply_play(&re_play, false);
        assert_eq!(st.len(), 3, "已存在 id 更新不重复计数");
        assert_eq!(st.current().unwrap().url, "http://x/2b.mp4");
    }

    #[test]
    fn move_by_selects_next_and_prev() {
        let mut st = PlaylistState::new();
        st.apply_play(&json!({ "dramaBeans": [item_json("1", "u1"), item_json("2", "u2"), item_json("3", "u3")] }), false);
        assert_eq!(st.current_episode_id, "1");
        assert!(st.move_by(1));
        assert_eq!(st.current_episode_id, "2");
        assert!(st.move_by(1));
        assert_eq!(st.current_episode_id, "3");
        assert!(!st.move_by(1), "末尾无下一项");
        assert!(st.move_by(-1));
        assert_eq!(st.current_episode_id, "2");
        assert!(!st.move_by(-10), "开头无上一项");
    }

    #[test]
    fn add_drama_list_does_not_override_current() {
        // 防回归：TV 切集（move_by 到 2）后抖音端 AddDramaList（带旧 startDramaId=1）
        // **不得**把 current 打回 1——否则前端被 emit 换源回退（实测乱）。
        // demo 覆盖是因为其场景 startDramaId 恒等于 current；本场景有外部切集。
        let mut st = PlaylistState::new();
        st.apply_play(&json!({ "dramaBeans": [item_json("1", "u1"), item_json("2", "u2"), item_json("3", "u3")] }), false);
        assert!(st.move_by(1));
        assert_eq!(st.current_episode_id, "2", "TV 切到第 2 集");
        // 抖音端 AddDramaList 预加载，startDramaId=1（它 UI 还显示第 1 集）
        let add = json!({ "startDramaId": "1", "dramaBeans": [item_json("4", "u4"), item_json("5", "u5")] });
        st.apply_play(&add, true);
        assert_eq!(st.current_episode_id, "2", "AddDramaList 不覆盖 current（保留 TV 切集）");
        assert_eq!(st.len(), 5, "列表追加成功");
        // Play 命令（首次起播）仍遵循 startDramaId（demo 语义）
        st.apply_play(&json!({ "startDramaId": "4", "dramaBeans": [item_json("4", "u4")] }), false);
        assert_eq!(st.current_episode_id, "4", "Play 命令遵循 startDramaId");
    }

    #[test]
    fn switch_episode_prefills_duration_from_item() {
        // statusInfo **返回真实播放进度与时长**（陈兄 2026-08-25 17:29 要求）：
        // duration/position 取内部字段——起播/切集时预填列表项值（或 0），
        // 前端实时上报（update_progress）覆盖为精确毫秒值。
        let mut st = PlaylistState::new();
        st.apply_play(&json!({ "dramaBeans": [item_json("1", "u1"), item_json("2", "u2")] }), false);
        assert_eq!(st.status_info()["status"], "PLAYING");
        assert_eq!(st.status_info()["duration"], 8000, "起播即预填列表项时长");
        assert_eq!(st.status_info()["position"], 0);
        assert_eq!(st.duration, 8000, "内部 duration 起播即预填列表项值");
        assert!(st.move_by(1));
        assert_eq!(st.status_info()["duration"], 8000, "切集后预填新集值");
        assert_eq!(st.status_info()["position"], 0, "切集后 position 归零");
        // 前端实时上报覆盖内部字段 → statusInfo 返回实时值
        st.update_progress(300200, 102301);
        assert_eq!(st.duration, 300200);
        assert_eq!(st.position, 102301);
        assert_eq!(st.status_info()["duration"], 300200, "statusInfo 返回实时时长");
        assert_eq!(st.status_info()["position"], 102301, "statusInfo 返回实时进度");
    }

    #[test]
    fn clear_resets_all() {
        let mut st = PlaylistState::new();
        st.apply_play(&json!({ "dramaBeans": [item_json("1", "u1")] }), false);
        st.clear();
        assert!(!st.has_items());
        assert!(st.current_episode_id.is_empty());
        assert_eq!(st.status, "STOPPED");
    }

    #[test]
    fn status_info_position_advances_with_time() {
        // 防回归（2026-08-25 实测 position 卡死 559）：PLAYING 状态下 status_info()
        // 的 position 必须按流逝时间推进（1x、封顶 duration）——抖音端轮询才能
        // 看到进度在走；前端上报/切集重置基准。
        let mut st = PlaylistState::new();
        st.apply_play(&json!({ "dramaBeans": [item_json("1", "u1")] }), false);
        st.update_progress(300_000, 10_000);
        let p0 = st.status_info()["position"].as_u64().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(60));
        let p1 = st.status_info()["position"].as_u64().unwrap();
        assert!(p1 > p0, "position 随时间推进（{p0} → {p1}）");
        // 封顶 duration：把基准时间拉长后推进不超过 duration
        let cap = st.status_info()["position"].as_u64().unwrap();
        assert!(cap <= 300_000, "position 封顶 duration");
        // 前端新上报重置基准 → 从新值重新推进
        st.update_progress(300_000, 20_000);
        let q0 = st.status_info()["position"].as_u64().unwrap();
        assert!(q0 >= 20_000 && q0 < 20_500, "上报后基准重置（{q0}）");
    }

    #[test]
    fn delete_by_ids() {
        let mut st = PlaylistState::new();
        st.apply_play(&json!({ "dramaBeans": [item_json("1", "u1"), item_json("2", "u2"), item_json("3", "u3")] }), false);
        st.delete(&json!({ "dramaIds": ["2"] }));
        assert_eq!(st.len(), 2);
        assert!(st.items.get("2").is_none());
    }

    #[test]
    fn frame_roundtrip() {
        let v = json!({ "cmd": "GetDeviceInfo", "messageId": "m1" });
        let frame = build_frame(&v);
        assert_eq!(
            u32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize,
            frame.len() - 4
        );
        let parsed: Value = serde_json::from_slice(&frame[4..]).unwrap();
        assert_eq!(parsed["cmd"], "GetDeviceInfo");
    }

    #[test]
    fn deep_find_nested_key() {
        let v = json!({ "a": { "b": { "c": "found" } }, "arr": [ { "x": 1 } ] });
        assert_eq!(deep_find(&v, "c").and_then(|x| x.as_str()), Some("found"));
        assert_eq!(deep_find(&v, "x").and_then(|x| x.as_i64()), Some(1));
        assert_eq!(deep_find(&v, "missing"), None);
    }

    #[test]
    fn crypto_encrypt_decrypt_roundtrip() {
        let a = SessionCrypto::new();
        let b = SessionCrypto::new();
        // A 的私钥 + B 的公钥 = B 的私钥 + A 的公钥（共享密钥一致）
        let key_a = a.derive_aes_key(&b.public_b64).unwrap();
        let key_b = b.derive_aes_key(&a.public_b64).unwrap();
        assert_eq!(key_a, key_b);
        let enc = a.encrypt(&key_a, r#"{"cmd":"Play","dramaId":"1"}"#).unwrap();
        let dec = b.decrypt(&key_b, &enc).unwrap();
        assert_eq!(String::from_utf8(dec).unwrap(), r#"{"cmd":"Play","dramaId":"1"}"#);
    }

    // -----------------------------------------------------------------------
    // 端到端：真实 TCP 通道（模拟手机客户端）
    // -----------------------------------------------------------------------
    fn test_device() -> PlaylistDeviceIdentity {
        PlaylistDeviceIdentity {
            ip: "127.0.0.1".into(),
            name: "测试设备".into(),
            package_name: "com.test.tv".into(),
            device_id: "123456789".into(),
            os_version: "test".into(),
            device_model: "test".into(),
            device_brand: "test".into(),
        }
    }

    async fn send_frame(stream: &mut TcpStream, v: &Value) {
        let frame = build_frame(v);
        stream.write_all(&frame).await.unwrap();
        stream.flush().await.unwrap();
    }

    async fn recv_frame(stream: &mut TcpStream) -> Value {
        let mut header = [0u8; 4];
        stream.read_exact(&mut header).await.unwrap();
        let length = u32::from_le_bytes(header) as usize;
        let mut buf = vec![0u8; length];
        stream.read_exact(&mut buf).await.unwrap();
        serde_json::from_slice(&buf).unwrap()
    }

    #[tokio::test]
    async fn channel_end_to_end_handshake_and_playlist() {
        let (tx, rx) = broadcast::channel::<()>(1);
        let played: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let played_cb = played.clone();
        let ch = PlaylistChannel::start(
            test_device(),
            rx,
            Box::new(move |item| {
                played_cb.lock().unwrap().push(item.url.clone());
            }),
        )
        .await
        .unwrap();
        let port = ch.control_port();
        assert!(port > 0, "control port should be bound");
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();

        // 1. GetDeviceInfo 握手
        send_frame(
            &mut stream,
            &json!({ "encrypt": 0, "content": { "version": 1, "messageId": "m1", "sourceInfo": { "name": "抖音", "platform": "ios", "packageName": "com.ss.iphone" }, "body": { "cmd": "GetDeviceInfo" } } }),
        )
        .await;
        let resp = recv_frame(&mut stream).await;
        assert_eq!(resp["content"]["code"], 0);
        let device = &resp["content"]["body"]["deviceInfo"];
        assert!(device["preSharedKey"].as_str().unwrap().len() >= 32, "preSharedKey = X25519 公钥");
        assert_eq!(device["bitmap"], 2048);
        assert_eq!(device["deviceId"], "123456789");
        assert_eq!(device["version"], "39512");
        assert!(device["supportedCodecs"].is_array());

        // 2. Play 带剧集列表（startDramaId 指向第 1 集）
        send_frame(
            &mut stream,
            &json!({ "encrypt": 0, "content": { "version": 1, "messageId": "m2", "body": { "cmd": "Play", "startDramaId": "1", "dramaBeans": [ item_json("1", "http://x/1.mp4"), item_json("2", "http://x/2.mp4"), item_json("3", "http://x/3.mp4") ] } } }),
        )
        .await;
        // 期望：先 ACK（code=0），后 PushMediaInfo
        let ack = recv_frame(&mut stream).await;
        assert_eq!(ack["content"]["code"], 0);
        assert_eq!(ack["content"]["messageId"], "m2");
        let push = recv_frame(&mut stream).await;
        assert_eq!(push["content"]["body"]["cmd"], "PushMediaInfo");
        let media = &push["content"]["body"]["mediaInfo"];
        assert_eq!(media["dramaId"], "1");
        assert_eq!(media["dramaBeans"].as_array().unwrap().len(), 3);
        // on_play_request 回调已触发第一集
        assert_eq!(played.lock().unwrap()[0], "http://x/1.mp4");

        // 3. AddDramaList 追加（顶层无 dramaId → 当前项不变）
        send_frame(
            &mut stream,
            &json!({ "encrypt": 0, "content": { "version": 1, "messageId": "m3", "body": { "cmd": "AddDramaList", "dramaBeans": [ item_json("4", "http://x/4.mp4") ] } } }),
        )
        .await;
        let _ack = recv_frame(&mut stream).await;
        let push2 = recv_frame(&mut stream).await;
        assert_eq!(push2["content"]["body"]["mediaInfo"]["dramaId"], "1", "AddDramaList 不清当前集");
        assert_eq!(push2["content"]["body"]["mediaInfo"]["dramaBeans"].as_array().unwrap().len(), 4);

        // 4. PlayNextDrama → 当前集切到 2，回调第二集
        send_frame(
            &mut stream,
            &json!({ "encrypt": 0, "content": { "version": 1, "messageId": "m4", "body": { "cmd": "PlayNextDrama" } } }),
        )
        .await;
        let _ack = recv_frame(&mut stream).await;
        let push3 = recv_frame(&mut stream).await;
        assert_eq!(push3["content"]["body"]["mediaInfo"]["dramaId"], "2");
        assert_eq!(played.lock().unwrap()[1], "http://x/2.mp4");

        // 5. 播完自动切集（宿主调用 next_and_get）→ 切到 3
        let next = ch.next_and_get(true, false);
        assert!(next.is_some());
        assert_eq!(next.unwrap().url, "http://x/3.mp4");
        assert_eq!(played.lock().unwrap()[2], "http://x/3.mp4");
        // 列表末尾无下一项
        assert!(ch.next_and_get(true, false).is_none());

        // 6. ClearDramaList → 列表清空
        send_frame(
            &mut stream,
            &json!({ "encrypt": 0, "content": { "version": 1, "messageId": "m5", "body": { "cmd": "ClearDramaList" } } }),
        )
        .await;
        let _ack = recv_frame(&mut stream).await;
        assert!(!ch.has_playlist());

        tx.send(()).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}
