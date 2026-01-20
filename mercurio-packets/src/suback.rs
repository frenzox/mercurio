#[cfg(not(feature = "std"))]
use alloc::{vec, vec::Vec};

use bytes::{Buf, BytesMut};

use mercurio_core::{
    codec::{Decoder, Encoder, VariableByteInteger},
    error::Error,
    properties::*,
    protocol::ProtocolVersion,
    reason::ReasonCode,
};

#[derive(Default, Debug, PartialEq, Eq)]
pub struct SubAckProperties {
    reason_string: Option<ReasonString>,
    user_property: Option<Vec<UserProperty>>,
}

impl Encoder for SubAckProperties {
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

impl Decoder for SubAckProperties {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        use Property::*;

        let len = VariableByteInteger::decode(buffer)?;
        let mut properties = SubAckProperties::default();

        if len.0 == 0 {
            return Ok(properties);
        } else if (buffer.remaining() as u32) < len.0 {
            return Err(Error::PacketIncomplete);
        }

        let mut encoded_properties = buffer.take(len.0 as usize);

        while encoded_properties.has_remaining() {
            match Property::decode(&mut encoded_properties)? {
                ReasonString(v) => properties.reason_string = Some(v),
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

#[derive(Debug, PartialEq, Eq)]
pub struct SubAckPayload {
    pub reason_code: ReasonCode,
}

impl Encoder for SubAckPayload {
    fn encode(&self, buffer: &mut bytes::BytesMut) {
        self.reason_code.encode(buffer);
    }

    fn encoded_size(&self) -> usize {
        let mut len = 0;

        len += self.reason_code.encoded_size();

        len
    }
}

impl Decoder for SubAckPayload {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        let reason_code = ReasonCode::decode(buffer)?;

        Ok(SubAckPayload { reason_code })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct SubAckPacket {
    pub protocol_version: ProtocolVersion,
    pub packet_id: u16,
    pub properties: Option<SubAckProperties>,
    pub payload: Vec<SubAckPayload>,
}

const PACKET_TYPE: u8 = 0x09;

impl Encoder for SubAckPacket {
    fn encode(&self, buffer: &mut bytes::BytesMut) {
        let mut remaining_len = 0;

        // Fixed header
        let fixed_header: u8 = PACKET_TYPE << 4;
        fixed_header.encode(buffer);

        remaining_len += self.packet_id.encoded_size();

        // Properties only for MQTT 5.0
        if self.protocol_version.supports_properties() {
            remaining_len +=
                VariableByteInteger(self.properties.encoded_size() as u32).encoded_size();
            remaining_len += self.properties.encoded_size();
        }

        remaining_len += self.payload.encoded_size();

        VariableByteInteger(remaining_len as u32).encode(buffer);

        self.packet_id.encode(buffer);

        // Properties only for MQTT 5.0
        if self.protocol_version.supports_properties() {
            VariableByteInteger(self.properties.encoded_size() as u32).encode(buffer);
            self.properties.encode(buffer);
        }

        self.payload.encode(buffer);
    }
}

impl Decoder for SubAckPacket {
    /// Decodes a SUBACK packet assuming MQTT 5.0 format.
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        buffer.advance(1); // Packet type
        let remaining_len = VariableByteInteger::decode(buffer)?.0 as usize; // Remaining length
        let buffer_len = buffer.remaining();

        let packet_id = u16::decode(buffer)?;
        let properties = Some(SubAckProperties::decode(buffer)?);

        if !buffer.has_remaining() {
            return Err(ReasonCode::ProtocolError.into());
        }

        let next_packet = buffer_len - remaining_len;
        let mut payload = Vec::new();

        while buffer.remaining() > next_packet {
            payload.push(SubAckPayload::decode(buffer)?);
        }

        Ok(SubAckPacket {
            protocol_version: ProtocolVersion::V5,
            packet_id,
            properties,
            payload,
        })
    }
}

impl SubAckPacket {
    /// Decodes a SUBACK packet in MQTT 3.x format.
    /// MQTT 3.x SUBACK: packet_id + return codes (no properties)
    pub fn decode_v3<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        buffer.advance(1); // Packet type
        let remaining_len = VariableByteInteger::decode(buffer)?.0 as usize;
        let buffer_len = buffer.remaining();

        let packet_id = u16::decode(buffer)?;

        // No properties in MQTT 3.x
        // Return codes: 0x00=QoS0, 0x01=QoS1, 0x02=QoS2, 0x80=failure
        let next_packet = buffer_len - remaining_len;
        let mut payload = Vec::new();

        while buffer.remaining() > next_packet {
            let return_code = buffer.get_u8();
            let reason_code = match return_code {
                0x00 => ReasonCode::GrantedQoS0,
                0x01 => ReasonCode::GrantedQoS1,
                0x02 => ReasonCode::GrantedQoS2,
                0x80 => ReasonCode::UnspecifiedError,
                _ => ReasonCode::UnspecifiedError,
            };
            payload.push(SubAckPayload { reason_code });
        }

        Ok(SubAckPacket {
            protocol_version: ProtocolVersion::V3_1_1,
            packet_id,
            properties: None,
            payload,
        })
    }
}

#[cfg(test)]
mod tests {
    use bytes::{Bytes, BytesMut};

    use crate::suback::*;

    #[test]
    fn test_suback_packet_encode_decode() {
        let expected = vec![0x90, 0x04, 0x00, 0x01, 0x00, 0x01];

        let packet = SubAckPacket {
            protocol_version: ProtocolVersion::V5,
            packet_id: 1,
            properties: SubAckProperties::default().into(),
            payload: vec![SubAckPayload {
                reason_code: ReasonCode::GrantedQoS1,
            }],
        };

        let mut encoded = BytesMut::new();
        packet.encode(&mut encoded);

        assert_eq!(encoded.to_vec(), expected);

        let mut bytes = Bytes::from(expected);

        let new_packet = SubAckPacket::decode(&mut bytes).expect("Unexpected error");
        assert_eq!(packet, new_packet);
    }
}
