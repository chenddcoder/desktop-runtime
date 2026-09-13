// 设备描述与 SCPD 生成 —— 从 工具类/dlna-cast 的 device-description.ts 移植
// 纯函数，无副作用，只负责把 DLNA DMR 的 XML 描述拼出来。

use std::sync::Mutex;

pub struct DeviceDesc {
    uuid: String,
    friendly_name: Mutex<String>,
}

impl DeviceDesc {
    pub fn new(uuid: String, friendly_name: String) -> Self {
        Self {
            uuid,
            friendly_name: Mutex::new(friendly_name),
        }
    }

    /// 运行时修改广播名（DLNA 运行中调用同样生效）。
    pub fn set_friendly_name(&self, name: String) {
        *self.friendly_name.lock().unwrap() = name;
    }

    #[allow(dead_code)]
    pub fn location(&self, local_ip: &str, http_port: u16) -> String {
        format!("http://{local_ip}:{http_port}/device-desc.xml")
    }

    /// 按 HTTP 路径返回 (body, content_type)，未知路径返回 None。
    pub fn handle(&self, path: &str) -> Option<(String, String)> {
        let ct = "text/xml; charset=\"utf-8\"".to_string();
        match path {
            "/device-desc.xml" => Some((self.build_device_description(), ct)),
            "/AVTransport-scpd.xml" => Some((Self::build_avtransport_scpd(), ct)),
            "/ConnectionManager-scpd.xml" => Some((Self::build_connection_manager_scpd(), ct)),
            "/RenderingControl-scpd.xml" => Some((Self::build_rendering_control_scpd(), ct)),
            _ => None,
        }
    }

    fn escape_xml(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;")
    }

    /// device-desc.xml：UPnP 设备描述。
    /// ⚠️ `dlna:X_DLNADOC=DMR-1.50` 是 DLNA 设备标识，**不能省**：华为/荣耀（HarmonyOS
    /// 投播）与乐播等客户端据此确认「对面是一台标准 DMR」，缺字段时部分客户端直接过滤
    /// 掉设备（表现为「搜不到设备」）。它的命名空间声明在 root 元素上。
    fn build_device_description(&self) -> String {
        // 先取出广播名快照（Mutex guard 不能跨 format! 占位符作用域存活），再在模板中消费。
        let friendly = Self::escape_xml(&*self.friendly_name.lock().unwrap());
        format!(
            r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0" xmlns:dlna="urn:schemas-dlna-org:device-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <device>
    <deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>
    <friendlyName>{friendly}</friendlyName>
    <manufacturer>QuickAppDesktop</manufacturer>
    <modelName>QuickAppDesktop DMR</modelName>
    <dlna:X_DLNADOC>DMR-1.50</dlna:X_DLNADOC>
    <UDN>uuid:{uuid}</UDN>
    <serviceList>
      <service>
        <serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:AVTransport</serviceId>
        <SCPDURL>/AVTransport-scpd.xml</SCPDURL>
        <controlURL>/AVTransport/control</controlURL>
        <eventSubURL>/AVTransport/event</eventSubURL>
      </service>
      <service>
        <serviceType>urn:schemas-upnp-org:service:RenderingControl:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:RenderingControl</serviceId>
        <SCPDURL>/RenderingControl-scpd.xml</SCPDURL>
        <controlURL>/RenderingControl/control</controlURL>
        <eventSubURL>/RenderingControl/event</eventSubURL>
      </service>
      <service>
        <serviceType>urn:schemas-upnp-org:service:ConnectionManager:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:ConnectionManager</serviceId>
        <SCPDURL>/ConnectionManager-scpd.xml</SCPDURL>
        <controlURL>/ConnectionManager/control</controlURL>
        <eventSubURL>/ConnectionManager/event</eventSubURL>
      </service>
    </serviceList>
  </device>
</root>"#,
            friendly = friendly,
            uuid = Self::escape_xml(&self.uuid)
        )
    }

    fn build_avtransport_scpd() -> String {
        r#"<?xml version="1.0"?>
<scpd xmlns="urn:schemas-upnp-org:service-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <actionList>
    <action><name>SetAVTransportURI</name>
      <argumentList>
        <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
        <argument><name>CurrentURI</name><direction>in</direction><relatedStateVariable>AVTransportURI</relatedStateVariable></argument>
        <argument><name>CurrentURIMetaData</name><direction>in</direction><relatedStateVariable>AVTransportURIMetaData</relatedStateVariable></argument>
      </argumentList>
    </action>
    <action><name>Play</name>
      <argumentList>
        <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
        <argument><name>Speed</name><direction>in</direction><relatedStateVariable>TransportPlaySpeed</relatedStateVariable></argument>
      </argumentList>
    </action>
    <action><name>Pause</name>
      <argumentList><argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument></argumentList>
    </action>
    <action><name>Stop</name>
      <argumentList><argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument></argumentList>
    </action>
    <action><name>Seek</name>
      <argumentList>
        <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
        <argument><name>Unit</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_SeekMode</relatedStateVariable></argument>
        <argument><name>Target</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_SeekTarget</relatedStateVariable></argument>
      </argumentList>
    </action>
    <action><name>GetTransportInfo</name>
      <argumentList>
        <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
        <argument><name>CurrentTransportState</name><direction>out</direction><relatedStateVariable>TransportState</relatedStateVariable></argument>
        <argument><name>CurrentTransportStatus</name><direction>out</direction><relatedStateVariable>TransportStatus</relatedStateVariable></argument>
        <argument><name>CurrentSpeed</name><direction>out</direction><relatedStateVariable>TransportPlaySpeed</relatedStateVariable></argument>
      </argumentList>
    </action>
    <action><name>GetPositionInfo</name>
      <argumentList>
        <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
        <argument><name>Track</name><direction>out</direction><relatedStateVariable>CurrentTrack</relatedStateVariable></argument>
        <argument><name>TrackDuration</name><direction>out</direction><relatedStateVariable>CurrentTrackDuration</relatedStateVariable></argument>
        <argument><name>TrackMetaData</name><direction>out</direction><relatedStateVariable>CurrentTrackMetaData</relatedStateVariable></argument>
        <argument><name>TrackURI</name><direction>out</direction><relatedStateVariable>CurrentTrackURI</relatedStateVariable></argument>
        <argument><name>RelTime</name><direction>out</direction><relatedStateVariable>RelativeTimePosition</relatedStateVariable></argument>
        <argument><name>AbsTime</name><direction>out</direction><relatedStateVariable>AbsoluteTimePosition</relatedStateVariable></argument>
        <argument><name>RelCount</name><direction>out</direction><relatedStateVariable>RelativeCounterPosition</relatedStateVariable></argument>
        <argument><name>AbsCount</name><direction>out</direction><relatedStateVariable>AbsoluteCounterPosition</relatedStateVariable></argument>
      </argumentList>
    </action>
    <action><name>GetDeviceCapabilities</name>
      <argumentList><argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument></argumentList>
    </action>
  </actionList>
  <serviceStateTable>
    <stateVariable sendEvents="yes"><name>TransportState</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="yes"><name>TransportStatus</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="yes"><name>CurrentTrack</name><dataType>i4</dataType></stateVariable>
    <stateVariable sendEvents="yes"><name>CurrentTrackDuration</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="yes"><name>CurrentTrackMetaData</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="yes"><name>AVTransportURI</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="yes"><name>CurrentTrackURI</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_InstanceID</name><dataType>i4</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_SeekMode</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_SeekTarget</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>TransportPlaySpeed</name><dataType>string</dataType></stateVariable>
  </serviceStateTable>
</scpd>"#
            .to_string()
    }

    /// ConnectionManager SCPD。
    /// Sink 段（GetProtocolInfo）用**具体 mime** 而不只是 `video/*`：DLNA 规范里
    /// `video/*` 属非标写法，严格客户端（部分手机端 SDK / 乐播）按 mime 精确匹配，
    /// 只见通配会判「无可用 Sink 格式」→ 不投。末尾保留三条通配兜底。
    fn build_connection_manager_scpd() -> String {
        r#"<?xml version="1.0"?>
<scpd xmlns="urn:schemas-upnp-org:service-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <actionList>
    <action><name>GetProtocolInfo</name>
      <argumentList>
        <argument><name>Source</name><direction>out</direction><relatedStateVariable>SourceProtocolInfo</relatedStateVariable></argument>
        <argument><name>Sink</name><direction>out</direction><relatedStateVariable>SinkProtocolInfo</relatedStateVariable></argument>
      </argumentList>
    </action>
    <action><name>GetCurrentConnectionInfo</name>
      <argumentList>
        <argument><name>ConnectionID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_ConnectionID</relatedStateVariable></argument>
        <argument><name>RcsID</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_RcsID</relatedStateVariable></argument>
        <argument><name>AVTransportID</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_AVTransportID</relatedStateVariable></argument>
        <argument><name>ProtocolInfo</name><direction>out</direction><relatedStateVariable>ProtocolInfo</relatedStateVariable></argument>
        <argument><name>PeerConnectionManager</name><direction>out</direction><relatedStateVariable>PeerConnectionManager</relatedStateVariable></argument>
        <argument><name>PeerConnectionID</name><direction>out</direction><relatedStateVariable>PeerConnectionID</relatedStateVariable></argument>
        <argument><name>Direction</name><direction>out</direction><relatedStateVariable>Direction</relatedStateVariable></argument>
        <argument><name>Status</name><direction>out</direction><relatedStateVariable>Status</relatedStateVariable></argument>
      </argumentList>
    </action>
  </actionList>
  <serviceStateTable>
    <stateVariable sendEvents="yes"><name>SourceProtocolInfo</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="yes"><name>SinkProtocolInfo</name><dataType>string</dataType>
      <allowedValueList>
        <allowedValue>http-get:*:video/mp4:*</allowedValue>
        <allowedValue>http-get:*:video/mpeg:*</allowedValue>
        <allowedValue>http-get:*:video/quicktime:*</allowedValue>
        <allowedValue>http-get:*:video/webm:*</allowedValue>
        <allowedValue>http-get:*:video/x-matroska:*</allowedValue>
        <allowedValue>http-get:*:video/x-msvideo:*</allowedValue>
        <allowedValue>http-get:*:audio/mpeg:*</allowedValue>
        <allowedValue>http-get:*:audio/mp4:*</allowedValue>
        <allowedValue>http-get:*:audio/aac:*</allowedValue>
        <allowedValue>http-get:*:image/jpeg:*</allowedValue>
        <allowedValue>http-get:*:image/png:*</allowedValue>
        <allowedValue>http-get:*:application/vnd.apple.mpegurl:*</allowedValue>
        <allowedValue>http-get:*:application/x-mpegURL:*</allowedValue>
        <allowedValue>http-get:*:video/*:*</allowedValue>
        <allowedValue>http-get:*:audio/*:*</allowedValue>
        <allowedValue>http-get:*:image/*:*</allowedValue>
      </allowedValueList>
    </stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_ConnectionID</name><dataType>i4</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_RcsID</name><dataType>i4</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_AVTransportID</name><dataType>i4</dataType></stateVariable>
  </serviceStateTable>
</scpd>"#
            .to_string()
    }

    fn build_rendering_control_scpd() -> String {
        r#"<?xml version="1.0"?>
<scpd xmlns="urn:schemas-upnp-org:service-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <actionList>
    <action><name>SetVolume</name>
      <argumentList>
        <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
        <argument><name>Channel</name><direction>in</direction><relatedStateVariable>VolumeChannel</relatedStateVariable></argument>
        <argument><name>DesiredVolume</name><direction>in</direction><relatedStateVariable>Volume</relatedStateVariable></argument>
      </argumentList>
    </action>
    <action><name>GetVolume</name>
      <argumentList>
        <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
        <argument><name>Channel</name><direction>in</direction><relatedStateVariable>VolumeChannel</relatedStateVariable></argument>
        <argument><name>CurrentVolume</name><direction>out</direction><relatedStateVariable>Volume</relatedStateVariable></argument>
      </argumentList>
    </action>
    <action><name>SetMute</name>
      <argumentList>
        <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
        <argument><name>Channel</name><direction>in</direction><relatedStateVariable>VolumeChannel</relatedStateVariable></argument>
        <argument><name>DesiredMute</name><direction>in</direction><relatedStateVariable>Mute</relatedStateVariable></argument>
      </argumentList>
    </action>
    <action><name>GetMute</name>
      <argumentList>
        <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
        <argument><name>Channel</name><direction>in</direction><relatedStateVariable>VolumeChannel</relatedStateVariable></argument>
        <argument><name>CurrentMute</name><direction>out</direction><relatedStateVariable>Mute</relatedStateVariable></argument>
      </argumentList>
    </action>
  </actionList>
  <serviceStateTable>
    <stateVariable sendEvents="yes"><name>Volume</name><dataType>i4</dataType></stateVariable>
    <stateVariable sendEvents="yes"><name>Mute</name><dataType>boolean</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_InstanceID</name><dataType>i4</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>VolumeChannel</name><dataType>string</dataType>
      <allowedValueList><allowedValue>Master</allowedValue></allowedValueList>
    </stateVariable>
      </serviceStateTable>
</scpd>"#
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desc() -> DeviceDesc {
        DeviceDesc::new("test-uuid".into(), "扩展屏".into())
    }

    #[test]
    fn device_desc_declares_dlna_dmr_doc() {
        // 华为/荣耀（HarmonyOS 投播）、乐播等客户端靠这两个字段确认「对面是标准 DMR」，
        // 缺任一（漏了 root 上的命名空间声明同样无效）会被过滤 → 设备搜不到。
        // 这是静默失效型回归，用测试锁住。
        let (xml, _) = desc().handle("/device-desc.xml").unwrap();
        assert!(xml.contains(r#"xmlns:dlna="urn:schemas-dlna-org:device-1-0""#));
        assert!(xml.contains("<dlna:X_DLNADOC>DMR-1.50</dlna:X_DLNADOC>"));
        assert!(xml.contains("<deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>"));
    }

    #[test]
    fn connection_manager_sink_lists_concrete_mimes() {
        // Sink 段（GetProtocolInfo）必须给具体 mime：只列 `video/*` 这类通配时，
        // 严格客户端判「无可用 Sink 格式」→ 不投。
        let (xml, _) = desc().handle("/ConnectionManager-scpd.xml").unwrap();
        assert!(xml.contains("http-get:*:video/mp4:*"));
        assert!(xml.contains("http-get:*:application/vnd.apple.mpegurl:*"));
        assert!(xml.contains("http-get:*:video/*:*"));
    }
}
