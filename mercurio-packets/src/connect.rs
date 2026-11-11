//! CONNECT, the first packet a client sends: who it is, its will, its
//! credentials and, in MQTT 5, its limits (section 3.1).

use core::mem;

#[cfg(not(feature = "std"))]
use alloc::{string::String, vec::Vec};

use bytes::{Buf, BufMut, Bytes, BytesMut};

use mercurio_core::{
    codec::{Decoder, Encoder},
    error::EncodeError,
    error::Error,
    properties::*,
    protocol::ProtocolVersion,
    qos::QoS,
    reason::ReasonCode,
};

use crate::{push_user_property, set_once, Packet, PacketType};

/// The Connect Flags byte (section 3.1.2.3). It says which optional fields
/// of the payload follow, so [`ConnectPacket`] derives it from the payload
/// on encode instead of storing it.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct ConnectFlags {
    user_name: bool,
    password: bool,
    will_retain: bool,
    will_qos: QoS,
    will_flag: bool,
    clean_start: bool,
}

impl ConnectFlags {
    fn of(packet: &ConnectPacket) -> ConnectFlags {
        let will = packet.payload.will.as_ref();
        ConnectFlags {
            user_name: packet.payload.user_name.is_some(),
            password: packet.payload.password.is_some(),
            will_retain: will.is_some_and(|will| will.retain),
            will_qos: will.map_or(QoS::AtMostOnce, |will| will.qos),
            will_flag: will.is_some(),
            clean_start: packet.clean_start,
        }
    }
}

impl Encoder for ConnectFlags {
    fn encode(&self, buffer: &mut BytesMut) -> Result<(), EncodeError> {
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
        Ok(())
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

        // Will QoS and retain only mean something with a will [MQTT-3.1.2-11]
        if (byte & 0b0000_0100) == 0 && (byte & 0b0011_1000) != 0 {
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
            flags.will_qos = QoS::try_from((byte >> 3) & 0b0000_0011)
                .map_err(|_| Error::from(ReasonCode::MalformedPacket))?;
        }

        Ok(flags)
    }
}

#[derive(Clone, Default, Debug, PartialEq, Eq)]
/// MQTT 5 only.
pub struct ConnectProperties {
    /// Seconds the session outlives the connection. Absent means 0, the
    /// session ends with the connection. `u32::MAX` means it never expires.
    pub session_expiry_interval: Option<SessionExpiryInterval>,
    /// QoS 1 and 2 PUBLISH packets the client handles at once. Absent means
    /// 65,535.
    pub receive_maximum: Option<ReceiveMaximum>,
    /// Largest packet, in bytes, the client accepts. Absent means no limit
    /// beyond the protocol's.
    pub maximum_packet_size: Option<MaximumPacketSize>,
    /// Highest topic alias the client accepts from the server. Absent means
    /// 0: the server may not use topic aliases.
    pub topic_alias_maximum: Option<TopicAliasMaximum>,
    /// 1 asks the server for Response Information in the CONNACK.
    pub request_response_information: Option<RequestResponseInformation>,
    /// 0 asks the server to send reason strings and user properties only on
    /// PUBLISH, CONNACK and DISCONNECT. Absent means 1.
    pub request_problem_information: Option<RequestProblemInformation>,
    pub user_property: Option<Vec<UserProperty>>,
    /// Set to start enhanced authentication, continued with AUTH packets
    /// (section 4.12).
    pub authentication_method: Option<AuthenticationMethod>,
    pub authentication_data: Option<AuthenticationData>,
}

impl Encoder for ConnectProperties {
    fn encode(&self, buffer: &mut BytesMut) -> Result<(), EncodeError> {
        crate::check_authentication(&self.authentication_method, &self.authentication_data)?;
        self.session_expiry_interval.encode(buffer)?;
        self.receive_maximum.encode(buffer)?;
        self.maximum_packet_size.encode(buffer)?;
        self.topic_alias_maximum.encode(buffer)?;
        self.request_response_information.encode(buffer)?;
        self.request_problem_information.encode(buffer)?;
        self.user_property.encode(buffer)?;
        self.authentication_method.encode(buffer)?;
        self.authentication_data.encode(buffer)?;
        Ok(())
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

        let mut properties = ConnectProperties::default();

        decode_property_list(buffer, |property| match property {
            SessionExpiryInterval(v) => set_once(&mut properties.session_expiry_interval, v),
            AuthenticationMethod(v) => set_once(&mut properties.authentication_method, v),
            AuthenticationData(v) => set_once(&mut properties.authentication_data, v),
            RequestProblemInformation(v) => {
                set_once(&mut properties.request_problem_information, v)
            }
            RequestResponseInformation(v) => {
                set_once(&mut properties.request_response_information, v)
            }
            ReceiveMaximum(v) => set_once(&mut properties.receive_maximum, v),
            TopicAliasMaximum(v) => set_once(&mut properties.topic_alias_maximum, v),
            MaximumPacketSize(v) => set_once(&mut properties.maximum_packet_size, v),
            UserProperty(v) => {
                push_user_property(&mut properties.user_property, v);
                Ok(())
            }
            _ => Err(ReasonCode::MalformedPacket.into()),
        })?;

        crate::check_decoded_authentication(
            &properties.authentication_method,
            &properties.authentication_data,
        )?;

        Ok(properties)
    }
}

#[derive(Clone, Default, Debug, PartialEq, Eq)]
/// Properties of the will message (section 3.1.3.2). MQTT 5 only.
pub struct WillProperties {
    /// Seconds the server waits after the connection closes before it
    /// publishes the will. Absent means 0. If the session resumes in that
    /// time, the will is not published.
    pub will_delay_interval: Option<WillDelayInterval>,
    pub payload_format_indicator: Option<PayloadFormatIndicator>,
    pub message_expiry_interval: Option<MessageExpiryInterval>,
    pub content_type: Option<ContentType>,
    pub response_topic: Option<ResponseTopic>,
    pub correlation_data: Option<CorrelationData>,
    pub user_property: Option<Vec<UserProperty>>,
}

impl Encoder for WillProperties {
    fn encode(&self, buffer: &mut BytesMut) -> Result<(), EncodeError> {
        self.will_delay_interval.encode(buffer)?;
        self.payload_format_indicator.encode(buffer)?;
        self.message_expiry_interval.encode(buffer)?;
        self.content_type.encode(buffer)?;
        self.response_topic.encode(buffer)?;
        self.correlation_data.encode(buffer)?;
        self.user_property.encode(buffer)?;
        Ok(())
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

        let mut properties = WillProperties::default();

        decode_property_list(buffer, |property| match property {
            WillDelayInterval(v) => set_once(&mut properties.will_delay_interval, v),
            PayloadFormatIndicator(v) => set_once(&mut properties.payload_format_indicator, v),
            MessageExpiryInterval(v) => set_once(&mut properties.message_expiry_interval, v),
            ContentType(v) => set_once(&mut properties.content_type, v),
            ResponseTopic(v) => set_once(&mut properties.response_topic, v),
            CorrelationData(v) => set_once(&mut properties.correlation_data, v),
            UserProperty(v) => {
                push_user_property(&mut properties.user_property, v);
                Ok(())
            }
            _ => Err(ReasonCode::MalformedPacket.into()),
        })?;

        Ok(properties)
    }
}

/// The will message: what the server publishes for the client when the
/// connection closes without a normal DISCONNECT (section 3.1.2.5).
#[derive(Clone, Default, Debug, Eq, PartialEq)]
pub struct Will {
    pub qos: QoS,
    /// Publish the will as a retained message.
    pub retain: bool,
    /// MQTT 5 only.
    pub properties: Option<WillProperties>,
    pub topic: String,
    pub payload: Bytes,
}

/// The CONNECT payload. Each optional field is on the wire exactly when it
/// is set here: the Connect Flags are derived from it.
#[derive(Clone, Default, Debug, Eq, PartialEq)]
pub struct ConnectPayload {
    /// May be empty, and the server then assigns one (section 3.1.3.1).
    pub client_id: String,
    pub will: Option<Will>,
    pub user_name: Option<String>,
    /// MQTT 3.x doesn't allow a password without a user name
    /// [MQTT-3.1.2-22].
    pub password: Option<Bytes>,
}

impl ConnectPayload {
    /// Writes the payload; the will properties only in MQTT 5.
    fn encode(&self, buffer: &mut BytesMut, version: ProtocolVersion) -> Result<(), EncodeError> {
        self.client_id.encode(buffer)?;

        if let Some(will) = &self.will {
            if version.supports_properties() {
                // Will properties length must be encoded even if 0
                crate::encode_length(will.properties.encoded_size(), buffer)?;
                will.properties.encode(buffer)?;
            }
            will.topic.encode(buffer)?;
            will.payload.encode(buffer)?;
        }

        self.user_name.encode(buffer)?;
        self.password.encode(buffer)
    }

    /// Size of what [`Self::encode`] writes.
    fn encoded_size(&self, version: ProtocolVersion) -> usize {
        let mut len = self.client_id.encoded_size();

        if let Some(will) = &self.will {
            if version.supports_properties() {
                len += crate::property_list_size(&will.properties);
            }
            len += will.topic.encoded_size() + will.payload.encoded_size();
        }

        len + self.user_name.encoded_size() + self.password.encoded_size()
    }

    /// Reads the payload fields that `flags` announces.
    fn decode<T: Buf>(
        buffer: &mut T,
        version: ProtocolVersion,
        flags: &ConnectFlags,
    ) -> crate::Result<Self> {
        let client_id = String::decode(buffer)?;

        let will = if flags.will_flag {
            // Will properties only exist in MQTT 5.0
            let properties = if version.supports_properties() {
                crate::present(WillProperties::decode(buffer)?)
            } else {
                None
            };
            Some(Will {
                qos: flags.will_qos,
                retain: flags.will_retain,
                properties,
                topic: String::decode(buffer)?,
                payload: Bytes::decode(buffer)?,
            })
        } else {
            None
        };

        let user_name = flags
            .user_name
            .then(|| String::decode(buffer))
            .transpose()?;
        let password = flags.password.then(|| Bytes::decode(buffer)).transpose()?;

        Ok(ConnectPayload {
            client_id,
            will,
            user_name,
            password,
        })
    }
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct ConnectPacket {
    /// Read from the protocol name and level. Selects the wire format on
    /// encode.
    pub protocol_version: ProtocolVersion,
    /// Discard any existing session. MQTT 3.x calls it Clean Session, and
    /// there it also ends the session when the connection closes.
    pub clean_start: bool,
    /// Longest time, in seconds, between two packets from the client. 0 turns
    /// keep alive off (section 3.1.2.10).
    pub keepalive: u16,
    /// MQTT 5 only.
    pub properties: Option<ConnectProperties>,
    pub payload: ConnectPayload,
}

const PACKET_TYPE: u8 = 0x01;

/// A CONNECT names its own protocol version, `protocol_version`; it is
/// what decoding reads, whatever version is passed in.
impl Packet for ConnectPacket {
    /// Fails for a `version` other than the packet's own.
    fn encode(&self, buffer: &mut BytesMut, version: ProtocolVersion) -> Result<(), EncodeError> {
        if version != self.protocol_version {
            return Err(EncodeError::UnsupportedVersion(
                PacketType::Connect,
                version,
            ));
        }
        // MQTT 3.x has no password without a user name [MQTT-3.1.2-22]
        if !version.supports_properties()
            && self.payload.password.is_some()
            && self.payload.user_name.is_none()
        {
            return Err(EncodeError::PasswordWithoutUserName);
        }
        crate::write_packet(buffer, |buffer| {
            let protocol_name = self.protocol_version.protocol_name();
            let protocol_level = self.protocol_version.protocol_level();

            // Fixed header
            buffer.put_u8(PACKET_TYPE << 4);
            crate::encode_length(self.remaining_len(), buffer)?;

            // Variable header
            protocol_name.encode(buffer)?;
            protocol_level.encode(buffer)?;
            ConnectFlags::of(self).encode(buffer)?;
            self.keepalive.encode(buffer)?;

            // Properties only for MQTT 5.0
            if self.protocol_version.supports_properties() {
                crate::encode_length(self.properties.encoded_size(), buffer)?;
                self.properties.encode(buffer)?;
            }

            self.payload.encode(buffer, self.protocol_version)?;
            Ok(())
        })
    }

    fn encoded_size(&self, _: ProtocolVersion) -> usize {
        crate::packet_size(self.remaining_len())
    }

    fn decode<T: Buf>(buffer: &mut T, version: ProtocolVersion) -> crate::Result<Self> {
        crate::decode_packet(buffer, PacketType::Connect, version, |_, body| {
            Self::decode_body(body)
        })
    }
}

impl ConnectPacket {
    fn remaining_len(&self) -> usize {
        let mut len = self.protocol_version.protocol_name().encoded_size()
            + self.protocol_version.protocol_level().encoded_size()
            + ConnectFlags::default().encoded_size()
            + self.keepalive.encoded_size();

        // Properties only for MQTT 5.0
        if self.protocol_version.supports_properties() {
            len += crate::property_list_size(&self.properties);
        }

        len + self.payload.encoded_size(self.protocol_version)
    }

    fn decode_body<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        // Parse protocol name and level to determine version
        let protocol_name = String::decode(buffer)?;
        let protocol_level = u8::decode(buffer)?;

        let protocol_version = ProtocolVersion::from_name_and_level(&protocol_name, protocol_level)
            .ok_or(ReasonCode::UnsupportedProtocolVersion)?;

        let flags = ConnectFlags::decode(buffer)?;

        // MQTT 3.x has no password without a user name [MQTT-3.1.2-22]
        if flags.password && !flags.user_name && !protocol_version.supports_properties() {
            return Err(ReasonCode::MalformedPacket.into());
        }

        let keepalive = u16::decode(buffer)?;

        // Properties only exist in MQTT 5.0
        let properties = if protocol_version.supports_properties() {
            crate::present(ConnectProperties::decode(buffer)?)
        } else {
            None
        };

        let payload = ConnectPayload::decode(buffer, protocol_version, &flags)?;

        Ok(ConnectPacket {
            protocol_version,
            clean_start: flags.clean_start,
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
    fn test_payload_encoder_writes_empty_will_properties() {
        let payload = ConnectPayload {
            client_id: "c".into(),
            will: Some(Will {
                topic: "t".into(),
                payload: Bytes::from_static(b"p"),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut encoded = BytesMut::new();
        payload.encode(&mut encoded, ProtocolVersion::V5).unwrap();
        assert_eq!(
            &encoded[..],
            [0x00, 0x01, b'c', 0x00, 0x00, 0x01, b't', 0x00, 0x01, b'p']
        );
        assert_eq!(payload.encoded_size(ProtocolVersion::V5), encoded.len());
    }

    #[test]
    fn test_flags_follow_the_payload() {
        let packet = ConnectPacket {
            protocol_version: ProtocolVersion::V3_1_1,
            clean_start: false,
            keepalive: 0,
            properties: None,
            payload: ConnectPayload {
                client_id: "c".into(),
                will: Some(Will {
                    qos: QoS::ExactlyOnce,
                    retain: true,
                    topic: "t".into(),
                    ..Default::default()
                }),
                user_name: Some("u".into()),
                password: None,
            },
        };
        let mut encoded = BytesMut::new();
        packet
            .encode(&mut encoded, packet.protocol_version)
            .unwrap();
        // User name, will retain, will QoS 2 and will flag
        assert_eq!(encoded[9], 0b1011_0100);
        assert_eq!(
            ConnectPacket::decode(&mut encoded.freeze(), ProtocolVersion::V5).unwrap(),
            packet
        );
    }

    #[test]
    fn test_connect_packet_encoding() {
        let expected = vec![
            0x10, 0x10, 0x00, 0x04, 0x4d, 0x51, 0x54, 0x54, 0x05, 0x02, 0x00, 0x3c, 0x03, 0x21,
            0x00, 0x14, 0x00, 0x00,
        ];

        let properties = ConnectProperties {
            receive_maximum: ReceiveMaximum::new(20).into(),
            ..Default::default()
        };

        let payload = ConnectPayload::default();
        let packet = ConnectPacket {
            protocol_version: ProtocolVersion::V5,
            clean_start: true,
            keepalive: 60,
            properties: properties.into(),
            payload,
        };

        let mut encoded = BytesMut::new();
        packet
            .encode(&mut encoded, packet.protocol_version)
            .unwrap();

        assert_eq!(encoded, expected);

        let mut bytes = Bytes::from(expected);

        let new_packet =
            ConnectPacket::decode(&mut bytes, ProtocolVersion::V5).expect("Unexpected error");
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
            will: Some(Will {
                qos: QoS::AtLeastOnce,
                retain: true,
                properties: will_properties.into(),
                topic: String::from("test_topic"),
                payload: Bytes::from("test_payload"),
            }),
            user_name: String::from("test_user").into(),
            password: Bytes::from("test_pwd").into(),
            ..Default::default()
        };

        let packet = ConnectPacket {
            protocol_version: ProtocolVersion::V5,
            clean_start: true,
            keepalive: 60,
            properties: properties.into(),
            payload,
        };

        let mut encoded = BytesMut::new();
        packet
            .encode(&mut encoded, packet.protocol_version)
            .unwrap();

        assert_eq!(encoded, expected);

        let mut bytes = Bytes::from(expected);

        let new_packet =
            ConnectPacket::decode(&mut bytes, ProtocolVersion::V5).expect("Unexpected error");
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

        let payload = ConnectPayload::default();
        let packet = ConnectPacket {
            protocol_version: ProtocolVersion::V3_1_1,
            clean_start: true,
            keepalive: 60,
            properties: None, // No properties for 3.1.1
            payload,
        };

        let mut encoded = BytesMut::new();
        packet
            .encode(&mut encoded, packet.protocol_version)
            .unwrap();

        assert_eq!(encoded.to_vec(), expected);

        let mut bytes = Bytes::from(expected);
        let new_packet =
            ConnectPacket::decode(&mut bytes, ProtocolVersion::V5).expect("Unexpected error");
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
        let packet =
            ConnectPacket::decode(&mut bytes, ProtocolVersion::V5).expect("Unexpected error");

        assert_eq!(packet.protocol_version, ProtocolVersion::V3_1);
        assert!(packet.clean_start);
        assert_eq!(packet.keepalive, 60);
        assert!(packet.properties.is_none());
    }
}
