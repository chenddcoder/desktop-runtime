// SOAP 解析 / 构建 / 动作处理 —— 从 工具类/dlna-cast 的 soap-handler.ts + dmr-controller.ts 移植
// 投屏控制指令（SetAVTransportURI/Play/Pause/Stop/Seek + 查询类）在此落地。

use std::collections::HashMap;
use std::sync::OnceLock;

use regex::Regex;

use crate::dlna::av_transport::AvTransport;

/// 强制完成（切集）时 RelTime **超出** TrackDuration 的余量（毫秒）。
/// 抖音只对"进度超出总时长"（RelTime > TrackDuration）判定播完并切集，
/// 停在 ==dur 会被视为未超出、不切集（历史实证：上报 position=dur+1000 必切集）。
const FORCE_COMPLETE_OVERSHOOT_MS: u64 = 0;

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

/// DLNA 时间格式 H+:MM:SS[.F+]（毫秒精度，AvTransport 内部按毫秒存储）。
/// 毫秒为 0 输出整秒（00:00:05），毫秒>0 带小数（00:00:00.500）。
/// 关键：之前整秒截断会让 <1s 的进度显示 00:00:00，客户端（Android 抖音）
/// 持续看到 0 会误判"设备未播放"、进度条不更新——带毫秒小数后播放
/// 0.5s 即可让 RelTime 非零，客户端立即确认设备在播。
fn ms_to_hms(ms: u64) -> String {
    let total_secs = ms / 1000;
    let millis = ms % 1000;
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    if millis > 0 {
        format!("{h:02}:{m:02}:{s:02}.{millis:03}")
    } else {
        format!("{h:02}:{m:02}:{s:02}")
    }
}

/// DLNA 时间格式 H+:MM:SS（整秒，无毫秒小数）。
/// iOS 抖音兼容（2026-08-24 实测）：带小数的 RelTime（HH:MM:SS.mmm）会被
/// iOS UPnP 解析失败 → 判定进度无效 → 永不触发"播完→切集"。证据：
///   伪造 dur+1000=154067 → 00:02:34.067（带小数）iPhone 不切集；
///   自然播完 428000 → 00:07:08（整秒）iPhone 切集成功。
/// 安卓抖音可正常解析带小数（所以安卓不受影响），但整秒输出对安卓
/// 判定无副作用（RelTime>TrackDuration 依然成立，见调用处注释）。
fn ms_to_hms_whole(ms: u64) -> String {
    let total_secs = ms / 1000;
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
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
                // params 值在 parse_soap 时已 decode_html_entities，此处直接保存解码后的
                // DIDL-Lite XML；GetPositionInfo 需原样回传 TrackMetaData（客户端会校验一致性）。
                let meta = params.get("CurrentURIMetaData").cloned().unwrap_or_default();
                eprintln!(
                    "[dlna_soap_req] SetAVTransportURI metaLen={} metaHead={}",
                    meta.len(),
                    meta.chars().take(120).collect::<String>()
                );
                av.set_uri(&decode_html_entities(uri), &meta);
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
            eprintln!(
                "[dlna_soap_resp] GetTransportInfo -> CurrentTransportState={} pos={}ms dur={}ms",
                st,
                av.position(),
                av.duration()
            );
            m.insert("CurrentTransportState".into(), st.into());
            m.insert("CurrentTransportStatus".into(), "OK".into());
            m.insert("CurrentSpeed".into(), "1".into());
        }
        "GetPositionInfo" => {
            // 关键修复：之前这里硬编码 0:00:00，客户端进度条永远停在 0、拖动后读回仍是 0。
            // 现在用前端 <video> 上报进 AvTransport 的真实进度（毫秒）。
            let mut pos = av.position();
            let dur = av.duration();
            let uri = av.track_uri();
            // 短视频伪装（**仅抖音投屏场景**）：抖音安卓客户端对"总时长 5 秒内"的视频
            // 不自动切换（播完不进入下一集）。其他客户端（手机自带 DLNA/优酷等）无此
            // bug，看到伪装的 6s 时长反而显示异常 → 必须限定抖音场景。
            // 判定：TrackURI host 命中抖音/字节系 CDN 后缀（对齐 xiaoyoucast
            // tools/douyin-cast.ts 的 DOUYIN_CDN_HOST_SUFFIXES）或带 ott_cast 参数
            //（抖音 TV 投屏特有）。
            // ⚠️ 抖音的判定是**整数秒截断**——实测 5163ms（5.163s）也会被当作 5s
            // 而卡住不切集，因此阈值不能用字面 <=5000ms，而应覆盖所有"截断成秒后
            // <=5s"的视频，即真实时长 **<6000ms 一律伪装**。把这类视频"虚拟拉长"
            // 到 6s——假装视频大于 5 秒（截断后 6s>5s），让客户端按正常视频建立
            // "播完→切集"机制：
            //   TrackDuration 一律上报 6000ms；RelTime 在 播放中 报真实进度
            //   （<6s，客户端确认在播、进度条正常走），在 播完 时上报
            //   **7000ms（虚拟时长 6s + 1s 超出）**——
            //   ⚠️ 实测 RelTime=6000 == TrackDuration=6000 抖音视为"未超出"不切集，
            //      必须超出（与长视频 force_complete 的 dur+1000 模式一致）。
            //   - 按下切集（force_complete）：进度返回 7s
            //   - 没有按下（自然播完）：真实进度到达真实末尾（前端已上报
            //     pos>=真实时长）→ 进度同样返回 7s
            let is_douyin_source = [
                "douyinvod.com",
                "douyincdn.com",
                "iesdouyin.com",
                "bytecdn.cn",
                "pstatp.com",
                "byteimg.com",
                "toutiaoimg.com",
                "toutiaovod.com",
                "ixigua.com",
            ]
            .iter()
            .any(|suffix| uri.contains(suffix))
                || uri.contains("ott_cast");
            let fake_short = is_douyin_source && dur > 0 && dur < 6000;
            let reported_dur = if fake_short { 6000 } else { dur };
            if fake_short {
                // 超时兜底：客户端一直不切集时清除 force_complete，防止锁死
                if av.force_complete() && av.force_complete_expired(std::time::Duration::from_secs(20)) {
                    av.clear_force_complete();
                }
                if av.force_complete() || pos >= dur {
                    // 按下切集 / 自然播完 → 返回超出虚拟时长的进度（6s+1s=7s）
                    pos = reported_dur + FORCE_COMPLETE_OVERSHOOT_MS;
                }
                // 其余情况（播放中）：保留真实进度（<6s）
            } else if av.force_complete() && dur > 0 {
                // 强制完成逻辑（TV 下键/手动切集，前端发 force_complete 信号）：
                // **与安卓端完全对齐——瞬间跳变到超出总时长**（RelTime = dur + OVERSHOOT）。
                // 安卓抖音实测：position=dur+1000 必切集（历史实证）。iOS 抖音按下后
                // 同样能触发切集（客户端切集流程 Stop→SetAVTransportURI→Play 已实测走通，
                // 2026-08-25；换集后 tvcast 跟随播放见 casting 页 onDlnaStop 延迟退出修复）。
                // 上一版"平滑递增模拟自然播完曲线"方案已按用户要求废弃（2026-08-25）——
                // 两端统一走"切下进度到头，客户端自动切下一个"。
                // 抖音客户端有"先确认在播（上次回报进度>1s）再接受播完信号"的判断：
                //   - 上次回报 <1s（客户端未确认在播）→ 先给 >1s 过渡值（2s）确认在播
                //   - 上次回报 ≥1s → **直接返回超出总时长的进度**（RelTime = dur+1000
                //     > TrackDuration，整秒化后两者恒差 1s，判定不受影响）
                // ⚠️ 必须持续返回（保持标志）：只返回一次就回退会被客户端判定"进度倒退"
                //   而不切集；持续返回直到客户端 SetAVTransportURI 换集（set_uri 清标志）。
                // 超时兜底：20s 后自动清除（防止客户端一直不切集，进度永久锁死在末尾）。
                if av.force_complete_expired(std::time::Duration::from_secs(20)) {
                    av.clear_force_complete();
                } else {
                    let last = av.last_reported();
                    if last >= 1000 {
                        pos = dur + FORCE_COMPLETE_OVERSHOOT_MS;
                    } else {
                        let transition = if dur >= 2000 { 2000 } else { dur };
                        pos = pos.max(transition);
                    }
                }
            }
            av.set_last_reported(pos);
            let meta = av.track_meta_data();
            // iOS 抖音兼容（2026-08-24 实测）：RelTime/TrackDuration 必须输出
            // 整秒（HH:MM:SS），不能带毫秒小数——iOS UPnP 对 HH:MM:SS.mmm 解析
            // 失败 → 进度判定无效 → 永不触发切集。播放中的 RelTime 保持带小数
            // （安卓抖音需要毫秒精度避免 <1s 进度截断成 0 误判未播放，且 iOS
            // 对播放中进度条的解析失败不影响播完判定——播完时刻的值是整秒）。
            // 播完/超出值（pos>dur）强制整秒：毫秒余数向下舍去后仍满足
            // RelTime > TrackDuration（dur+1000 整秒化后两者恒差 1000ms），
            // 安卓抖音判定"RelTime>TrackDuration=播完"不受影响。
            let rel_time = if pos > dur { ms_to_hms_whole(pos) } else { ms_to_hms(pos) };
            // ⚠️ eprintln 必须打印与 XML 一致的值（rel_time/整秒 TrackDuration）：
            // 之前用 ms_to_hms 打印带小数，日志看着"没生效"但 XML 已是整秒——
            // 判断是否生效以本日志为准。
            eprintln!(
                "[dlna_soap_resp] GetPositionInfo -> RelTime={rel_time} TrackDuration={} (pos={pos}ms realDur={dur}ms fake_short={fake_short} is_douyin={is_douyin_source} force_complete={}) TrackURI={uri:?} TrackMetaData.len={}",
                ms_to_hms_whole(reported_dur),
                av.force_complete(),
                meta.len()
            );
            m.insert("Track".into(), "1".into());
            m.insert("TrackDuration".into(), ms_to_hms_whole(reported_dur));
            // 回传投屏时携带的 DIDL-Lite 元数据（规范要求与 SetAVTransportURI 一致；
            // 之前恒为空，抖音等客户端校验失败会丢弃整个响应 → 进度条/下一集判断失效）。
            m.insert("TrackMetaData".into(), meta);
            m.insert("TrackURI".into(), uri);
            m.insert("RelTime".into(), rel_time.clone());
            m.insert("AbsTime".into(), rel_time);
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
        av.set_uri("http://x/v.mp4", "");
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
        av.set_uri("http://x/v.mp4", "");
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
        av.set_uri("http://x/v.mp4", "");
        av.update_position(90_000); // 1:30（毫秒）
        av.update_duration(600_000); // 10:00（毫秒）
        let m = response_params("GetPositionInfo", &av);
        assert_eq!(m.get("RelTime").map(|s| s.as_str()), Some("00:01:30"));
        assert_eq!(m.get("AbsTime").map(|s| s.as_str()), Some("00:01:30"));
        assert_eq!(m.get("TrackDuration").map(|s| s.as_str()), Some("00:10:00"));
        assert_eq!(m.get("TrackURI").map(|s| s.as_str()), Some("http://x/v.mp4"));
        assert_eq!(m.get("Track").map(|s| s.as_str()), Some("1"));
    }

    /// 毫秒精度验证：<1s 的进度必须输出非零 RelTime（客户端判定"在播"的关键）。
    #[test]
    fn position_below_one_second_is_not_truncated_to_zero() {
        let av = crate::dlna::av_transport::AvTransport::new();
        av.set_uri("http://x/v.mp4", "");
        av.update_position(500); // 0.5s
        av.update_duration(16_000); // 16s
        let m = response_params("GetPositionInfo", &av);
        assert_eq!(m.get("RelTime").map(|s| s.as_str()), Some("00:00:00.500"));
        assert_eq!(m.get("TrackDuration").map(|s| s.as_str()), Some("00:00:16"));
        // 0 值仍是纯整秒 00:00:00
        av.update_position(0);
        let m = response_params("GetPositionInfo", &av);
        assert_eq!(m.get("RelTime").map(|s| s.as_str()), Some("00:00:00"));
    }

    /// 换源（切集）后 duration 必须清零，避免残留上一集时长被客户端校验丢弃。
    #[test]
    fn set_uri_resets_duration() {
        let av = crate::dlna::av_transport::AvTransport::new();
        av.update_duration(29_000);
        av.set_uri("http://x/v2.mp4", "");
        assert_eq!(av.duration(), 0);
        assert_eq!(av.position(), 0);
    }

    /// 强制完成渐进逻辑：客户端上次回报 <1s（未确认在播）→ 先返回 2s 过渡值
    /// 确认在播，保持标志；之后**瞬间跳变到超出总时长的进度**（与安卓端对齐——
    /// RelTime=TrackDuration+1000 > TrackDuration，抖音对"进度超出总时长"判定
    /// 播完切集，停在 ==dur 会被视为未超出而不切集；只返回一次就回退则会被判定
    /// 进度倒退而不切集。iOS/安卓统一走"切下进度到头"策略，2026-08-25）。
    #[test]
    fn force_complete_progressive_when_client_below_one_second() {
        let av = crate::dlna::av_transport::AvTransport::new();
        av.set_uri("http://x/v.mp4", "");
        av.update_duration(29_000);
        av.set_force_complete();
        // 第一次轮询：last_reported=0（<1s）→ 返回 2s 过渡值，标志保持
        let m = response_params("GetPositionInfo", &av);
        assert_eq!(m.get("RelTime").map(|s| s.as_str()), Some("00:00:02"));
        assert!(av.force_complete(), "过渡后标志应保持");
        // 第二次及以后：瞬间跳变到超出值 30s（29s+1s），持续保持（不回退）
        let m = response_params("GetPositionInfo", &av);
        assert_eq!(m.get("RelTime").map(|s| s.as_str()), Some("00:00:30"));
        assert!(av.force_complete(), "返回超出进度后标志应保持（持续确认播完）");
        let m = response_params("GetPositionInfo", &av);
        assert_eq!(m.get("RelTime").map(|s| s.as_str()), Some("00:00:30"));
        assert!(av.force_complete());
        // 换集（SetAVTransportURI）清标志
        av.set_uri("http://x/v2.mp4", "");
        assert!(!av.force_complete());
    }

    /// 强制完成：客户端已确认在播（上次回报 ≥1s）→ **瞬间跳变**到超出总时长的
    /// 进度后持续保持（与安卓端完全对齐，2026-08-25——两端统一"切下进度到头，
    /// 客户端自动切下一个"；平滑递增方案已废弃）。
    #[test]
    fn force_complete_direct_when_client_playing() {
        let av = crate::dlna::av_transport::AvTransport::new();
        av.set_uri("http://x/v.mp4", "");
        av.update_duration(29_000);
        av.update_position(5_000);
        // 正常轮询一次，让 last_reported=5s（客户端已确认在播）
        response_params("GetPositionInfo", &av);
        av.set_force_complete();
        // 已确认在播 → 直接返回超出值 30s（29s+1s，RelTime>TrackDuration）
        let m = response_params("GetPositionInfo", &av);
        assert_eq!(m.get("RelTime").map(|s| s.as_str()), Some("00:00:30"));
        assert!(av.force_complete(), "返回超出进度后标志应保持");
        // 下一轮仍返回超出进度（不回退，持续确认播完）
        let m = response_params("GetPositionInfo", &av);
        assert_eq!(m.get("RelTime").map(|s| s.as_str()), Some("00:00:30"));
    }

    /// 换源（切集）清除强制完成信号与上次回报进度。
    #[test]
    fn set_uri_clears_force_complete() {
        let av = crate::dlna::av_transport::AvTransport::new();
        av.set_force_complete();
        av.set_uri("http://x/v2.mp4", "");
        assert!(!av.force_complete());
        assert_eq!(av.last_reported(), 0);
    }

    /// 短视频伪装（**仅抖音源**，抖音安卓 <=5s 不自动切集适配）：抖音源
    /// URL（douyinvod.com / ott_cast）且真实时长 <6s 时 TrackDuration 恒报 6s
    /// （假装视频大于 5 秒）；播放中报真实进度；自然播完（前端上报
    /// pos=真实dur+1000）→ 进度报 **7s**（虚拟时长 6s+1s 超出——抖音只对
    /// RelTime>TrackDuration 判定播完，==6s 实测不切集）。
    /// 非抖音源即使短视频也不伪装（其他客户端无此 bug，伪装反显异常）。
    #[test]
    fn short_video_faked_to_six_seconds_on_natural_end() {
        let av = crate::dlna::av_transport::AvTransport::new();
        av.set_uri("http://v11-cold1.douyinvod.com/short.mp4?cast_type=ott_cast", "");
        av.update_duration(3_000); // 真实 3s（<6s）
        // 播放中：报真实进度，TrackDuration 报 6s
        av.update_position(2_000);
        let m = response_params("GetPositionInfo", &av);
        assert_eq!(m.get("RelTime").map(|s| s.as_str()), Some("00:00:02"));
        assert_eq!(m.get("TrackDuration").map(|s| s.as_str()), Some("00:00:06"));
        // 自然播完：前端上报 pos=真实dur+1000（4s）→ 伪装成 7s（6s+1s 超出，触发切集）
        av.update_position(4_000);
        let m = response_params("GetPositionInfo", &av);
        assert_eq!(m.get("RelTime").map(|s| s.as_str()), Some("00:00:07"));
        assert_eq!(m.get("TrackDuration").map(|s| s.as_str()), Some("00:00:06"));
        // 非抖音源（其他客户端投屏）短视频 → 不伪装，按真实时长返回
        av.set_uri("http://example.com/short.mp4", "");
        av.update_duration(3_000);
        av.update_position(4_000); // 播完信号 pos=dur+1000
        let m = response_params("GetPositionInfo", &av);
        assert_eq!(m.get("TrackDuration").map(|s| s.as_str()), Some("00:00:03"));
        assert_eq!(m.get("RelTime").map(|s| s.as_str()), Some("00:00:04"));
    }

    /// 短视频伪装：抖音源按下切集（force_complete）→ 进度返回 7s（虚拟时长 6s+1s 超出）。
    #[test]
    fn short_video_faked_to_six_seconds_on_force_complete() {
        let av = crate::dlna::av_transport::AvTransport::new();
        av.set_uri("http://v13-cold.douyinvod.com/short.mp4", ""); // 仅 douyinvod 域名也可识别
        av.update_duration(4_000); // 真实 4s（<6s）
        av.update_position(1_000);
        av.set_force_complete();
        let m = response_params("GetPositionInfo", &av);
        assert_eq!(m.get("RelTime").map(|s| s.as_str()), Some("00:00:07"));
        assert_eq!(m.get("TrackDuration").map(|s| s.as_str()), Some("00:00:06"));
        assert!(av.force_complete(), "换集前标志应保持（持续返回 7s）");
        let m = response_params("GetPositionInfo", &av);
        assert_eq!(m.get("RelTime").map(|s| s.as_str()), Some("00:00:07"));
    }

    /// 短视频伪装边界（抖音源）：抖音按整数秒截断判定（5163ms 也被当 5s），
    /// 故真实时长 <6000ms 一律伪装成 6s 时长、播完/切集进度 7s；
    /// 恰好 6000ms（截断 6s>5s）不伪装；时长未知（流式）不伪装。
    #[test]
    fn short_video_fake_boundary() {
        let av = crate::dlna::av_transport::AvTransport::new();
        av.set_uri("http://v.douyinvod.com/b.mp4?cast_type=ott_cast", "");
        // 恰好 5s → 伪装 6s 时长、播完进度 7s
        av.update_duration(5_000);
        av.update_position(5_000);
        let m = response_params("GetPositionInfo", &av);
        assert_eq!(m.get("TrackDuration").map(|s| s.as_str()), Some("00:00:06"));
        assert_eq!(m.get("RelTime").map(|s| s.as_str()), Some("00:00:07"));
        // 5.163s（真实日志场景，抖音整数秒截断当 5s）→ 也要伪装（时长 6s、进度 7s）
        av.set_uri("http://v.douyinvod.com/b2.mp4", "");
        av.update_duration(5_163);
        av.update_position(6_163); // 播完/切集上报的超实时长进度
        let m = response_params("GetPositionInfo", &av);
        assert_eq!(m.get("TrackDuration").map(|s| s.as_str()), Some("00:00:06"));
        assert_eq!(m.get("RelTime").map(|s| s.as_str()), Some("00:00:07"));
        // 恰好 6s（截断 6s>5s）→ 不伪装，按真实时长返回
        av.set_uri("http://v.douyinvod.com/c.mp4", "");
        av.update_duration(6_000);
        av.update_position(6_000);
        let m = response_params("GetPositionInfo", &av);
        assert_eq!(m.get("TrackDuration").map(|s| s.as_str()), Some("00:00:06"));
        assert_eq!(m.get("RelTime").map(|s| s.as_str()), Some("00:00:06"));
        // 时长未知（流式 dur=0）→ 不伪装，进度按真实值
        av.set_uri("http://v.douyinvod.com/d.mp4", "");
        av.update_position(3_000);
        let m = response_params("GetPositionInfo", &av);
        assert_eq!(m.get("TrackDuration").map(|s| s.as_str()), Some("00:00:00"));
        assert_eq!(m.get("RelTime").map(|s| s.as_str()), Some("00:00:03"));
    }
}
