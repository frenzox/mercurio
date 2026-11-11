//! UNSUBACK, the server's answer to UNSUBSCRIBE (section 3.11).

#[cfg(not(feature = "std"))]
use alloc::vec::Vec;

use bytes::{Buf, BytesMut};

use mercurio_core::{
    codec::{Decoder, Encoder},
    error::EncodeError,
    properties::*,
    protocol::ProtocolVersion,
    reason::ReasonCode,
};

use crate::{Packet, PacketType};

#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct UnsubAckProperties {
    pub reason_string: Option<ReasonString>,
    pub user_property: Option<Vec<UserProperty>>,
}

impl Encoder for UnsubAckProperties {
    fn encode(&self, buffer: &mut BytesMut) -> Result<(), EncodeError> {
        self.reason_string.encode(buffer)?;
        self.user_property.encode(buffer)?;
        Ok(())
    }

    fn encoded_size(&self) -> usize {
        let mut len = 0;

        len += self.reason_string.encoded_size();
        len += self.user_property.encoded_size();

        len
    }
}

impl Decoder for UnsubAckProperties {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        use Property::*;

        let mut properties = UnsubAckProperties::default();

        decode_property_list(buffer, |property| match property {
            ReasonString(v) => set_once(&mut properties.reason_string, v),
            UserProperty(v) => {
                push_user_property(&mut properties.user_property, v);
                Ok(())
            }
            _ => Err(ReasonCode::MalformedPacket.into()),
        })?;

        Ok(properties)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnsubAckPayload {
    /// Result for the topic filter at the same position in the UNSUBSCRIBE.
    pub reason_code: ReasonCode,
}

impl Encoder for UnsubAckPayload {
    fn encode(&self, buffer: &mut bytes::BytesMut) -> Result<(), EncodeError> {
        crate::encode_reason(self.reason_code, PacketType::UnsubAck, buffer)
    }

    fn encoded_size(&self) -> usize {
        let mut len = 0;

        len += self.reason_code.encoded_size();

        len
    }
}

impl Decoder for UnsubAckPayload {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        let reason_code = crate::decode_reason(buffer, PacketType::UnsubAck)?;

        Ok(UnsubAckPayload { reason_code })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnsubAckPacket {
    pub packet_id: u16,
    /// MQTT 5 only.
    pub properties: Option<UnsubAckProperties>,
    /// MQTT 5 only. A 3.x UNSUBACK has no payload, so this is empty after
    /// decoding one and not encoded.
    pub payload: Vec<UnsubAckPayload>,
}

const PACKET_TYPE: u8 = 0x0b;

impl Packet for UnsubAckPacket {
    fn encode(
        &self,
        buffer: &mut bytes::BytesMut,
        version: ProtocolVersion,
    ) -> Result<(), EncodeError> {
        crate::write_packet(buffer, |buffer| {
            if version.supports_reason_codes() && self.payload.is_empty() {
                return Err(EncodeError::EmptyPayload(PacketType::UnsubAck));
            }
            // Fixed header
            let fixed_header: u8 = PACKET_TYPE << 4;
            fixed_header.encode(buffer)?;

            crate::encode_length(self.remaining_len(version), buffer)?;

            crate::encode_packet_id(self.packet_id, buffer)?;

            // Properties and payload reason codes only for MQTT 5.0
            if version.supports_properties() {
                crate::encode_length(self.properties.encoded_size(), buffer)?;
                self.properties.encode(buffer)?;
                self.payload.encode(buffer)?;
            }
            Ok(())
        })
    }

    fn encoded_size(&self, version: ProtocolVersion) -> usize {
        crate::packet_size(self.remaining_len(version))
    }

    fn decode<T: Buf>(buffer: &mut T, version: ProtocolVersion) -> crate::Result<Self> {
        crate::decode_packet(buffer, PacketType::UnsubAck, version, |_, buffer| {
            let packet_id = crate::decode_packet_id(buffer)?;

            // MQTT 3.x UNSUBACK: just packet_id (no properties, no payload)
            if !version.supports_properties() {
                return Ok(UnsubAckPacket {
                    packet_id,
                    properties: None,
                    payload: Vec::new(),
                });
            }

            let properties = crate::present(UnsubAckProperties::decode(buffer)?);

            if !buffer.has_remaining() {
                return Err(ReasonCode::ProtocolError.into());
            }

            let mut payload = Vec::new();
            while buffer.has_remaining() {
                payload.push(UnsubAckPayload::decode(buffer)?);
            }

            Ok(UnsubAckPacket {
                packet_id,
                properties,
                payload,
            })
        })
    }
}

impl UnsubAckPacket {
    fn remaining_len(&self, version: ProtocolVersion) -> usize {
        // MQTT 3.x UNSUBACK is just the packet id
        let mut len = self.packet_id.encoded_size();
        if version.supports_properties() {
            len += crate::property_list_size(&self.properties) + self.payload.encoded_size();
        }
        len
    }
}

#[cfg(test)]
mod tests {
    use bytes::{Bytes, BytesMut};

    use crate::unsuback::*;

    #[test]
    fn test_unsuback_packet_encode_decode() {
        let expected = vec![0xb0, 0x04, 0x00, 0x01, 0x00, 0x11];

        let packet = UnsubAckPacket {
            packet_id: 1,
            properties: None,
            payload: vec![UnsubAckPayload {
                reason_code: ReasonCode::NoSubscriptionExisted,
            }],
        };

        let mut encoded = BytesMut::new();
        packet.encode(&mut encoded, ProtocolVersion::V5).unwrap();

        assert_eq!(encoded.to_vec(), expected);

        let mut bytes = Bytes::from(expected);

        let new_packet =
            UnsubAckPacket::decode(&mut bytes, ProtocolVersion::V5).expect("Unexpected error");
        assert_eq!(packet, new_packet);
    }
}
