//! DISCONNECT, the end of a connection, sent by the client or, in MQTT 5,
//! the server, with a reason (section 3.14).

#[cfg(not(feature = "std"))]
use alloc::vec::Vec;

use bytes::{Buf, BufMut, BytesMut};

use mercurio_core::{
    codec::{Decoder, Encoder, VariableByteInteger},
    properties::*,
    reason::ReasonCode,
};

#[derive(Default, PartialEq, Eq, Debug)]
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

#[derive(PartialEq, Eq, Debug)]
pub struct DisconnectPacket {
    /// Decodes as `NormalDisconnection` when left out or 0x00. Not encoded in
    /// MQTT 3.x, see [`Self::encode_v3`].
    pub reason: ReasonCode,
    pub properties: Option<DisconnectProperties>,
}

const PACKET_TYPE: u8 = 0x0e;

impl Encoder for DisconnectPacket {
    fn encode(&self, buffer: &mut BytesMut) {
        let mut remaining_len = 0;

        let properties_len = VariableByteInteger(self.properties.encoded_size() as u32);

        buffer.put_u8(PACKET_TYPE << 4);
        remaining_len += self.reason.encoded_size();
        if self.properties.is_some() {
            remaining_len += properties_len.encoded_size() + self.properties.encoded_size();
        }
        VariableByteInteger(remaining_len as u32).encode(buffer);

        self.reason.encode(buffer);
        if self.properties.is_some() {
            properties_len.encode(buffer);
            self.properties.encode(buffer);
        }
    }
}

impl DisconnectPacket {
    /// Encodes a DISCONNECT packet in MQTT 3.x format, which has no
    /// variable header.
    pub fn encode_v3(&self, buffer: &mut BytesMut) {
        buffer.put_u8(PACKET_TYPE << 4);
        buffer.put_u8(0);
    }
}

impl Decoder for DisconnectPacket {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        let reserved = buffer.get_u8() & 0xF;

        if reserved != 0 {
            return Err(ReasonCode::MalformedPacket.into());
        }

        let min_len_reason = 1;
        let min_len_properties = 2;

        let remaining_len = VariableByteInteger::decode(buffer)?; //Remaining length
                                                                  // 0x00 is "Normal disconnection" in DISCONNECT, whether it is sent
                                                                  // or left out
        let reason = match remaining_len.0.cmp(&min_len_reason) {
            core::cmp::Ordering::Less => ReasonCode::NormalDisconnection,
            _ => match ReasonCode::decode(buffer)? {
                ReasonCode::Success => ReasonCode::NormalDisconnection,
                reason => reason,
            },
        };

        let properties = match remaining_len.0.cmp(&min_len_properties) {
            core::cmp::Ordering::Less => None,
            _ => crate::present(DisconnectProperties::decode(buffer)?),
        };

        Ok(DisconnectPacket { reason, properties })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reason_zero_reads_the_same_sent_or_left_out() {
        use crate::ControlPacket;
        use mercurio_core::protocol::ProtocolVersion;

        for encoded in [&[0xe0, 0x00][..], &[0xe0, 0x01, 0x00]] {
            let packet = ControlPacket::parse_with_version(
                &mut BytesMut::from(encoded),
                ProtocolVersion::V5,
            )
            .unwrap();
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
        packet.encode(&mut buffer);
        assert_eq!(DisconnectPacket::decode(&mut buffer).unwrap(), packet);
        assert!(buffer.is_empty());
    }

    #[test]
    fn test_encode_v3_has_no_variable_header() {
        let packet = DisconnectPacket {
            reason: ReasonCode::NormalDisconnection,
            properties: None,
        };

        let mut buffer = BytesMut::new();
        packet.encode_v3(&mut buffer);
        assert_eq!(&buffer[..], &[0xe0, 0x00]);
    }
}
