use bytes::Buf;

use crate::control_packet::{ControlPacket, ControlPacketType};
use crate::endec::{Decoder, Encoder, VariableByteInteger};
use crate::properties::Property;
use crate::properties::UserProperty;
use crate::reason::ReasonCode;

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
    fn decode<T: Buf>(buffer: &mut T) -> Result<Option<Self>, ReasonCode> {
        let len = VariableByteInteger::decode(buffer)?.unwrap();
        if len.0 == 0 {
            return Ok(None);
        } else if (buffer.remaining() as u32) < len.0 {
            return Err(ReasonCode::MalformedPacket);
        }

        let mut encoded_properties = buffer.take(len.0 as usize);
        let mut unsubscribe_properties = UnsubscribeProperties::default();

        loop {
            let p = Property::decode(&mut encoded_properties)?.unwrap();

            match p {
                Property::UserProperty => {
                    let user_property = UserProperty::decode(&mut encoded_properties)?.unwrap();

                    if let Some(v) = &mut unsubscribe_properties.user_property {
                        v.push(user_property);
                    } else {
                        let v = vec![user_property];
                        unsubscribe_properties.user_property = Some(v);
                    }
                }

                _ => return Err(ReasonCode::MalformedPacket),
            }

            if !encoded_properties.has_remaining() {
                break;
            }
        }

        Ok(Some(unsubscribe_properties))
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
    fn decode<T: Buf>(buffer: &mut T) -> Result<Option<Self>, ReasonCode> {
        let topic_filter = String::decode(buffer)?.unwrap();

        Ok(Some(UnsubscribePayload { topic_filter }))
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
        let mut fixed_header: u8;
        let mut remaining_len = 0;

        // Fixed header
        fixed_header = (Self::PACKET_TYPE as u8) << 4;
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
    fn decode<T: Buf>(buffer: &mut T) -> Result<Option<Self>, ReasonCode> {
        buffer.advance(1); // Packet type
        let _ = VariableByteInteger::decode(buffer)?; //Remaining length

        let packet_id = u16::decode(buffer)?.unwrap();
        let properties = UnsubscribeProperties::decode(buffer)?;

        if !buffer.has_remaining() {
            return Err(ReasonCode::ProtocolError);
        }

        let mut payload = Vec::new();

        while buffer.has_remaining() {
            payload.push(UnsubscribePayload::decode(buffer)?.unwrap());
        }

        Ok(Some(UnsubscribePacket {
            packet_id,
            properties,
            payload,
        }))
    }
}

impl ControlPacket for UnsubscribePacket {
    const PACKET_TYPE: ControlPacketType = ControlPacketType::Unsubscribe;
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

        // let properties = UnsubscribeProperties {
        //     user_property: vec![UserProperty::new("key".to_string(), "value".to_string())].into(),
        // };

        let packet = UnsubscribePacket {
            packet_id: 1,
            properties: None, //Some(properties),
            payload: vec![UnsubscribePayload {
                topic_filter: "test_topic".to_string(),
            }],
        };

        let mut encoded = BytesMut::new();
        packet.encode(&mut encoded);

        assert_eq!(encoded.to_vec(), expected);

        let mut bytes = Bytes::from(expected);

        let new_packet = UnsubscribePacket::decode(&mut bytes)
            .expect("Unexpected error")
            .unwrap();
        assert_eq!(packet, new_packet);
    }
}
