use bytes::{Buf, BufMut, BytesMut};

use crate::endec::{Decoder, Encoder, VariableByteInteger};
use crate::properties::{Property, ReasonString, UserProperty};
use crate::reason::ReasonCode;
use crate::result::Result;

use super::control_packet_type::ControlPacketType;

#[derive(Default, Debug, PartialEq)]
pub struct PubRecProperties {
    reason_str: Option<ReasonString>,
    user_property: Option<Vec<UserProperty>>,
}

impl Encoder for PubRecProperties {
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

impl Decoder for PubRecProperties {
    type Context = ();

    fn decode<T: Buf>(buffer: &mut T, _context: Option<&Self::Context>) -> Result<Option<Self>> {
        let len = VariableByteInteger::decode(buffer, None)?.unwrap();
        if len.0 == 0 {
            return Ok(None);
        } else if (buffer.remaining() as u32) < len.0 {
            return Err(ReasonCode::MalformedPacket.into());
        }

        let mut encoded_properties = buffer.take(len.0 as usize);
        let mut puback_properties = PubRecProperties::default();

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
                _ => return Err(ReasonCode::MalformedPacket.into()),
            }
        }
    }
}

#[derive(Default, Debug, PartialEq)]
pub struct PubRecPacket {
    packet_id: u16,
    reason: ReasonCode,
    properties: Option<PubRecProperties>,
}

impl ControlPacketType for PubRecPacket {
    const PACKET_TYPE: u8 = 0x05;
}

impl Encoder for PubRecPacket {
    fn encode(&self, buffer: &mut BytesMut) {
        let mut remaining_len = 0;

        buffer.put_u8(Self::PACKET_TYPE << 4);

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

impl Decoder for PubRecPacket {
    type Context = ();

    fn decode<T: Buf>(buffer: &mut T, _context: Option<&Self::Context>) -> Result<Option<Self>> {
        buffer.advance(1);
        let _ = VariableByteInteger::decode(buffer, None);
        let packet_id = u16::decode(buffer, None)?.unwrap();
        let reason = ReasonCode::decode(buffer, None)?.unwrap();
        let properties = PubRecProperties::decode(buffer, None)?;

        Ok(Some(PubRecPacket {
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
        packets::pubrec::PubRecPacket,
        reason::ReasonCode,
    };

    #[test]
    fn test_pubrec_packet_encode_decode() {
        let expected = vec![0x50, 0x04, 0x00, 0x01, 0x10, 0x00];

        let packet = PubRecPacket {
            packet_id: 1,
            reason: ReasonCode::NoMatchingSubscribers,
            properties: None,
        };

        let mut encoded = BytesMut::new();
        packet.encode(&mut encoded);

        assert_eq!(encoded.to_vec(), expected);

        let mut bytes = Bytes::from(expected);

        let new_packet = PubRecPacket::decode(&mut bytes, None)
            .expect("Unexpected error")
            .unwrap();
        assert_eq!(packet, new_packet);
    }
}
