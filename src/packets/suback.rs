use bytes::Buf;

use crate::control_packet::{ControlPacket, ControlPacketType};
use crate::endec::{Decoder, Encoder, VariableByteInteger};
use crate::properties::Property;
use crate::properties::ReasonString;
use crate::properties::UserProperty;
use crate::reason::ReasonCode;

#[derive(Default, Debug, PartialEq)]
pub struct SubAckProperties {
    reason_string: Option<ReasonString>,
    user_property: Option<Vec<UserProperty>>,
}

impl Encoder for SubAckProperties {
    fn encode(&self, buffer: &mut bytes::BytesMut) {
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
    type Context = ();

    fn decode<T: Buf>(
        buffer: &mut T,
        _context: Option<&Self::Context>,
    ) -> Result<Option<Self>, ReasonCode> {
        let len = VariableByteInteger::decode(buffer, None)?.unwrap();
        if len.0 == 0 {
            return Ok(None);
        } else if (buffer.remaining() as u32) < len.0 {
            return Err(ReasonCode::MalformedPacket);
        }

        let mut encoded_properties = buffer.take(len.0 as usize);
        let mut suback_properties = SubAckProperties::default();

        loop {
            let p = Property::decode(&mut encoded_properties, None)?.unwrap();

            match p {
                Property::ReasonString => {
                    suback_properties.reason_string =
                        ReasonString::decode(&mut encoded_properties, None)?
                }

                Property::UserProperty => {
                    let user_property =
                        UserProperty::decode(&mut encoded_properties, None)?.unwrap();

                    if let Some(v) = &mut suback_properties.user_property {
                        v.push(user_property);
                    } else {
                        let v = vec![user_property];
                        suback_properties.user_property = Some(v);
                    }
                }

                _ => return Err(ReasonCode::MalformedPacket),
            }

            if !encoded_properties.has_remaining() {
                break;
            }
        }

        Ok(Some(suback_properties))
    }
}

#[derive(Debug, PartialEq)]
pub struct SubAckPayload {
    reason_code: ReasonCode,
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
    type Context = ();

    fn decode<T: Buf>(
        buffer: &mut T,
        _context: Option<&Self::Context>,
    ) -> Result<Option<Self>, ReasonCode> {
        let reason_code = ReasonCode::decode(buffer, Some(&ControlPacketType::SubAck))?.unwrap();

        Ok(Some(SubAckPayload { reason_code }))
    }
}

#[derive(Debug, PartialEq)]
pub struct SubAckPacket {
    packet_id: u16,
    properties: Option<SubAckProperties>,
    payload: Vec<SubAckPayload>,
}

impl Encoder for SubAckPacket {
    fn encode(&self, buffer: &mut bytes::BytesMut) {
        let fixed_header: u8;
        let mut remaining_len = 0;

        // Fixed header
        fixed_header = (Self::PACKET_TYPE as u8) << 4;
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

impl Decoder for SubAckPacket {
    type Context = ();

    fn decode<T: Buf>(
        buffer: &mut T,
        _context: Option<&Self::Context>,
    ) -> Result<Option<Self>, ReasonCode> {
        buffer.advance(1); // Packet type
        let _ = VariableByteInteger::decode(buffer, None)?; //Remaining length

        let packet_id = u16::decode(buffer, None)?.unwrap();
        let properties = SubAckProperties::decode(buffer, None)?;

        if !buffer.has_remaining() {
            return Err(ReasonCode::ProtocolError);
        }

        let mut payload = Vec::new();

        while buffer.has_remaining() {
            payload.push(SubAckPayload::decode(buffer, None)?.unwrap());
        }

        Ok(Some(SubAckPacket {
            packet_id,
            properties,
            payload,
        }))
    }
}

impl ControlPacket for SubAckPacket {
    const PACKET_TYPE: ControlPacketType = ControlPacketType::SubAck;
}

#[cfg(test)]
mod tests {
    use bytes::{Bytes, BytesMut};

    use crate::packets::suback::*;

    #[test]
    fn test_suback_packet_encode_decode() {
        let expected = vec![0x90, 0x04, 0x00, 0x01, 0x00, 0x01];

        let packet = SubAckPacket {
            packet_id: 1,
            properties: None,
            payload: vec![SubAckPayload {
                reason_code: ReasonCode::GrantedQoS1,
            }],
        };

        let mut encoded = BytesMut::new();
        packet.encode(&mut encoded);

        assert_eq!(encoded.to_vec(), expected);

        let mut bytes = Bytes::from(expected);

        let new_packet = SubAckPacket::decode(&mut bytes, None)
            .expect("Unexpected error")
            .unwrap();
        assert_eq!(packet, new_packet);
    }
}
