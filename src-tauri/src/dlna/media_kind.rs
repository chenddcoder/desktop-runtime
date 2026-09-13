// 被投媒体的类型判定（CurrentURI + DIDL-Lite metadata → video / audio / image）。
//
// 为什么需要单独一个模块：
//   前端（esapp-tvcast casting 页）一律用 <video> 播放投屏内容。但 DLNA 推送
//   的不只是视频——华为图库「投屏播放」推的就是 JPEG：
//     http://192.168.1.30:49152/upnp/service/local/393151004/0/0.jpg
//   <video> 加载 jpg 必然 error → 业务层兜底 stop → 手机端看到「投屏失败」。
//   2026-09-13 实测 4 次投屏全部是 .jpg（索引 0/1/2 递增，逐张选图语义），
//   每次都死在同一个地方。判定放在 Rust 侧的原因：这里同时能看到
//   CurrentURI 与 DIDL-Lite（upnp:class / protocolInfo）——比前端嗅探扩展名权威；
//   结果经 dlna://play.mediaType 下发，前端据此分支渲染。
//
// 本模块是纯函数（无 IO / 无锁），便于单测。

/// 被投媒体类型。Unknown 表示判定不出来，前端按自己的兜底逻辑（扩展名）处理。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MediaKind {
    Video,
    Audio,
    Image,
    Unknown,
}

impl MediaKind {
    /// 下发给前端的字面量（dlna://play 的 mediaType 字段）。
    pub fn as_str(self) -> &'static str {
        match self {
            MediaKind::Video => "video",
            MediaKind::Audio => "audio",
            MediaKind::Image => "image",
            MediaKind::Unknown => "unknown",
        }
    }
}

/// 取 `<tag ...>text</tag>` 的 text（跳过标签上的属性）。
/// 用最朴素的字符串扫描而非引 XML 解析器：DIDL-Lite 由手机端生成，结构简单，
/// 且解析失败时我们只想降级为 Unknown，不希望因一个坏字符整条链路报错。
fn element_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let start = xml.find(&open)?;
    let gt = start + xml[start..].find('>')?;
    let close = format!("</{tag}>");
    let end = gt + 1 + xml[gt + 1..].find(&close)?;
    let text = xml[gt + 1..end].trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

/// 取 `attr="value"` 的 value（DIDL 的 protocolInfo 是 res 的属性而非元素）。
fn attr_value(xml: &str, attr: &str) -> Option<String> {
    let needle = format!("{attr}=\"");
    let start = xml.find(&needle)? + needle.len();
    let end = start + xml[start..].find('"')?;
    let v = xml[start..end].trim();
    if v.is_empty() {
        None
    } else {
        Some(v.to_string())
    }
}

/// DIDL-Lite 的 `upnp:class`：`object.item.imageItem.photo` / `.videoItem` / `.audioItem`。
/// 华为图库的 jpg 条目即为 `object.item.imageItem.photo`。
pub fn class_of(meta: &str) -> Option<String> {
    element_text(meta, "upnp:class").or_else(|| element_text(meta, "class"))
}

/// protocolInfo 第四段 mime：`http-get:*:image/jpeg:DLNA.ORG_PN=JPEG_LRG` → `image/jpeg`。
pub fn mime_of(meta: &str) -> Option<String> {
    let pi = attr_value(meta, "protocolInfo")?;
    let parts: Vec<&str> = pi.split(':').collect();
    // 标准 4 段：protocol:network:contentFormat:additionalInfo
    let mime = parts.get(2).copied().unwrap_or("").trim();
    if mime.is_empty() || mime == "*" {
        None
    } else {
        Some(mime.to_string())
    }
}

/// URI 的扩展名（小写、去 query/fragment）。取不到返回空串。
pub fn ext_of(uri: &str) -> String {
    let path = uri.split(['?', '#']).next().unwrap_or(uri);
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rsplit_once('.') {
        Some((_, ext)) if !ext.is_empty() && ext.len() <= 5 => ext.to_ascii_lowercase(),
        _ => String::new(),
    }
}

/// 按「DIDL 元数据 → protocolInfo mime → URI 扩展名」的优先级判定媒体类型。
/// 元数据最权威（手机端自己声明的 upnp:class），扩展名只作兜底
/// （部分客户端 URL 无扩展名，如抖音的调度链接）。
pub fn detect(uri: &str, meta: &str) -> MediaKind {
    if let Some(class) = class_of(meta) {
        let c = class.to_ascii_lowercase();
        if c.contains("imageitem") || c.contains("image") {
            return MediaKind::Image;
        }
        if c.contains("audioitem") || c.contains("audio") {
            return MediaKind::Audio;
        }
        if c.contains("videoitem") || c.contains("video") {
            return MediaKind::Video;
        }
    }
    if let Some(mime) = mime_of(meta) {
        let m = mime.to_ascii_lowercase();
        if m.starts_with("image/") {
            return MediaKind::Image;
        }
        if m.starts_with("audio/") {
            return MediaKind::Audio;
        }
        if m.starts_with("video/") {
            return MediaKind::Video;
        }
    }
    // 华为图库的 m3u8 播放列表：application/vnd.apple.mpegurl → 按视频处理
    if let Some(mime) = mime_of(meta) {
        let m = mime.to_ascii_lowercase();
        if m.contains("mpegurl") || m.contains("dash+xml") {
            return MediaKind::Video;
        }
    }
    match ext_of(uri).as_str() {
        "jpg" | "jpeg" | "jpe" | "jfif" | "png" | "bmp" | "gif" | "webp" | "heic" | "heif"
        | "tif" | "tiff" | "avif" => MediaKind::Image,
        "mp3" | "aac" | "m4a" | "flac" | "wav" | "ogg" | "oga" | "wma" | "ape" | "opus" => {
            MediaKind::Audio
        }
        "mp4" | "m4v" | "mov" | "mkv" | "webm" | "avi" | "ts" | "m2ts" | "mpg" | "mpeg" | "flv"
        | "wmv" | "3gp" | "rmvb" | "m3u8" | "mpd" => MediaKind::Video,
        _ => MediaKind::Unknown,
    }
}

/// 一行日志摘要（SetAVTransportURI 落盘用）：把判定依据全摊开，便于复现
/// 「为什么这次被判成图片」。
pub fn describe(uri: &str, meta: &str) -> String {
    format!(
        "mediaType={} class={:?} mime={:?} ext={:?}",
        detect(uri, meta).as_str(),
        class_of(meta),
        mime_of(meta),
        ext_of(uri)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 华为图库真实报文（2026-09-13 实测，metaLen≈586）。
    const HUAWEI_JPG_META: &str = r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/"><item id="0" parentID="-1" restricted="0"><dc:title>0.jpg</dc:title><upnp:class>object.item.imageItem.photo</upnp:class><res protocolInfo="http-get:*:image/jpeg:DLNA.ORG_PN=JPEG_LRG">http://192.168.1.30:49152/upnp/service/local/393151004/0/0.jpg</res></item></DIDL-Lite>"#;

    const VIDEO_META: &str = r#"<DIDL-Lite><item><dc:title>a.mp4</dc:title><upnp:class>object.item.videoItem</upnp:class><res protocolInfo="http-get:*:video/mp4:DLNA.ORG_PN=AVC_MP4_MP_HD_720p_AAC">http://192.168.1.30:1/a.mp4</res></item></DIDL-Lite>"#;

    #[test]
    fn huawei_gallery_jpg_is_image() {
        let uri = "http://192.168.1.30:49152/upnp/service/local/393151004/0/0.jpg";
        assert_eq!(detect(uri, HUAWEI_JPG_META), MediaKind::Image);
        assert_eq!(detect(uri, HUAWEI_JPG_META).as_str(), "image");
        assert_eq!(
            class_of(HUAWEI_JPG_META).as_deref(),
            Some("object.item.imageItem.photo")
        );
        assert_eq!(mime_of(HUAWEI_JPG_META).as_deref(), Some("image/jpeg"));
        assert_eq!(ext_of(uri), "jpg");
    }

    #[test]
    fn upnp_class_beats_uri_extension() {
        // 元数据说是视频，但 URI 结尾是 .jpg（客户端把封面当 URI 的畸形报文）：
        // 以元数据为准（客户端自己的声明）。
        assert_eq!(detect("http://h/a.jpg", VIDEO_META), MediaKind::Video);
    }

    #[test]
    fn extension_is_fallback_when_meta_empty() {
        assert_eq!(detect("http://h/a.JPG", ""), MediaKind::Image);
        assert_eq!(detect("http://h/a.jpeg?t=1", ""), MediaKind::Image);
        assert_eq!(detect("http://h/a.png#f", ""), MediaKind::Image);
        assert_eq!(detect("http://h/a.mkv", ""), MediaKind::Video);
        assert_eq!(detect("http://h/a.m3u8", ""), MediaKind::Video);
        assert_eq!(detect("http://h/a.mp3", ""), MediaKind::Audio);
    }

    #[test]
    fn protocol_info_mime_used_when_class_absent() {
        let meta = r#"<res protocolInfo="http-get:*:image/png:DLNA.ORG_PN=PNG_LRG">http://h/1</res>"#;
        assert_eq!(detect("http://h/1", meta), MediaKind::Image);
    }

    #[test]
    fn hls_mime_counts_as_video() {
        let meta = r#"<res protocolInfo="http-get:*:application/vnd.apple.mpegurl:*">http://h/x</res>"#;
        assert_eq!(detect("http://h/x", meta), MediaKind::Video);
    }

    #[test]
    fn douyin_signed_url_without_extension_is_unknown() {
        // 抖音调度链接无扩展名、无 DIDL → Unknown（前端按既有 <video> 路径处理）
        assert_eq!(detect("https://v3-web.douyinvod.com/abc/def/", ""), MediaKind::Unknown);
    }

    #[test]
    fn extension_with_query_and_long_suffix() {
        // 扩展名超长（>5）视为无扩展名：避免 "index.bundle" 这类被误当媒体后缀
        assert_eq!(ext_of("http://h/a.bundle"), "");
        assert_eq!(detect("http://h/a.bundle", ""), MediaKind::Unknown);
        assert_eq!(ext_of("http://h/a.jpg?v=2"), "jpg");
        // 无点号的路径（华为图库之外的部分客户端会给无扩展名 URL）
        assert_eq!(ext_of("http://h/upnp/service/local/1/0/0"), "");
    }

    #[test]
    fn describe_exposes_evidence() {
        let s = describe("http://h/a.jpg", HUAWEI_JPG_META);
        assert!(s.contains("mediaType=image"), "{s}");
        assert!(s.contains("imageItem"), "{s}");
    }
}
