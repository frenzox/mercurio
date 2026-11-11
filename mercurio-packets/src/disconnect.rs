//! DISCONNECT, the end of a connection, sent by the client or, in MQTT 5,
//! the server, with a reason (section 3.14).

#[cfg(not(feature = "std"))]
use alloc::vec::Vec;

use bytes::{Buf, BufMut, BytesMut};

use mercurio_core::{
    codec::{Decoder, Encoder, VariableByteInteger},
    properties::*,
    protocol::ProtocolVersion,
    reason::ReasonCode,
};

use crate::{Packet, PacketType};

#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct DisconnectProperties {
    /// Replaces the session expiry from CONNECT. Sent by the client only
    /// [MQTT-3.14.2-2].
    pub session_expiry_interval: Option<SessionExpiryInterval>,
    pub reason_string: Option<ReasonString>,
    pub user_property: Option<Vec<UserProperty>>,
    /// Another server the client may use (section 4.11).
    pub server_reference: Option<ServerReference>,
}

impl Encoder for DisconnectProperties {
    fn encode(&self, buffer: &mut BytesMut) {
        self.session_expiry_interval.encode(buffer);
        self.reason_string.encode(buffer);
        self.user_property.encode(buffer);
        self.server_reference.encode(buffer);
    }

    fn encoded_size(&self) -> usize {
        let mut len = 0;

        len += self.session_expiry_interval.encoded_size();
        len += self.reason_string.encoded_size();
        len += self.user_property.encoded_size();
        len += self.server_reference.encoded_size();

        len
    }
}

impl Decoder for DisconnectProperties {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        use Property::*;

        let mut properties = DisconnectProperties::default();

        decode_property_list(buffer, |property| match property {
            SessionExpiryInterval(v) => set_once(&mut properties.session_expiry_interval, v),
            ReasonString(v) => set_once(&mut properties.reason_string, v),
            UserProperty(v) => {
                push_user_property(&mut properties.user_property, v);
                Ok(())
            }
            ServerReference(v) => set_once(&mut properties.server_reference, v),
            _ => Err(ReasonCode::MalformedPacket.into()),
        })?;

        Ok(properties)
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DisconnectPacket {
    /// Decodes as `NormalDisconnection` when left out or 0x00. Not encoded in
    /// MQTT 3.x.
    pub reason: ReasonCode,
    pub properties: Option<DisconnectProperties>,
}

const PACKET_TYPE: u8 = 0x0e;

impl Packet for DisconnectPacket {
    /// MQTT 3.x has no variable header: the reason and properties are left
    /// out.
    fn encode(&self, buffer: &mut BytesMut, version: ProtocolVersion) {
        buffer.put_u8(PACKET_TYPE << 4);
        VariableByteInteger(self.remaining_len(version) as u32).encode(buffer);

        if version.supports_reason_codes() {
            self.reason.encode(buffer);
            if self.properties.is_some() {
                VariableByteInteger(self.properties.encoded_size() as u32).encode(buffer);
                self.properties.encode(buffer);
            }
        }
    }

    fn encoded_size(&self, version: ProtocolVersion) -> usize {
        crate::packet_size(self.remaining_len(version))
    }

    fn decode<T: Buf>(buffer: &mut T, version: ProtocolVersion) -> crate::Result<Self> {
        crate::decode_packet(buffer, PacketType::Disconnect, version, |_, buffer| {
            // A reason left out is a normal disconnection, and MQTT 3.x
            // never has one
            let reason = if version.supports_reason_codes() && buffer.has_remaining() {
                crate::decode_reason(buffer, PacketType::Disconnect)?
            } else {
                ReasonCode::NormalDisconnection
            };

            let properties = if version.supports_properties() && buffer.has_remaining() {
                crate::present(DisconnectProperties::decode(buffer)?)
            } else {
                None
            };

            Ok(DisconnectPacket { reason, properties })
        })
    }
}

impl DisconnectPacket {
    fn remaining_len(&self, version: ProtocolVersion) -> usize {
        if !version.supports_reason_codes() {
            return 0;
        }
        let mut len = self.reason.encoded_size();
        if self.properties.is_some() {
            len += crate::property_list_size(&self.properties);
        }
        len
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reason_zero_reads_the_same_sent_or_left_out() {
        use crate::ControlPacket;

        for encoded in [&[0xe0, 0x00][..], &[0xe0, 0x01, 0x00]] {
            let packet =
                ControlPacket::parse(&mut BytesMut::from(encoded), ProtocolVersion::V5).unwrap();
            assert_eq!(
                packet,
                ControlPacket::Disconnect(DisconnectPacket {
                    reason: ReasonCode::NormalDisconnection,
                    properties: None,
                })
            );
        }
    }

    #[test]
    fn test_round_trip_with_properties() {
        let packet = DisconnectPacket {
            reason: ReasonCode::DisconnectWithWillMessage,
            properties: Some(DisconnectProperties {
                session_expiry_interval: Some(SessionExpiryInterval { value: 30 }),
                ..Default::default()
            }),
        };

        let mut buffer = BytesMut::new();
        packet.encode(&mut buffer, ProtocolVersion::V5);
        assert_eq!(
            DisconnectPacket::decode(&mut buffer, ProtocolVersion::V5).unwrap(),
            packet
        );
        assert!(buffer.is_empty());
    }

    #[test]
    fn test_encode_v3_has_no_variable_header() {
        let packet = DisconnectPacket {
            reason: ReasonCode::ServerShuttingDown,
            properties: None,
        };

        let mut buffer = BytesMut::new();
        packet.encode(&mut buffer, ProtocolVersion::V3_1_1);
        assert_eq!(&buffer[..], &[0xe0, 0x00]);
    }
}
