//! UNSUBACK, the server's answer to UNSUBSCRIBE (section 3.11).

#[cfg(not(feature = "std"))]
use alloc::vec::Vec;

use bytes::{Buf, BytesMut};

use mercurio_core::{
    codec::{Decoder, Encoder, VariableByteInteger},
    properties::*,
    protocol::ProtocolVersion,
    reason::ReasonCode,
};

use crate::PacketType;

#[derive(Default, Debug, PartialEq, Eq)]
pub struct UnsubAckProperties {
    pub reason_string: Option<ReasonString>,
    pub user_property: Option<Vec<UserProperty>>,
}

impl Encoder for UnsubAckProperties {
    fn encode(&self, buffer: &mut BytesMut) {
        self.reason_string.encode(buffer);
        self.user_property.encode(buffer);
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

#[derive(Debug, PartialEq, Eq)]
pub struct UnsubAckPayload {
    /// Result for the topic filter at the same position in the UNSUBSCRIBE.
    pub reason_code: ReasonCode,
}

impl Encoder for UnsubAckPayload {
    fn encode(&self, buffer: &mut bytes::BytesMut) {
        self.reason_code.encode(buffer);
    }

    fn encoded_size(&self) -> usize {
        let mut len = 0;

        len += self.reason_code.encoded_size();

        len
    }
}

impl Decoder for UnsubAckPayload {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        let reason_code = ReasonCode::decode(buffer)?;

        Ok(UnsubAckPayload { reason_code })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct UnsubAckPacket {
    /// Selects the wire format on encode. `decode` sets `V5`, and
    /// `decode_v3` sets `V3_1_1`.
    /// [`ControlPacket::parse_with_version`](crate::ControlPacket::parse_with_version)
    /// sets the version it is given.
    pub protocol_version: ProtocolVersion,
    pub packet_id: u16,
    pub properties: Option<UnsubAckProperties>,
    /// MQTT 5 only. A 3.x UNSUBACK has no payload, so this is empty after
    /// `decode_v3` and not encoded.
    pub payload: Vec<UnsubAckPayload>,
}

const PACKET_TYPE: u8 = 0x0b;

impl Encoder for UnsubAckPacket {
    fn encode(&self, buffer: &mut bytes::BytesMut) {
        // Fixed header
        let fixed_header: u8 = PACKET_TYPE << 4;
        fixed_header.encode(buffer);

        VariableByteInteger(self.remaining_len() as u32).encode(buffer);

        self.packet_id.encode(buffer);

        // Properties and payload reason codes only for MQTT 5.0
        if self.protocol_version.supports_properties() {
            VariableByteInteger(self.properties.encoded_size() as u32).encode(buffer);
            self.properties.encode(buffer);
            self.payload.encode(buffer);
        }
    }

    fn encoded_size(&self) -> usize {
        crate::packet_size(self.remaining_len())
    }
}

impl UnsubAckPacket {
    fn remaining_len(&self) -> usize {
        // MQTT 3.x UNSUBACK is just the packet id
        let mut len = self.packet_id.encoded_size();
        if self.protocol_version.supports_properties() {
            len += crate::property_list_size(&self.properties) + self.payload.encoded_size();
        }
        len
    }
}

impl Decoder for UnsubAckPacket {
    /// Decodes an UNSUBACK packet assuming MQTT 5.0 format.
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        crate::decode_packet(buffer, PacketType::UnsubAck, |_, buffer| {
            let packet_id = crate::decode_packet_id(buffer)?;
            let properties = crate::present(UnsubAckProperties::decode(buffer)?);

            if !buffer.has_remaining() {
                return Err(ReasonCode::ProtocolError.into());
            }

            let mut payload = Vec::new();
            while buffer.has_remaining() {
                payload.push(UnsubAckPayload::decode(buffer)?);
            }

            Ok(UnsubAckPacket {
                protocol_version: ProtocolVersion::V5,
                packet_id,
                properties,
                payload,
            })
        })
    }
}

impl UnsubAckPacket {
    /// Decodes an UNSUBACK packet in MQTT 3.x format.
    /// MQTT 3.x UNSUBACK: just packet_id (no properties, no payload)
    pub fn decode_v3<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        crate::decode_packet(buffer, PacketType::UnsubAck, |_, buffer| {
            let packet_id = crate::decode_packet_id(buffer)?;

            Ok(UnsubAckPacket {
                protocol_version: ProtocolVersion::V3_1_1,
                packet_id,
                properties: None,
                payload: Vec::new(),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use bytes::{Bytes, BytesMut};

    use crate::unsuback::*;

    #[test]
    fn test_unsuback_packet_encode_decode() {
        let expected = vec![0xb0, 0x04, 0x00, 0x01, 0x00, 0x01];

        let packet = UnsubAckPacket {
            protocol_version: ProtocolVersion::V5,
            packet_id: 1,
            properties: None,
            payload: vec![UnsubAckPayload {
                reason_code: ReasonCode::GrantedQoS1,
            }],
        };

        let mut encoded = BytesMut::new();
        packet.encode(&mut encoded);

        assert_eq!(encoded.to_vec(), expected);

        let mut bytes = Bytes::from(expected);

        let new_packet = UnsubAckPacket::decode(&mut bytes).expect("Unexpected error");
        assert_eq!(packet, new_packet);
    }
}
