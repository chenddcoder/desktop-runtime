// AVTransport 状态机 —— 从 工具类/dlna-cast 的 av-transport.ts 移植
// 纯状态机，不依赖网络/AppHandle。收到 Play 时返回被投 URL 交由调用方去 emit 事件。

use std::sync::Mutex;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[allow(dead_code)]
pub enum TransportState {
    NoMedia,
    Stopped,
    Playing,
    Paused,
    Transitioning,
}

pub struct AvTransport {
    state: Mutex<TransportState>,
    track_uri: Mutex<String>,
    /// SetAVTransportURI 携带的 DIDL-Lite 元数据（抖音等客户端会校验
    /// GetPositionInfo 返回的 TrackMetaData 与投屏时传入的一致，空则丢弃响应）。
    track_meta_data: Mutex<String>,
    /// 播放进度（毫秒）。前端上报即毫秒；GetPositionInfo 输出 RelTime 时
    /// 转 H+:MM:SS[.F+]（毫秒小数），避免 <1s 的进度被截断成 0 导致
    /// 客户端（Android 抖音等）误判"设备未播放"、进度条不更新。
    position: Mutex<u64>,
    /// 总时长（毫秒）。
    duration: Mutex<u64>,
    /// 强制完成信号（TV 下键/手动切集）：客户端（抖音）有"先确认在播
    /// （进度>1s）再接受播完信号"的判断逻辑，直接返回总时长在进度<1s 时
    /// 会被忽略。GetPositionInfo 响应时做渐进处理：上次返回 <1s → 先给
    /// 一个 >1s 过渡值确认在播，下次轮询再返回总时长；上次 ≥1s → 直接返回总时长。
    force_complete: Mutex<bool>,
    /// 上次 GetPositionInfo 返回给客户端的 RelTime（毫秒），用于判断客户端
    /// 是否已确认在播。换源（SetAVTransportURI）时清零。
    last_reported: Mutex<u64>,
}

#[allow(dead_code)]
impl AvTransport {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(TransportState::NoMedia),
            track_uri: Mutex::new(String::new()),
            track_meta_data: Mutex::new(String::new()),
            position: Mutex::new(0),
            duration: Mutex::new(0),
            force_complete: Mutex::new(false),
            last_reported: Mutex::new(0),
        }
    }

    pub fn state(&self) -> TransportState {
        *self.state.lock().unwrap()
    }

    pub fn track_uri(&self) -> String {
        self.track_uri.lock().unwrap().clone()
    }

    pub fn track_meta_data(&self) -> String {
        self.track_meta_data.lock().unwrap().clone()
    }

    fn allowed_actions(&self) -> String {
        match self.state() {
            TransportState::NoMedia => "SetAVTransportURI".into(),
            TransportState::Stopped => "SetAVTransportURI,Play".into(),
            TransportState::Playing => "Pause,Stop,Seek,Next,Previous".into(),
            TransportState::Paused => "Play,Stop,Seek,Next,Previous".into(),
            TransportState::Transitioning => String::new(),
        }
    }

    pub fn info(&self) -> serde_json::Value {
        serde_json::json!({
            "state": format!("{:?}", self.state()),
            "currentTransportActions": self.allowed_actions(),
            "trackURI": self.track_uri(),
            "trackDuration": *self.duration.lock().unwrap(),
            "relativeTimePosition": *self.position.lock().unwrap(),
        })
    }

    pub fn set_uri(&self, uri: &str, meta_data: &str) {
        *self.track_uri.lock().unwrap() = uri.to_string();
        *self.track_meta_data.lock().unwrap() = meta_data.to_string();
        *self.position.lock().unwrap() = 0;
        // 关键：换源时一并清空 duration，避免新集 TrackDuration 短暂残留
        // 上一集时长，与 TrackMetaData 不一致被客户端（抖音）校验丢弃。
        *self.duration.lock().unwrap() = 0;
        // 新集开始：清强制完成信号与上次回报进度（客户端在新集重新确认在播）
        *self.force_complete.lock().unwrap() = false;
        *self.last_reported.lock().unwrap() = 0;
        *self.state.lock().unwrap() = TransportState::Stopped;
    }

    /// 播放成功返回被投 URL，否则返回 None。
    pub fn play(&self) -> Option<String> {
        let s = self.state();
        if s == TransportState::Stopped || s == TransportState::Paused {
            *self.state.lock().unwrap() = TransportState::Playing;
            return Some(self.track_uri());
        }
        None
    }

    pub fn pause(&self) -> bool {
        if self.state() == TransportState::Playing {
            *self.state.lock().unwrap() = TransportState::Paused;
            return true;
        }
        false
    }

    pub fn stop(&self) -> bool {
        let s = self.state();
        if s == TransportState::Playing || s == TransportState::Paused {
            *self.state.lock().unwrap() = TransportState::Stopped;
            return true;
        }
        false
    }

    /// SOAP Seek 传入的是秒（客户端拖动进度），内部按毫秒存储。
    pub fn seek(&self, position_seconds: u64) -> bool {
        let s = self.state();
        if s == TransportState::Playing || s == TransportState::Paused {
            *self.position.lock().unwrap() = position_seconds.saturating_mul(1000);
            return true;
        }
        false
    }

    /// 更新播放进度（毫秒）。
    pub fn update_position(&self, position_ms: u64) {
        *self.position.lock().unwrap() = position_ms;
    }

    /// 更新总时长（毫秒）。
    pub fn update_duration(&self, duration_ms: u64) {
        *self.duration.lock().unwrap() = duration_ms;
    }

    /// 设置强制完成信号（TV 下键/手动切集）：GetPositionInfo 将渐进把进度导向总时长。
    pub fn set_force_complete(&self) {
        *self.force_complete.lock().unwrap() = true;
    }

    /// 强制完成信号已发出（GetPositionInfo 返回总时长后清除）。
    pub fn clear_force_complete(&self) {
        *self.force_complete.lock().unwrap() = false;
    }

    pub fn force_complete(&self) -> bool {
        *self.force_complete.lock().unwrap()
    }

    /// 记录本次 GetPositionInfo 返回给客户端的 RelTime（毫秒）。
    pub fn set_last_reported(&self, ms: u64) {
        *self.last_reported.lock().unwrap() = ms;
    }

    /// 上次 GetPositionInfo 返回给客户端的 RelTime（毫秒）。
    pub fn last_reported(&self) -> u64 {
        *self.last_reported.lock().unwrap()
    }

    /// 供 GetPositionInfo / 前端进度回传读取当前播放进度（毫秒）。
    pub fn position(&self) -> u64 {
        *self.position.lock().unwrap()
    }

    /// 供 GetPositionInfo 读取总时长（毫秒）。
    pub fn duration(&self) -> u64 {
        *self.duration.lock().unwrap()
    }

    /// 由前端 <video> 真实状态反向同步：playing=正在播, paused=暂停, 二者皆否=停止/未投屏。
    pub fn update_playback(&self, playing: bool, paused: bool) {
        let mut s = self.state.lock().unwrap();
        *s = if paused {
            TransportState::Paused
        } else if playing {
            TransportState::Playing
        } else {
            TransportState::Stopped
        };
    }

    pub fn reset(&self) {
        *self.state.lock().unwrap() = TransportState::NoMedia;
        *self.track_uri.lock().unwrap() = String::new();
        *self.track_meta_data.lock().unwrap() = String::new();
        *self.position.lock().unwrap() = 0;
        *self.duration.lock().unwrap() = 0;
        *self.force_complete.lock().unwrap() = false;
        *self.last_reported.lock().unwrap() = 0;
    }
}

impl Default for AvTransport {
    fn default() -> Self {
        Self::new()
    }
}
