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
    position: Mutex<u64>,
    duration: Mutex<u64>,
}

#[allow(dead_code)]
impl AvTransport {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(TransportState::NoMedia),
            track_uri: Mutex::new(String::new()),
            position: Mutex::new(0),
            duration: Mutex::new(0),
        }
    }

    pub fn state(&self) -> TransportState {
        *self.state.lock().unwrap()
    }

    pub fn track_uri(&self) -> String {
        self.track_uri.lock().unwrap().clone()
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

    pub fn set_uri(&self, uri: &str) {
        *self.track_uri.lock().unwrap() = uri.to_string();
        *self.position.lock().unwrap() = 0;
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

    pub fn seek(&self, position_seconds: u64) -> bool {
        let s = self.state();
        if s == TransportState::Playing || s == TransportState::Paused {
            *self.position.lock().unwrap() = position_seconds;
            return true;
        }
        false
    }

    pub fn update_position(&self, seconds: u64) {
        *self.position.lock().unwrap() = seconds;
    }

    pub fn update_duration(&self, seconds: u64) {
        *self.duration.lock().unwrap() = seconds;
    }

    /// 供 GetPositionInfo / 前端进度回传读取当前播放进度（秒）。
    pub fn position(&self) -> u64 {
        *self.position.lock().unwrap()
    }

    /// 供 GetPositionInfo 读取总时长（秒）。
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
        *self.position.lock().unwrap() = 0;
        *self.duration.lock().unwrap() = 0;
    }
}

impl Default for AvTransport {
    fn default() -> Self {
        Self::new()
    }
}
