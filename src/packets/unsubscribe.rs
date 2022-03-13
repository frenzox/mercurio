use bytes::Buf;

use crate::codec::{Decoder, Encoder, VariableByteInteger};
use crate::error::Error;
use crate::properties::*;
use crate::reason::ReasonCode;
use crate::result::Result;

use super::control_packet_type::ControlPacketType;

#[derive(Default, Debug, PartialEq)]
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
    type Context = ();

    fn decode<T: Buf>(buffer: &mut T, _context: Option<&Self::Context>) -> Result<Self> {
        use Property::*;

        let len = VariableByteInteger::decode(buffer, None)?;
        let mut properties = UnsubscribeProperties::default();

        if len.0 == 0 {
            return Ok(properties);
        } else if (buffer.remaining() as u32) < len.0 {
            return Err(Error::PacketIncomplete);
        }

        let mut encoded_properties = buffer.take(len.0 as usize);

        while encoded_properties.has_remaining() {
            match Property::decode(&mut encoded_properties, None)? {
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
pub struct UnsubscribePayload {
    topic_filter: String,
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
    type Context = ();

    fn decode<T: Buf>(buffer: &mut T, _context: Option<&Self::Context>) -> Result<Self> {
        let topic_filter = String::decode(buffer, None)?;

        Ok(UnsubscribePayload { topic_filter })
    }
}

#[derive(Debug, PartialEq)]
pub struct UnsubscribePacket {
    packet_id: u16,
    properties: Option<UnsubscribeProperties>,
    payload: Vec<UnsubscribePayload>,
}

impl Encoder for UnsubscribePacket {
    fn encode(&self, buffer: &mut bytes::BytesMut) {
        let mut remaining_len = 0;

        // Fixed header
        let mut fixed_header: u8 = Self::PACKET_TYPE << 4;
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
    type Context = ();

    fn decode<T: Buf>(buffer: &mut T, _context: Option<&Self::Context>) -> Result<Self> {
        buffer.advance(1); // Packet type
        let _ = VariableByteInteger::decode(buffer, None)?; //Remaining length

        let packet_id = u16::decode(buffer, None)?;
        let properties = Some(UnsubscribeProperties::decode(buffer, None)?);

        if !buffer.has_remaining() {
            return Err(ReasonCode::ProtocolError.into());
        }

        let mut payload = Vec::new();

        while buffer.has_remaining() {
            payload.push(UnsubscribePayload::decode(buffer, None)?);
        }

        Ok(UnsubscribePacket {
            packet_id,
            properties,
            payload,
        })
    }
}

impl ControlPacketType for UnsubscribePacket {
    const PACKET_TYPE: u8 = 0x0a;
}

#[cfg(test)]
mod tests {
    use bytes::{Bytes, BytesMut};

    use crate::packets::unsubscribe::*;

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

        let new_packet = UnsubscribePacket::decode(&mut bytes, None).expect("Unexpected error");
        assert_eq!(packet, new_packet);
    }
}
