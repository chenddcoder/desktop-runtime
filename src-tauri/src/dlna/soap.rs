// SOAP 解析 / 构建 / 动作处理 —— 从 工具类/dlna-cast 的 soap-handler.ts + dmr-controller.ts 移植
// 投屏控制指令（SetAVTransportURI/Play/Pause/Stop/Seek + 查询类）在此落地。

use std::collections::HashMap;
use std::sync::OnceLock;

use regex::Regex;

use crate::dlna::av_transport::AvTransport;

/// 动作处理的结果：携带需要广播给前端的控制意图。
/// 之前只有 Play 会 emit 事件，Seek/Stop/Pause 被当成 StateChanged 静默吞掉，
/// 导致手机端进度/退出指令走到 Rust 就断、前端 <video> 永远不知道 → 投屏控制失效。
#[derive(Debug, PartialEq)]
pub enum ActionOutcome {
    Play(String),
    Seek(u64),
    Stop,
    Pause,
    StateChanged,
    None,
}

/// DLNA Seek / PositionInfo 的时间格式是 `H+:MM:SS[.F+]`（也可能 MM:SS 或裸秒），
/// 例如 `00:01:30`。之前代码直接 `t.parse::<u64>()` 解析整数，对 HH:MM:SS 必失败 → seek 永远无效。
fn parse_dlna_time(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Ok(secs) = s.parse::<u64>() {
        return Some(secs); // 裸秒
    }
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() == 2 {
        let m = parts[0].parse::<u64>().ok()?;
        let sec = parts[1].parse::<f64>().ok()?;
        Some(m * 60 + sec as u64)
    } else if parts.len() >= 3 {
        let h = parts[0].parse::<u64>().ok()?;
        let m = parts[1].parse::<u64>().ok()?;
        let sec = parts[2].parse::<f64>().ok()?;
        Some(h * 3600 + m * 60 + sec as u64)
    } else {
        None
    }
}

fn param_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // ⚠️ Rust `regex` 是 RE2 引擎，**不支持反向引用 `\1`**——原 `<([a-zA-Z_]\w*)>([^<]*)</\1>`
    // 会让 `Regex::new().unwrap()` 在运行时 panic（编译通过但启动即炸）。
    // 改为同时捕获开/闭标签名（两个独立分组），调用处校验相等，语义等价且不依赖反向引用。
    // `[^<]*` 取值（SOAP 参数无嵌套标签），标签名限制在 `[a-zA-Z_]\w*`（跳过带 `:` 的命名空间标签）。
    RE.get_or_init(|| Regex::new(r#"<([a-zA-Z_]\w*)>([^<]*)</([a-zA-Z_]\w*)>"#).unwrap())
}

/// DLNA 时间格式 H+:MM:SS（统一补零到 HH:MM:SS）。
fn secs_to_hms(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn decode_html_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

pub struct ParsedSoap {
    pub action: String,
    pub service_type: String,
    pub params: HashMap<String, String>,
}

pub fn parse_soap(body: &str) -> Option<ParsedSoap> {
    let action = {
        let marker = "<u:";
        let i = body.find(marker)?;
        let rest = &body[i + marker.len()..];
        let end = rest.find('>')?;
        let raw = &rest[..end];
        // ⚠️ 动作标签常带 `xmlns:u="..."` 属性（如 `<u:SetAVTransportURI xmlns:u="urn:...">`）。
        // 必须只取标签名（到第一个空白为止），否则 `handle_action` 的精确匹配会 miss → 投屏 Play 永不触发。
        raw.split_whitespace().next().unwrap_or(raw).to_string()
    };

    let service_type = {
        let marker = "xmlns:u=\"";
        let i = body.find(marker)?;
        let rest = &body[i + marker.len()..];
        let end = rest.find('"')?;
        rest[..end].to_string()
    };

    let mut params = HashMap::new();
    for cap in param_regex().captures_iter(body) {
        let name = &cap[1];
        let close = &cap[3];
        // 开闭标签名必须一致（等价原 `\1` 反向引用语义）
        if name != close {
            continue;
        }
        // 跳过命名空间前缀标签（u:/s:）与信封标签
        if name == "s:Envelope" || name == "s:Body" || name.starts_with("u:") {
            continue;
        }
        let val = decode_html_entities(&cap[2]);
        params.insert(name.to_string(), val);
    }

    Some(ParsedSoap {
        action,
        service_type,
        params,
    })
}

/// 处理动作业务逻辑，返回结果供调用方 emit 事件 / 构建响应。
pub fn handle_action(action: &str, params: &HashMap<String, String>, av: &AvTransport) -> ActionOutcome {
    match action {
        "SetAVTransportURI" => {
            if let Some(uri) = params.get("CurrentURI") {
                av.set_uri(&decode_html_entities(uri));
            }
            ActionOutcome::StateChanged
        }
        "Play" => match av.play() {
            Some(url) => ActionOutcome::Play(url),
            None => ActionOutcome::None,
        },
        "Pause" => {
            av.pause();
            ActionOutcome::Pause
        }
        "Stop" => {
            av.stop();
            ActionOutcome::Stop
        }
        "Seek" => {
            if let Some(t) = params.get("Target") {
                if let Some(secs) = parse_dlna_time(t) {
                    av.seek(secs);
                    return ActionOutcome::Seek(secs);
                }
            }
            // 解析失败也回 OK（DLNA 规范要求 200），但前端拿不到有效进度。
            ActionOutcome::StateChanged
        }
        _ => ActionOutcome::None,
    }
}

/// 查询类动作的固定响应参数（从 dmr-controller.ts 的 getSOAPResponseParams 移植）。
pub fn response_params(action: &str, av: &AvTransport) -> HashMap<String, String> {
    let mut m = HashMap::new();
    match action {
        "GetTransportInfo" => {
            let st = match av.state() {
                crate::dlna::av_transport::TransportState::Playing => "PLAYING",
                crate::dlna::av_transport::TransportState::Paused => "PAUSED_PLAYBACK",
                crate::dlna::av_transport::TransportState::Stopped => "STOPPED",
                crate::dlna::av_transport::TransportState::Transitioning => "TRANSITIONING",
                crate::dlna::av_transport::TransportState::NoMedia => "NO_MEDIA_PRESENT",
            };
            m.insert("CurrentTransportState".into(), st.into());
            m.insert("CurrentTransportStatus".into(), "OK".into());
            m.insert("CurrentSpeed".into(), "1".into());
        }
        "GetPositionInfo" => {
            // 关键修复：之前这里硬编码 0:00:00，客户端进度条永远停在 0、拖动后读回仍是 0。
            // 现在用前端 <video> 上报进 AvTransport 的真实进度。
            let pos = av.position();
            let dur = av.duration();
            m.insert("Track".into(), "1".into());
            m.insert("TrackDuration".into(), secs_to_hms(dur));
            m.insert("TrackMetaData".into(), String::new());
            m.insert("TrackURI".into(), av.track_uri());
            m.insert("RelTime".into(), secs_to_hms(pos));
            m.insert("AbsTime".into(), secs_to_hms(pos));
            m.insert("RelCount".into(), "0".into());
            m.insert("AbsCount".into(), "0".into());
        }
        "GetVolume" => {
            m.insert("CurrentVolume".into(), "50".into());
        }
        "GetMute" => {
            m.insert("CurrentMute".into(), "0".into());
        }
        "GetDeviceCapabilities" => {
            m.insert("PlayMedia".into(), "VIDEO,AUDIO".into());
            m.insert("RecMedia".into(), String::new());
            m.insert("RecQualityModes".into(), String::new());
        }
        "GetProtocolInfo" => {
            m.insert("Source".into(), String::new());
            m.insert(
                "Sink".into(),
                "http-get:*:video/*:*,http-get:*:audio/*:*,http-get:*:image/*:*".into(),
            );
        }
        "GetCurrentConnectionInfo" => {
            m.insert("RcsID".into(), "0".into());
            m.insert("AVTransportID".into(), "0".into());
            m.insert("ProtocolInfo".into(), String::new());
            m.insert("PeerConnectionManager".into(), String::new());
            m.insert("PeerConnectionID".into(), "-1".into());
            m.insert("Direction".into(), "Input".into());
            m.insert("Status".into(), "OK".into());
        }
        _ => {}
    }
    m
}

pub fn build_soap_response(action: &str, service_type: &str, params: &HashMap<String, String>) -> String {
    let inner: String = params
        .iter()
        .map(|(k, v)| format!("<{k}>{}</{k}>", escape_xml(v)))
        .collect();
    format!(
        r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
  <s:Body>
    <u:{action}Response xmlns:u="{service_type}">
      {inner}
    </u:{action}Response>
  </s:Body>
</s:Envelope>"#
    )
}

pub fn build_soap_error(error_code: u16, error_desc: &str) -> String {
    format!(
        r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
  <s:Body>
    <s:Fault>
      <faultcode>s:Client</faultcode>
      <faultstring>UPnPError</faultstring>
      <detail>
        <UPnPError xmlns="urn:schemas-upnp-org:control-1-0">
          <errorCode>{error_code}</errorCode>
          <errorDescription>{desc}</errorDescription>
        </UPnPError>
      </detail>
    </s:Fault>
  </s:Body>
</s:Envelope>"#,
        desc = escape_xml(error_desc)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // 真实 DLNA 投屏控制请求体（SetAVTransportURI），验证：
    // ① 不 panic（之前 `\1` 反向引用会让 Regex::new().unwrap() 启动即炸）
    // ② 正确解析出 CurrentURI / InstanceID 参数
    // ③ 跳过信封标签（s:Envelope / s:Body）与动作包装标签（u:SetAVTransportURI）
    #[test]
    fn parse_soap_set_av_transport_uri() {
        let body = r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
  <s:Body>
    <u:SetAVTransportURI xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">
      <InstanceID>0</InstanceID>
      <CurrentURI>http://192.168.1.10:8080/video.mp4</CurrentURI>
      <CurrentURIMetaData></CurrentURIMetaData>
    </u:SetAVTransportURI>
  </s:Body>
</s:Envelope>"#;

        let parsed = parse_soap(body).expect("parse_soap should succeed");
        assert_eq!(parsed.action, "SetAVTransportURI");
        assert_eq!(parsed.service_type, "urn:schemas-upnp-org:service:AVTransport:1");
        assert_eq!(parsed.params.get("InstanceID").map(|s| s.as_str()), Some("0"));
        assert_eq!(
            parsed.params.get("CurrentURI").map(|s| s.as_str()),
            Some("http://192.168.1.10:8080/video.mp4")
        );
        // 信封 / 动作包装标签不应进入 params
        assert!(!parsed.params.contains_key("s:Envelope"));
        assert!(!parsed.params.contains_key("s:Body"));
        assert!(!parsed.params.contains_key("u:SetAVTransportURI"));
    }

    #[test]
    fn parse_soap_html_entities_decoded() {
        // CurrentURI 里若带 &amp; 等实体应被解码
        let body = r#"<u:SetAVTransportURI xmlns:u="x"><InstanceID>0</InstanceID><CurrentURI>a&amp;b</CurrentURI></u:SetAVTransportURI>"#;
        let parsed = parse_soap(body).unwrap();
        assert_eq!(parsed.params.get("CurrentURI").map(|s| s.as_str()), Some("a&b"));
    }

    #[test]
    fn parse_soap_mismatched_tags_skipped() {
        // 开闭标签名不一致的不应入库（正向原 `\1` 反向引用语义）
        let body = r#"<u:X xmlns:u="x"><A>1</B><C>2</C></u:X>"#;
        let parsed = parse_soap(body).unwrap();
        assert!(!parsed.params.contains_key("A"));
        assert_eq!(parsed.params.get("C").map(|s| s.as_str()), Some("2"));
    }

    // DLNA Seek Target 用 HH:MM:SS 格式，之前裸 parse::<u64>() 解析失败 → seek 永远无效。
    #[test]
    fn seek_parses_hms_format() {
        // 真实控制器发的 Seek Target 形如 "00:01:30"
        let body = r#"<u:Seek xmlns:u="urn:schemas-upnp-org:service:AVTransport:1"><InstanceID>0</InstanceID><Unit>ABS_TIME</Unit><Target>00:01:30</Target></u:Seek>"#;
        let parsed = parse_soap(body).unwrap();
        assert_eq!(parsed.action, "Seek");
        // 通过 handle_action 验证能正确转成秒并发出 Seek(90)
        let av = crate::dlna::av_transport::AvTransport::new();
        av.set_uri("http://x/v.mp4");
        av.play();
        let outcome = handle_action("Seek", &parsed.params, &av);
        match outcome {
            ActionOutcome::Seek(secs) => assert_eq!(secs, 90),
            other => panic!("expected Seek(90), got {:?}", other),
        }
    }

    #[test]
    fn seek_parses_bare_seconds_and_mmss() {
        let av = crate::dlna::av_transport::AvTransport::new();
        av.set_uri("http://x/v.mp4");
        av.play();

        let mut p1 = std::collections::HashMap::new();
        p1.insert("Target".to_string(), "42".to_string());
        assert_eq!(handle_action("Seek", &p1, &av), ActionOutcome::Seek(42));

        let mut p2 = std::collections::HashMap::new();
        p2.insert("Target".to_string(), "01:30".to_string());
        assert_eq!(handle_action("Seek", &p2, &av), ActionOutcome::Seek(90));

        // Stop / Pause 应分别映射到对应控制意图
        assert_eq!(handle_action("Stop", &p1, &av), ActionOutcome::Stop);
        assert_eq!(handle_action("Pause", &p1, &av), ActionOutcome::Pause);
    }

    // 核心修复验证：之前 GetPositionInfo 永远返回 0:00:00 —— 客户端进度条不动、拖动后读回仍是 0。
    // 这里模拟前端 report_position 写入真实进度后，response_params 必须返回真实 RelTime / TrackDuration。
    #[test]
    fn get_position_info_returns_real_progress() {
        let av = crate::dlna::av_transport::AvTransport::new();
        av.set_uri("http://x/v.mp4");
        av.update_position(90); // 1:30
        av.update_duration(600); // 10:00
        let m = response_params("GetPositionInfo", &av);
        assert_eq!(m.get("RelTime").map(|s| s.as_str()), Some("00:01:30"));
        assert_eq!(m.get("AbsTime").map(|s| s.as_str()), Some("00:01:30"));
        assert_eq!(m.get("TrackDuration").map(|s| s.as_str()), Some("00:10:00"));
        assert_eq!(m.get("TrackURI").map(|s| s.as_str()), Some("http://x/v.mp4"));
        assert_eq!(m.get("Track").map(|s| s.as_str()), Some("1"));
    }
}
