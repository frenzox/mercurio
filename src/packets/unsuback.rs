use bytes::{Buf, BytesMut};

use crate::{
    codec::{Decoder, Encoder, VariableByteInteger},
    error::Error,
    properties::*,
    reason::ReasonCode,
};

#[derive(Default, Debug, PartialEq)]
pub struct UnsubAckProperties {
    reason_string: Option<ReasonString>,
    user_property: Option<Vec<UserProperty>>,
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

        let len = VariableByteInteger::decode(buffer)?;
        let mut properties = UnsubAckProperties::default();

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

#[derive(Debug, PartialEq)]
pub struct UnsubAckPayload {
    reason_code: ReasonCode,
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

#[derive(Debug, PartialEq)]
pub struct UnsubAckPacket {
    packet_id: u16,
    properties: Option<UnsubAckProperties>,
    payload: Vec<UnsubAckPayload>,
}

const PACKET_TYPE: u8 = 0x0b;

impl Encoder for UnsubAckPacket {
    fn encode(&self, buffer: &mut bytes::BytesMut) {
        let mut remaining_len = 0;

        // Fixed header
        let fixed_header: u8 = PACKET_TYPE << 4;
        fixed_header.encode(buffer);

        remaining_len += self.packet_id.encoded_size();
        remaining_len += VariableByteInteger(self.properties.encoded_size() as u32).encoded_size();
        remaining_len += self.properties.encoded_size();
        remaining_len += self.payload.encoded_size();

        VariableByteInteger(remaining_len as u32).encode(buffer);

        self.packet_id.encode(buffer);
        VariableByteInteger(self.properties.encoded_size() as u32).encode(buffer);
        self.properties.encode(buffer);
        self.payload.encode(buffer);
    }
}

impl Decoder for UnsubAckPacket {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        buffer.advance(1); // Packet type
        let remaining_len = VariableByteInteger::decode(buffer)?.0 as usize; //Remaining length
        let buffer_len = buffer.remaining();

        let packet_id = u16::decode(buffer)?;
        let properties = Some(UnsubAckProperties::decode(buffer)?);

        if !buffer.has_remaining() {
            return Err(ReasonCode::ProtocolError.into());
        }

        let next_packet = buffer_len - remaining_len;
        let mut payload = Vec::new();

        while buffer.remaining() > next_packet {
            payload.push(UnsubAckPayload::decode(buffer)?);
        }

        Ok(UnsubAckPacket {
            packet_id,
            properties,
            payload,
        })
    }
}

#[cfg(test)]
mod tests {
    use bytes::{Bytes, BytesMut};

    use crate::packets::unsuback::*;

    #[test]
    fn test_unsuback_packet_encode_decode() {
        let expected = vec![0xb0, 0x04, 0x00, 0x01, 0x00, 0x01];

        let packet = UnsubAckPacket {
            packet_id: 1,
            properties: UnsubAckProperties::default().into(),
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
