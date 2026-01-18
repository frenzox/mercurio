use std::mem;

use bytes::{Buf, BufMut};

use mercurio_core::{
    codec::{Decoder, Encoder, VariableByteInteger},
    error::Error,
    properties::*,
    protocol::ProtocolVersion,
    reason::ReasonCode,
};

#[derive(Default, Debug, PartialEq, Eq)]
pub struct ConnAckFlags {
    pub session_present: bool,
}

impl Encoder for ConnAckFlags {
    fn encode(&self, buffer: &mut bytes::BytesMut) {
        let flags = 0b0000_0001 & (self.session_present as u8);
        buffer.put_u8(flags);
    }

    fn encoded_size(&self) -> usize {
        mem::size_of::<u8>()
    }
}

impl Decoder for ConnAckFlags {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        let encoded = buffer.get_u8();

        if (0b1111_1110 & encoded) != 0 {
            return Err(ReasonCode::MalformedPacket.into());
        }

        Ok(ConnAckFlags {
            session_present: (0b0000_0001 & encoded) != 0,
        })
    }
}

#[derive(Default, Debug, PartialEq, Eq)]
pub struct ConnAckProperties {
    pub session_expiry_interval: Option<SessionExpiryInterval>,
    pub receive_maximum: Option<ReceiveMaximum>,
    pub maximum_qos: Option<MaximumQoS>,
    pub retain_available: Option<RetainAvailable>,
    pub maximum_packet_size: Option<MaximumPacketSize>,
    pub assigned_client_id: Option<AssignedClientIdentifier>,
    pub topic_alias_max: Option<TopicAliasMaximum>,
    pub reason_string: Option<ReasonString>,
    pub user_property: Option<Vec<UserProperty>>,
    pub wildcard_subscription_available: Option<WildcardSubscriptionAvailable>,
    pub subscription_identifier_available: Option<SubscriptionIdentifierAvailable>,
    pub shared_subscription_available: Option<SharedSubscriptionAvailable>,
    pub server_keepalive: Option<ServerKeepAlive>,
    pub response_information: Option<ResponseInformation>,
    pub server_reference: Option<ServerReference>,
    pub authentication_method: Option<AuthenticationMethod>,
    pub authentication_data: Option<AuthenticationData>,
}

impl Encoder for ConnAckProperties {
    fn encode(&self, buffer: &mut bytes::BytesMut) {
        self.session_expiry_interval.encode(buffer);
        self.receive_maximum.encode(buffer);
        self.maximum_qos.encode(buffer);
        self.retain_available.encode(buffer);
        self.maximum_packet_size.encode(buffer);
        self.assigned_client_id.encode(buffer);
        self.topic_alias_max.encode(buffer);
        self.reason_string.encode(buffer);
        self.user_property.encode(buffer);
        self.wildcard_subscription_available.encode(buffer);
        self.subscription_identifier_available.encode(buffer);
        self.shared_subscription_available.encode(buffer);
        self.server_keepalive.encode(buffer);
        self.response_information.encode(buffer);
        self.server_reference.encode(buffer);
        self.authentication_method.encode(buffer);
        self.authentication_data.encode(buffer);
    }

    fn encoded_size(&self) -> usize {
        let mut len: usize = 0;

        len += self.session_expiry_interval.encoded_size();
        len += self.receive_maximum.encoded_size();
        len += self.maximum_qos.encoded_size();
        len += self.retain_available.encoded_size();
        len += self.maximum_packet_size.encoded_size();
        len += self.assigned_client_id.encoded_size();
        len += self.topic_alias_max.encoded_size();
        len += self.reason_string.encoded_size();
        len += self.user_property.encoded_size();
        len += self.wildcard_subscription_available.encoded_size();
        len += self.subscription_identifier_available.encoded_size();
        len += self.shared_subscription_available.encoded_size();
        len += self.server_keepalive.encoded_size();
        len += self.response_information.encoded_size();
        len += self.server_reference.encoded_size();
        len += self.authentication_method.encoded_size();
        len += self.authentication_data.encoded_size();

        len
    }
}

impl Decoder for ConnAckProperties {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        use Property::*;

        let len = VariableByteInteger::decode(buffer)?;
        let mut properties = ConnAckProperties::default();

        if len.0 == 0 {
            return Ok(properties);
        } else if (buffer.remaining() as u32) < len.0 {
            return Err(Error::PacketIncomplete);
        }

        let mut encoded_properties = buffer.take(len.0 as usize);

        while encoded_properties.has_remaining() {
            match Property::decode(&mut encoded_properties)? {
                SessionExpiryInterval(v) => properties.session_expiry_interval = Some(v),
                ReceiveMaximum(v) => properties.receive_maximum = Some(v),
                MaximumQoS(v) => properties.maximum_qos = Some(v),
                RetainAvailable(v) => properties.retain_available = Some(v),
                MaximumPacketSize(v) => properties.maximum_packet_size = Some(v),
                AssignedClientIdentifier(v) => properties.assigned_client_id = Some(v),
                TopicAliasMaximum(v) => properties.topic_alias_max = Some(v),
                ReasonString(v) => properties.reason_string = Some(v),
                UserProperty(v) => {
                    if let Some(vec) = &mut properties.user_property {
                        vec.push(v);
                    } else {
                        let vec = vec![v];
                        properties.user_property = Some(vec);
                    }
                }
                WildcardSubscriptionAvailable(v) => {
                    properties.wildcard_subscription_available = Some(v)
                }
                SubscriptionIdentifierAvailable(v) => {
                    properties.subscription_identifier_available = Some(v)
                }
                SharedSubscriptionAvailable(v) => {
                    properties.shared_subscription_available = Some(v)
                }
                ServerKeepAlive(v) => properties.server_keepalive = Some(v),
                ResponseInformation(v) => properties.response_information = Some(v),
                ServerReference(v) => properties.server_reference = Some(v),
                AuthenticationMethod(v) => properties.authentication_method = Some(v),
                AuthenticationData(v) => properties.authentication_data = Some(v),
                _ => return Err(ReasonCode::MalformedPacket.into()),
            }
        }

        Ok(properties)
    }
}

/// MQTT 3.x return codes for CONNACK.
/// These are different from MQTT 5.0 reason codes.
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum ConnAckReturnCode {
    #[default]
    Accepted = 0x00,
    UnacceptableProtocolVersion = 0x01,
    IdentifierRejected = 0x02,
    ServerUnavailable = 0x03,
    BadUsernameOrPassword = 0x04,
    NotAuthorized = 0x05,
}

impl Encoder for ConnAckReturnCode {
    fn encode(&self, buffer: &mut bytes::BytesMut) {
        buffer.put_u8(*self as u8);
    }

    fn encoded_size(&self) -> usize {
        1
    }
}

impl From<ReasonCode> for ConnAckReturnCode {
    fn from(reason: ReasonCode) -> Self {
        match reason {
            ReasonCode::Success => ConnAckReturnCode::Accepted,
            ReasonCode::UnsupportedProtocolVersion => {
                ConnAckReturnCode::UnacceptableProtocolVersion
            }
            ReasonCode::ClientIdentifierNotValid => ConnAckReturnCode::IdentifierRejected,
            ReasonCode::ServerUnavailable => ConnAckReturnCode::ServerUnavailable,
            ReasonCode::BadUserNameOrPassword => ConnAckReturnCode::BadUsernameOrPassword,
            ReasonCode::NotAuthorized => ConnAckReturnCode::NotAuthorized,
            _ => ConnAckReturnCode::ServerUnavailable, // Default fallback for unmapped codes
        }
    }
}

#[derive(Default, Debug, Eq, PartialEq)]
pub struct ConnAckPacket {
    pub protocol_version: ProtocolVersion,
    pub flags: ConnAckFlags,
    pub reason_code: ReasonCode,
    pub properties: Option<ConnAckProperties>,
}

const PACKET_TYPE: u8 = 0x02;

impl Encoder for ConnAckPacket {
    fn encode(&self, buffer: &mut bytes::BytesMut) {
        let mut remaining_len = 0;

        // Fixed header
        buffer.put_u8(PACKET_TYPE << 4);
        remaining_len += self.flags.encoded_size();

        if self.protocol_version.supports_properties() {
            // MQTT 5.0: reason code + properties
            remaining_len += self.reason_code.encoded_size();
            remaining_len +=
                VariableByteInteger(self.properties.encoded_size() as u32).encoded_size();
            remaining_len += self.properties.encoded_size();
        } else {
            // MQTT 3.x: return code only, no properties
            remaining_len += 1; // Return code byte
        }

        VariableByteInteger(remaining_len as u32).encode(buffer);

        // Variable header
        self.flags.encode(buffer);

        if self.protocol_version.supports_properties() {
            // MQTT 5.0
            self.reason_code.encode(buffer);
            VariableByteInteger(self.properties.encoded_size() as u32).encode(buffer);
            self.properties.encode(buffer);
        } else {
            // MQTT 3.x: encode return code (mapped from reason code)
            let return_code: ConnAckReturnCode = self.reason_code.into();
            return_code.encode(buffer);
        }
    }
}

impl Decoder for ConnAckPacket {
    /// Decodes a CONNACK packet assuming MQTT 5.0 format.
    /// For MQTT 3.x decoding, use `decode_v3` instead.
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        buffer.advance(1); // Packet type
        let _ = VariableByteInteger::decode(buffer)?; // Remaining length

        let flags = ConnAckFlags::decode(buffer)?;
        let reason_code = ReasonCode::decode(buffer)?;
        let properties = Some(ConnAckProperties::decode(buffer)?);

        Ok(ConnAckPacket {
            protocol_version: ProtocolVersion::V5,
            flags,
            reason_code,
            properties,
        })
    }
}

impl ConnAckPacket {
    /// Decodes a CONNACK packet in MQTT 3.x format.
    pub fn decode_v3<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        buffer.advance(1); // Packet type
        let _ = VariableByteInteger::decode(buffer)?; // Remaining length

        let flags = ConnAckFlags::decode(buffer)?;
        let return_code = buffer.get_u8();

        // Map MQTT 3.x return code to reason code
        let reason_code = match return_code {
            0x00 => ReasonCode::Success,
            0x01 => ReasonCode::UnsupportedProtocolVersion,
            0x02 => ReasonCode::ClientIdentifierNotValid,
            0x03 => ReasonCode::ServerUnavailable,
            0x04 => ReasonCode::BadUserNameOrPassword,
            0x05 => ReasonCode::NotAuthorized,
            _ => ReasonCode::UnspecifiedError,
        };

        Ok(ConnAckPacket {
            protocol_version: ProtocolVersion::V3_1_1, // Default to 3.1.1 for v3 decoding
            flags,
            reason_code,
            properties: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::connack::*;
    use bytes::{Bytes, BytesMut};

    #[test]
    fn test_connack_packet_encode_decode() {
        let expected = vec![
            0x20, 0x45, 0x0, 0x0, 0x42, 0x25, 0x1, 0x27, 0x0, 0x10, 0x0, 0x0, 0x12, 0x0, 0x2f,
            0x4d, 0x7a, 0x41, 0x77, 0x4e, 0x7a, 0x45, 0x7a, 0x4e, 0x54, 0x55, 0x32, 0x4d, 0x6a,
            0x4d, 0x7a, 0x4d, 0x6a, 0x4d, 0x34, 0x4f, 0x44, 0x45, 0x32, 0x4d, 0x7a, 0x41, 0x77,
            0x4f, 0x54, 0x63, 0x7a, 0x4e, 0x54, 0x49, 0x34, 0x4e, 0x54, 0x41, 0x35, 0x4f, 0x54,
            0x63, 0x79, 0x4e, 0x44, 0x49, 0x22, 0xff, 0xff, 0x28, 0x1, 0x29, 0x1, 0x2a, 0x1,
        ];

        let flags = ConnAckFlags {
            session_present: false,
        };

        let reason_code = ReasonCode::Success;
        let properties = ConnAckProperties {
            assigned_client_id: AssignedClientIdentifier::new(String::from(
                "MzAwNzEzNTU2MjMzMjM4ODE2MzAwOTczNTI4NTA5OTcyNDI",
            ))
            .into(),
            maximum_packet_size: MaximumPacketSize::new(1048576).into(),
            retain_available: RetainAvailable::new(true).into(),
            shared_subscription_available: SharedSubscriptionAvailable::new(true).into(),
            subscription_identifier_available: SubscriptionIdentifierAvailable::new(true).into(),
            topic_alias_max: TopicAliasMaximum::new(65535).into(),
            wildcard_subscription_available: WildcardSubscriptionAvailable::new(true).into(),
            ..Default::default()
        };

        let packet = ConnAckPacket {
            protocol_version: ProtocolVersion::V5,
            flags,
            reason_code,
            properties: properties.into(),
        };

        let mut encoded = BytesMut::new();
        packet.encode(&mut encoded);

        assert_eq!(encoded, expected);

        let mut bytes = Bytes::from(expected);

        let new_packet = ConnAckPacket::decode(&mut bytes).expect("Unexpected error");

        assert_eq!(packet, new_packet);
    }

    #[test]
    fn test_connack_packet_v3_1_1_encode() {
        // MQTT 3.1.1 CONNACK: just flags (session_present) + return code
        // No properties, no reason codes - simpler format
        let expected = vec![
            0x20, // Packet type: CONNACK
            0x02, // Remaining length: 2
            0x00, // Flags: session_present = false
            0x00, // Return code: 0 = Connection Accepted
        ];

        let packet = ConnAckPacket {
            protocol_version: ProtocolVersion::V3_1_1,
            flags: ConnAckFlags {
                session_present: false,
            },
            reason_code: ReasonCode::Success,
            properties: None,
        };

        let mut encoded = BytesMut::new();
        packet.encode(&mut encoded);

        assert_eq!(encoded.to_vec(), expected);
    }

    #[test]
    fn test_connack_packet_v3_1_1_not_authorized() {
        // MQTT 3.1.1 CONNACK with "not authorized" error
        let expected = vec![
            0x20, // Packet type: CONNACK
            0x02, // Remaining length: 2
            0x00, // Flags: session_present = false
            0x05, // Return code: 5 = Not Authorized
        ];

        let packet = ConnAckPacket {
            protocol_version: ProtocolVersion::V3_1_1,
            flags: ConnAckFlags {
                session_present: false,
            },
            reason_code: ReasonCode::NotAuthorized,
            properties: None,
        };

        let mut encoded = BytesMut::new();
        packet.encode(&mut encoded);

        assert_eq!(encoded.to_vec(), expected);
    }
}
