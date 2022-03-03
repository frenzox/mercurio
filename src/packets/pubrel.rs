use bytes::{Buf, BufMut, BytesMut};

use crate::control_packet::{ControlPacket, ControlPacketType};
use crate::endec::{Decoder, Encoder, VariableByteInteger};
use crate::properties::{Property, ReasonString, UserProperty};
use crate::reason::ReasonCode;

#[derive(Default, Debug, PartialEq)]
pub struct PubRelProperties {
    reason_str: Option<ReasonString>,
    user_property: Option<Vec<UserProperty>>,
}

impl Encoder for PubRelProperties {
    fn encode(&self, buffer: &mut BytesMut) {
        self.reason_str.encode(buffer);
        self.user_property.encode(buffer);
    }

    fn encoded_size(&self) -> usize {
        let mut len = 0;

        len += self.reason_str.encoded_size();
        len += self.user_property.encoded_size();

        len
    }
}

impl Decoder for PubRelProperties {
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
        let mut puback_properties = PubRelProperties::default();

        loop {
            let p = Property::decode(&mut encoded_properties, None)?.unwrap();

            match p {
                Property::ReasonString => {
                    puback_properties.reason_str =
                        ReasonString::decode(&mut encoded_properties, None)?
                }

                Property::UserProperty => {
                    let user_property =
                        UserProperty::decode(&mut encoded_properties, None)?.unwrap();

                    if let Some(v) = &mut puback_properties.user_property {
                        v.push(user_property);
                    } else {
                        let v = vec![user_property];
                        puback_properties.user_property = Some(v);
                    }
                }
                _ => return Err(ReasonCode::MalformedPacket),
            }
        }
    }
}

#[derive(Default, Debug, PartialEq)]
pub struct PubRelPacket {
    packet_id: u16,
    reason: ReasonCode,
    properties: Option<PubRelProperties>,
}

impl ControlPacket for PubRelPacket {
    fn packet_type(&self) -> ControlPacketType {
        ControlPacketType::PubRel
    }
}

impl Encoder for PubRelPacket {
    fn encode(&self, buffer: &mut BytesMut) {
        let mut remaining_len = 0;

        buffer.put_u8(((self.packet_type() as u8) << 4) | 0x02);

        remaining_len += self.packet_id.encoded_size();
        remaining_len += self.reason.encoded_size();
        remaining_len += VariableByteInteger(self.properties.encoded_size() as u32).encoded_size();
        remaining_len += self.properties.encoded_size();

        VariableByteInteger(remaining_len as u32).encode(buffer);

        self.packet_id.encode(buffer);
        self.reason.encode(buffer);
        VariableByteInteger(self.properties.encoded_size() as u32).encode(buffer);
        self.properties.encode(buffer);
    }

    fn encoded_size(&self) -> usize {
        unimplemented!()
    }
}

impl Decoder for PubRelPacket {
    type Context = ();

    fn decode<T: Buf>(
        buffer: &mut T,
        _context: Option<&Self::Context>,
    ) -> Result<Option<Self>, ReasonCode> {
        buffer.advance(1);
        let _ = VariableByteInteger::decode(buffer, None);
        let packet_id = u16::decode(buffer, None)?.unwrap();
        let reason = ReasonCode::decode(buffer, Some(&ControlPacketType::PubRel))?.unwrap();
        let properties = PubRelProperties::decode(buffer, None)?;

        Ok(Some(PubRelPacket {
            packet_id,
            reason,
            properties,
        }))
    }
}
#[cfg(test)]
mod tests {
    use bytes::{Bytes, BytesMut};

    use crate::{
        endec::{Decoder, Encoder},
        packets::pubrel::PubRelPacket,
        reason::ReasonCode,
    };

    #[test]
    fn test_pubrel_packet_encode_decode() {
        let expected = vec![0x62, 0x04, 0x00, 0x01, 0x92, 0x00];

        let packet = PubRelPacket {
            packet_id: 1,
            reason: ReasonCode::PacketIdentifierNotFound,
            properties: None,
        };

        let mut encoded = BytesMut::new();
        packet.encode(&mut encoded);

        assert_eq!(encoded, expected);

        let mut bytes = Bytes::from(expected);

        let new_packet = PubRelPacket::decode(&mut bytes, None)
            .expect("Unexpected error")
            .unwrap();
        assert_eq!(packet, new_packet);
    }
}
