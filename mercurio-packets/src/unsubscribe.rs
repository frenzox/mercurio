#[cfg(not(feature = "std"))]
use alloc::{string::String, vec, vec::Vec};

use bytes::Buf;

use mercurio_core::{
    codec::{Decoder, Encoder, VariableByteInteger},
    error::Error,
    properties::*,
    reason::ReasonCode,
};

#[derive(Default, Debug, PartialEq, Eq)]
pub struct UnsubscribeProperties {
    user_property: Option<Vec<UserProperty>>,
}

impl Encoder for UnsubscribeProperties {
    fn encode(&self, buffer: &mut bytes::BytesMut) {
        self.user_property.encode(buffer);
    }

    fn encoded_size(&self) -> usize {
        let mut len = 0;

        len += self.user_property.encoded_size();

        len
    }
}

impl Decoder for UnsubscribeProperties {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        use Property::*;

        let len = VariableByteInteger::decode(buffer)?;
        let mut properties = UnsubscribeProperties::default();

        if len.0 == 0 {
            return Ok(properties);
        } else if (buffer.remaining() as u32) < len.0 {
            return Err(Error::PacketIncomplete);
        }

        let mut encoded_properties = buffer.take(len.0 as usize);

        while encoded_properties.has_remaining() {
            match Property::decode(&mut encoded_properties)? {
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
pub struct UnsubscribePayload {
    pub topic_filter: String,
}

impl Encoder for UnsubscribePayload {
    fn encode(&self, buffer: &mut bytes::BytesMut) {
        self.topic_filter.encode(buffer);
    }

    fn encoded_size(&self) -> usize {
        let mut len = 0;

        len += self.topic_filter.encoded_size();

        len
    }
}

impl Decoder for UnsubscribePayload {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        let topic_filter = String::decode(buffer)?;

        Ok(UnsubscribePayload { topic_filter })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct UnsubscribePacket {
    pub packet_id: u16,
    pub properties: Option<UnsubscribeProperties>,
    pub payload: Vec<UnsubscribePayload>,
}

const PACKET_TYPE: u8 = 0x0a;

impl Encoder for UnsubscribePacket {
    fn encode(&self, buffer: &mut bytes::BytesMut) {
        let mut remaining_len = 0;

        // Fixed header
        let mut fixed_header: u8 = PACKET_TYPE << 4;
        fixed_header |= 0b0000_0010;
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

impl Decoder for UnsubscribePacket {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        buffer.advance(1); // Packet type
        let remaining_len = VariableByteInteger::decode(buffer)?.0 as usize; //Remaining length
        let buffer_len = buffer.remaining();

        let packet_id = u16::decode(buffer)?;
        let properties = Some(UnsubscribeProperties::decode(buffer)?);

        if !buffer.has_remaining() {
            return Err(ReasonCode::ProtocolError.into());
        }

        let next_packet = buffer_len - remaining_len;
        let mut payload = Vec::new();

        while buffer.remaining() > next_packet {
            payload.push(UnsubscribePayload::decode(buffer)?);
        }

        Ok(UnsubscribePacket {
            packet_id,
            properties,
            payload,
        })
    }
}

#[cfg(test)]
mod tests {
    use bytes::{Bytes, BytesMut};

    use crate::unsubscribe::*;

    #[test]
    fn test_unsubscribe_packet_encode_decode() {
        let expected = vec![
            0xa2, 0x0f, 0x00, 0x01, 0x00, 0x00, 0x0a, 0x74, 0x65, 0x73, 0x74, 0x5f, 0x74, 0x6f,
            0x70, 0x69, 0x63,
        ];

        let packet = UnsubscribePacket {
            packet_id: 1,
            properties: UnsubscribeProperties::default().into(),
            payload: vec![UnsubscribePayload {
                topic_filter: "test_topic".to_string(),
            }],
        };

        let mut encoded = BytesMut::new();
        packet.encode(&mut encoded);

        assert_eq!(encoded.to_vec(), expected);

        let mut bytes = Bytes::from(expected);

        let new_packet = UnsubscribePacket::decode(&mut bytes).expect("Unexpected error");
        assert_eq!(packet, new_packet);
    }
}
