use bytes::{Buf, BytesMut};

use crate::codec::{Decoder, Encoder, VariableByteInteger};
use crate::error::Error;
use crate::properties::*;
use crate::reason::ReasonCode;
use crate::result::Result;

use super::control_packet_type::ControlPacketType;

#[derive(Default, Debug, PartialEq)]
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
    type Context = ();

    fn decode<T: Buf>(buffer: &mut T, _context: Option<&Self::Context>) -> Result<Self> {
        use Property::*;

        let len = VariableByteInteger::decode(buffer, None)?;
        let mut properties = SubAckProperties::default();

        if len.0 == 0 {
            return Ok(properties);
        } else if (buffer.remaining() as u32) < len.0 {
            return Err(Error::PacketIncomplete);
        }

        let mut encoded_properties = buffer.take(len.0 as usize);

        while encoded_properties.has_remaining() {
            match Property::decode(&mut encoded_properties, None)? {
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

    fn decode<T: Buf>(buffer: &mut T, _context: Option<&Self::Context>) -> Result<Self> {
        let reason_code = ReasonCode::decode(buffer, None)?;

        Ok(SubAckPayload { reason_code })
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
        let mut remaining_len = 0;

        // Fixed header
        let fixed_header: u8 = Self::PACKET_TYPE << 4;
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

    fn decode<T: Buf>(buffer: &mut T, _context: Option<&Self::Context>) -> Result<Self> {
        buffer.advance(1); // Packet type
        let remaining_len = VariableByteInteger::decode(buffer, None)?.0 as usize; //Remaining length
        let buffer_len = buffer.remaining();

        let packet_id = u16::decode(buffer, None)?;
        let properties = Some(SubAckProperties::decode(buffer, None)?);

        if !buffer.has_remaining() {
            return Err(ReasonCode::ProtocolError.into());
        }

        let next_packet = buffer_len - remaining_len;
        let mut payload = Vec::new();

        while buffer.remaining() > next_packet {
            payload.push(SubAckPayload::decode(buffer, None)?);
        }

        Ok(SubAckPacket {
            packet_id,
            properties,
            payload,
        })
    }
}

impl ControlPacketType for SubAckPacket {
    const PACKET_TYPE: u8 = 0x09;
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
            properties: SubAckProperties::default().into(),
            payload: vec![SubAckPayload {
                reason_code: ReasonCode::GrantedQoS1,
            }],
        };

        let mut encoded = BytesMut::new();
        packet.encode(&mut encoded);

        assert_eq!(encoded.to_vec(), expected);

        let mut bytes = Bytes::from(expected);

        let new_packet = SubAckPacket::decode(&mut bytes, None).expect("Unexpected error");
        assert_eq!(packet, new_packet);
    }
}
