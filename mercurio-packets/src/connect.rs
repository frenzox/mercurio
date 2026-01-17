use std::mem;

use bytes::{Buf, BufMut, Bytes, BytesMut};

use mercurio_core::{
    codec::{Decoder, Encoder, VariableByteInteger},
    error::Error,
    properties::*,
    protocol::ProtocolVersion,
    qos::QoS,
    reason::ReasonCode,
};

#[derive(Default, Debug, PartialEq, Eq)]
pub struct ConnectFlags {
    pub user_name: bool,
    pub password: bool,
    pub will_retain: bool,
    pub will_qos: QoS,
    pub will_flag: bool,
    pub clean_start: bool,
}

impl Encoder for ConnectFlags {
    fn encode(&self, buffer: &mut BytesMut) {
        let mut flags: u8 = 0;

        if self.user_name {
            flags = 0b1000_0000;
        }

        if self.password {
            flags |= 0b0100_0000;
        }

        if self.will_retain {
            flags |= 0b0010_0000;
        }

        if self.will_flag {
            flags |= 0b0000_0100;
            flags |= (self.will_qos as u8) << 3;
        }

        if self.clean_start {
            flags |= 0b0000_0010;
        }

        buffer.put_u8(flags);
    }

    fn encoded_size(&self) -> usize {
        mem::size_of::<u8>()
    }
}

impl Decoder for ConnectFlags {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        if !buffer.has_remaining() {
            return Err(Error::PacketIncomplete);
        }

        let byte = buffer.get_u8();
        if (byte & 0b0000_0001) != 0 {
            return Err(ReasonCode::MalformedPacket.into());
        }

        let mut flags = ConnectFlags {
            user_name: (byte & 0b1000_0000) != 0,
            password: (byte & 0b0100_0000) != 0,
            will_retain: (byte & 0b0010_0000) != 0,
            clean_start: (byte & 0b0000_0010) != 0,
            ..Default::default()
        };

        if (byte & 0b0000_0100) != 0 {
            flags.will_flag = true;
            flags.will_qos = ((byte >> 3) & 0b0000_0011).into();
        }

        if flags.will_qos != QoS::Invalid {
            Ok(flags)
        } else {
            Err(ReasonCode::MalformedPacket.into())
        }
    }
}

#[derive(Default, Debug, PartialEq, Eq)]
pub struct ConnectProperties {
    pub session_expiry_interval: Option<SessionExpiryInterval>,
    pub receive_maximum: Option<ReceiveMaximum>,
    pub maximum_packet_size: Option<MaximumPacketSize>,
    pub topic_alias_maximum: Option<TopicAliasMaximum>,
    pub request_response_information: Option<RequestResponseInformation>,
    pub request_problem_information: Option<RequestProblemInformation>,
    pub user_property: Option<Vec<UserProperty>>,
    pub authentication_method: Option<AuthenticationMethod>,
    pub authentication_data: Option<AuthenticationData>,
}

impl Encoder for ConnectProperties {
    fn encode(&self, buffer: &mut BytesMut) {
        self.session_expiry_interval.encode(buffer);
        self.receive_maximum.encode(buffer);
        self.maximum_packet_size.encode(buffer);
        self.topic_alias_maximum.encode(buffer);
        self.request_response_information.encode(buffer);
        self.request_problem_information.encode(buffer);
        self.user_property.encode(buffer);
        self.authentication_method.encode(buffer);
        self.authentication_data.encode(buffer);
    }

    fn encoded_size(&self) -> usize {
        let mut len = 0;

        len += self.session_expiry_interval.encoded_size();
        len += self.receive_maximum.encoded_size();
        len += self.maximum_packet_size.encoded_size();
        len += self.topic_alias_maximum.encoded_size();
        len += self.request_response_information.encoded_size();
        len += self.request_problem_information.encoded_size();
        len += self.user_property.encoded_size();
        len += self.authentication_method.encoded_size();
        len += self.authentication_data.encoded_size();

        len
    }
}

impl Decoder for ConnectProperties {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        use Property::*;

        let len = VariableByteInteger::decode(buffer)?;
        let mut properties = ConnectProperties::default();

        if len.0 == 0 {
            return Ok(properties);
        } else if (buffer.remaining() as u32) < len.0 {
            return Err(ReasonCode::MalformedPacket.into());
        }

        let mut encoded_properties = buffer.take(len.0 as usize);

        while encoded_properties.has_remaining() {
            match Property::decode(&mut encoded_properties)? {
                SessionExpiryInterval(v) => properties.session_expiry_interval = Some(v),
                AuthenticationMethod(v) => properties.authentication_method = Some(v),
                AuthenticationData(v) => properties.authentication_data = Some(v),
                RequestProblemInformation(v) => properties.request_problem_information = Some(v),
                ReceiveMaximum(v) => properties.receive_maximum = Some(v),
                TopicAliasMaximum(v) => properties.topic_alias_maximum = Some(v),
                MaximumPacketSize(v) => properties.maximum_packet_size = Some(v),
                UserProperty(v) => {
                    if let Some(vec) = &mut properties.user_property {
                        vec.push(v);
                    } else {
                        let vec = vec![v];
                        properties.user_property = Some(vec);
                    }
                }
                _ => return Err(ReasonCode::MalformedPacket.into()),
            }
        }

        Ok(properties)
    }
}

#[derive(Default, Debug, PartialEq, Eq)]
pub struct WillProperties {
    pub will_delay_interval: Option<WillDelayInterval>,
    pub payload_format_indicator: Option<PayloadFormatIndicator>,
    pub message_expiry_interval: Option<MessageExpiryInterval>,
    pub content_type: Option<ContentType>,
    pub response_topic: Option<ResponseTopic>,
    pub correlation_data: Option<CorrelationData>,
    pub user_property: Option<Vec<UserProperty>>,
}

impl Encoder for WillProperties {
    fn encode(&self, buffer: &mut BytesMut) {
        self.will_delay_interval.encode(buffer);
        self.payload_format_indicator.encode(buffer);
        self.message_expiry_interval.encode(buffer);
        self.content_type.encode(buffer);
        self.response_topic.encode(buffer);
        self.correlation_data.encode(buffer);
        self.user_property.encode(buffer);
    }

    fn encoded_size(&self) -> usize {
        let mut len = 0;

        len += self.will_delay_interval.encoded_size();
        len += self.payload_format_indicator.encoded_size();
        len += self.message_expiry_interval.encoded_size();
        len += self.content_type.encoded_size();
        len += self.response_topic.encoded_size();
        len += self.correlation_data.encoded_size();
        len += self.user_property.encoded_size();

        len
    }
}

impl Decoder for WillProperties {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        use Property::*;

        let len = VariableByteInteger::decode(buffer)?;
        let mut properties = WillProperties::default();

        if len.0 == 0 {
            return Ok(properties);
        } else if (buffer.remaining() as u32) < len.0 {
            return Err(Error::PacketIncomplete);
        }

        let mut encoded_properties = buffer.take(len.0 as usize);

        while encoded_properties.has_remaining() {
            match Property::decode(&mut encoded_properties)? {
                WillDelayInterval(v) => properties.will_delay_interval = Some(v),
                PayloadFormatIndicator(v) => properties.payload_format_indicator = Some(v),
                MessageExpiryInterval(v) => properties.message_expiry_interval = Some(v),
                ContentType(v) => properties.content_type = Some(v),
                ResponseTopic(v) => properties.response_topic = Some(v),
                CorrelationData(v) => properties.correlation_data = Some(v),
                UserProperty(v) => {
                    if let Some(vec) = &mut properties.user_property {
                        vec.push(v);
                    } else {
                        let vec = vec![v];
                        properties.user_property = Some(vec);
                    }
                }

                _ => return Err(ReasonCode::MalformedPacket.into()),
            }
        }

        Ok(properties)
    }
}

#[derive(Default, Debug, Eq, PartialEq)]
pub struct ConnectPayload {
    pub client_id: String,
    pub will_properties: Option<WillProperties>,
    pub will_topic: Option<String>,
    pub will_payload: Option<Bytes>,
    pub user_name: Option<String>,
    pub password: Option<Bytes>,
}

impl ConnectPayload {
    /// Encode payload with version-specific handling.
    pub fn encode_for_version(
        &self,
        buffer: &mut BytesMut,
        version: ProtocolVersion,
        flags: &ConnectFlags,
    ) {
        self.client_id.encode(buffer);

        // Will properties only for MQTT 5.0
        if flags.will_flag {
            if version.supports_properties() && self.will_properties.encoded_size() > 0 {
                VariableByteInteger(self.will_properties.encoded_size() as u32).encode(buffer);
                self.will_properties.encode(buffer);
            }
            self.will_topic.encode(buffer);
            self.will_payload.encode(buffer);
        }

        if flags.user_name {
            self.user_name.encode(buffer);
        }
        if flags.password {
            self.password.encode(buffer);
        }
    }

    /// Calculate encoded size with version-specific handling.
    pub fn encoded_size_for_version(
        &self,
        version: ProtocolVersion,
        flags: &ConnectFlags,
    ) -> usize {
        let mut len = 0;

        len += self.client_id.encoded_size();

        if flags.will_flag {
            // Will properties only for MQTT 5.0
            if version.supports_properties() && self.will_properties.encoded_size() > 0 {
                len +=
                    VariableByteInteger(self.will_properties.encoded_size() as u32).encoded_size();
                len += self.will_properties.encoded_size();
            }
            len += self.will_topic.encoded_size();
            len += self.will_payload.encoded_size();
        }

        if flags.user_name {
            len += self.user_name.encoded_size();
        }
        if flags.password {
            len += self.password.encoded_size();
        }

        len
    }
}

impl Encoder for ConnectPayload {
    fn encode(&self, buffer: &mut BytesMut) {
        // Default encoding assumes MQTT 5.0 with all optional fields
        self.client_id.encode(buffer);

        if self.will_properties.encoded_size() > 0 {
            VariableByteInteger(self.will_properties.encoded_size() as u32).encode(buffer);
            self.will_properties.encode(buffer);
        }

        self.will_topic.encode(buffer);
        self.will_payload.encode(buffer);
        self.user_name.encode(buffer);
        self.password.encode(buffer);
    }

    fn encoded_size(&self) -> usize {
        let mut len = 0;

        len += self.client_id.encoded_size();
        if self.will_properties.encoded_size() > 0 {
            len += VariableByteInteger(self.will_properties.encoded_size() as u32).encoded_size();
            len += self.will_properties.encoded_size();
        }

        len += self.will_topic.encoded_size();
        len += self.will_payload.encoded_size();
        len += self.user_name.encoded_size();
        len += self.password.encoded_size();

        len
    }
}

impl Decoder for ConnectPayload {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        let payload = ConnectPayload {
            client_id: String::decode(buffer)?,
            ..Default::default()
        };

        Ok(payload)
    }
}

#[derive(Eq, PartialEq, Debug)]
pub struct ConnectPacket {
    pub protocol_version: ProtocolVersion,
    pub flags: ConnectFlags,
    pub keepalive: u16,
    pub properties: Option<ConnectProperties>,
    pub payload: ConnectPayload,
}

const PACKET_TYPE: u8 = 0x01;

impl Encoder for ConnectPacket {
    fn encode(&self, buffer: &mut BytesMut) {
        let protocol_name = self.protocol_version.protocol_name();
        let protocol_level = self.protocol_version.protocol_level();

        let mut remaining_len = 0;

        // Fixed header
        buffer.put_u8(PACKET_TYPE << 4);
        remaining_len += protocol_name.encoded_size();
        remaining_len += protocol_level.encoded_size();
        remaining_len += self.flags.encoded_size();
        remaining_len += self.keepalive.encoded_size();

        // Properties only for MQTT 5.0
        if self.protocol_version.supports_properties() {
            remaining_len +=
                VariableByteInteger(self.properties.encoded_size() as u32).encoded_size();
            remaining_len += self.properties.encoded_size();
        }

        remaining_len += self
            .payload
            .encoded_size_for_version(self.protocol_version, &self.flags);
        VariableByteInteger(remaining_len as u32).encode(buffer);

        // Variable header
        protocol_name.encode(buffer);
        protocol_level.encode(buffer);
        self.flags.encode(buffer);
        self.keepalive.encode(buffer);

        // Properties only for MQTT 5.0
        if self.protocol_version.supports_properties() {
            VariableByteInteger(self.properties.encoded_size() as u32).encode(buffer);
            self.properties.encode(buffer);
        }

        // Payload
        self.payload
            .encode_for_version(buffer, self.protocol_version, &self.flags);
    }
}

impl Decoder for ConnectPacket {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        buffer.advance(1); // Packet type
        let _ = VariableByteInteger::decode(buffer)?; // Remaining length

        // Parse protocol name and level to determine version
        let protocol_name = String::decode(buffer)?;
        let protocol_level = u8::decode(buffer)?;

        let protocol_version = ProtocolVersion::from_name_and_level(&protocol_name, protocol_level)
            .ok_or(ReasonCode::UnsupportedProtocolVersion)?;

        let flags = ConnectFlags::decode(buffer)?;
        let keepalive = u16::decode(buffer)?;

        // Properties only exist in MQTT 5.0
        let properties = if protocol_version.supports_properties() {
            Some(ConnectProperties::decode(buffer)?)
        } else {
            None
        };

        let mut payload = ConnectPayload::decode(buffer)?;

        if flags.will_flag {
            // Will properties only exist in MQTT 5.0
            if protocol_version.supports_properties() {
                payload.will_properties = Some(WillProperties::decode(buffer)?);
            }
            payload.will_topic = Some(String::decode(buffer)?);
            payload.will_payload = Some(Bytes::decode(buffer)?);
        }

        if flags.user_name {
            payload.user_name = Some(String::decode(buffer)?);
        }

        if flags.password {
            payload.password = Some(Bytes::decode(buffer)?);
        }

        Ok(ConnectPacket {
            protocol_version,
            flags,
            keepalive,
            properties,
            payload,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::connect::*;

    #[test]
    fn test_connect_packet_encoding() {
        let expected = vec![
            0x10, 0x10, 0x00, 0x04, 0x4d, 0x51, 0x54, 0x54, 0x05, 0x02, 0x00, 0x3c, 0x03, 0x21,
            0x00, 0x14, 0x00, 0x00,
        ];

        let flags = ConnectFlags {
            user_name: false,
            password: false,
            will_retain: false,
            will_qos: QoS::AtMostOnce,
            will_flag: false,
            clean_start: true,
        };

        let properties = ConnectProperties {
            receive_maximum: ReceiveMaximum::new(20).into(),
            ..Default::default()
        };

        let payload = ConnectPayload::default();
        let packet = ConnectPacket {
            protocol_version: ProtocolVersion::V5,
            flags,
            keepalive: 60,
            properties: properties.into(),
            payload,
        };

        let mut encoded = BytesMut::new();
        packet.encode(&mut encoded);

        assert_eq!(encoded, expected);

        let mut bytes = Bytes::from(expected);

        let new_packet = ConnectPacket::decode(&mut bytes).expect("Unexpected error");
        assert_eq!(packet, new_packet);
    }

    #[test]
    fn test_connect_packet_encode_decode() {
        let expected = vec![
            0x10, 0x52, 0x00, 0x04, 0x4d, 0x51, 0x54, 0x54, 0x05, 0xee, 0x00, 0x3c, 0x08, 0x11,
            0x00, 0x00, 0x00, 0x1e, 0x21, 0x00, 0x14, 0x00, 0x00, 0x0d, 0x08, 0x00, 0x0a, 0x74,
            0x65, 0x73, 0x74, 0x5f, 0x74, 0x6f, 0x70, 0x69, 0x63, 0x00, 0x0a, 0x74, 0x65, 0x73,
            0x74, 0x5f, 0x74, 0x6f, 0x70, 0x69, 0x63, 0x00, 0x0c, 0x74, 0x65, 0x73, 0x74, 0x5f,
            0x70, 0x61, 0x79, 0x6c, 0x6f, 0x61, 0x64, 0x00, 0x09, 0x74, 0x65, 0x73, 0x74, 0x5f,
            0x75, 0x73, 0x65, 0x72, 0x00, 0x08, 0x74, 0x65, 0x73, 0x74, 0x5f, 0x70, 0x77, 0x64,
        ];

        let flags = ConnectFlags {
            user_name: true,
            password: true,
            will_retain: true,
            will_qos: QoS::AtLeastOnce,
            will_flag: true,
            clean_start: true,
        };

        let properties = ConnectProperties {
            session_expiry_interval: SessionExpiryInterval::new(30).into(),
            receive_maximum: ReceiveMaximum::new(20).into(),
            ..Default::default()
        };

        let will_properties = WillProperties {
            response_topic: ResponseTopic::new(String::from("test_topic")).into(),
            ..Default::default()
        };

        let payload = ConnectPayload {
            will_properties: will_properties.into(),
            will_topic: String::from("test_topic").into(),
            will_payload: Bytes::from("test_payload").into(),
            user_name: String::from("test_user").into(),
            password: Bytes::from("test_pwd").into(),
            ..Default::default()
        };

        let packet = ConnectPacket {
            protocol_version: ProtocolVersion::V5,
            flags,
            keepalive: 60,
            properties: properties.into(),
            payload,
        };

        let mut encoded = BytesMut::new();
        packet.encode(&mut encoded);

        assert_eq!(encoded, expected);

        let mut bytes = Bytes::from(expected);

        let new_packet = ConnectPacket::decode(&mut bytes).expect("Unexpected error");
        assert_eq!(packet, new_packet);
    }

    #[test]
    fn test_connect_packet_v3_1_1_encode_decode() {
        // MQTT 3.1.1 CONNECT packet: no properties
        // Protocol: MQTT (2+4=6 bytes), Level: 1 byte, Flags: 1 byte, Keepalive: 2 bytes, Client ID: 2 bytes = 12 total
        let expected = vec![
            0x10, // Packet type: CONNECT
            0x0c, // Remaining length: 12
            0x00, 0x04, 0x4d, 0x51, 0x54, 0x54, // Protocol name: "MQTT"
            0x04, // Protocol level: 4 (MQTT 3.1.1)
            0x02, // Flags: clean_start
            0x00, 0x3c, // Keepalive: 60
            0x00, 0x00, // Client ID: empty
        ];

        let flags = ConnectFlags {
            clean_start: true,
            ..Default::default()
        };

        let payload = ConnectPayload::default();
        let packet = ConnectPacket {
            protocol_version: ProtocolVersion::V3_1_1,
            flags,
            keepalive: 60,
            properties: None, // No properties for 3.1.1
            payload,
        };

        let mut encoded = BytesMut::new();
        packet.encode(&mut encoded);

        assert_eq!(encoded.to_vec(), expected);

        let mut bytes = Bytes::from(expected);
        let new_packet = ConnectPacket::decode(&mut bytes).expect("Unexpected error");
        assert_eq!(packet, new_packet);
    }

    #[test]
    fn test_connect_packet_v3_1_decode() {
        // MQTT 3.1 CONNECT packet: Protocol "MQIsdp", Level 3
        // Protocol: MQIsdp (2+6=8 bytes), Level: 1 byte, Flags: 1 byte, Keepalive: 2 bytes, Client ID: 2 bytes = 14 total
        let input = vec![
            0x10, // Packet type: CONNECT
            0x0e, // Remaining length: 14
            0x00, 0x06, 0x4d, 0x51, 0x49, 0x73, 0x64, 0x70, // Protocol name: "MQIsdp"
            0x03, // Protocol level: 3 (MQTT 3.1)
            0x02, // Flags: clean_start
            0x00, 0x3c, // Keepalive: 60
            0x00, 0x00, // Client ID: empty
        ];

        let mut bytes = Bytes::from(input);
        let packet = ConnectPacket::decode(&mut bytes).expect("Unexpected error");

        assert_eq!(packet.protocol_version, ProtocolVersion::V3_1);
        assert!(packet.flags.clean_start);
        assert_eq!(packet.keepalive, 60);
        assert!(packet.properties.is_none());
    }
}
