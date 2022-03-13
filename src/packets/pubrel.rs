use bytes::{Buf, BufMut, BytesMut};

use crate::codec::{Decoder, Encoder, VariableByteInteger};
use crate::error::Error;
use crate::properties::*;
use crate::reason::ReasonCode;
use crate::result::Result;

use super::control_packet_type::ControlPacketType;

#[derive(Default, Debug, PartialEq)]
pub struct PubRelProperties {
    reason_string: Option<ReasonString>,
    user_property: Option<Vec<UserProperty>>,
}

impl Encoder for PubRelProperties {
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

impl Decoder for PubRelProperties {
    type Context = ();

    fn decode<T: Buf>(buffer: &mut T, _context: Option<&Self::Context>) -> Result<Self> {
        use Property::*;

        let len = VariableByteInteger::decode(buffer, None)?;
        let mut properties = PubRelProperties::default();

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

#[derive(Default, Debug, PartialEq)]
pub struct PubRelPacket {
    packet_id: u16,
    reason: ReasonCode,
    properties: Option<PubRelProperties>,
}

impl ControlPacketType for PubRelPacket {
    const PACKET_TYPE: u8 = 0x06;
}

impl Encoder for PubRelPacket {
    fn encode(&self, buffer: &mut BytesMut) {
        let mut remaining_len = 0;

        buffer.put_u8((Self::PACKET_TYPE << 4) | 0x02);

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

    fn decode<T: Buf>(buffer: &mut T, _context: Option<&Self::Context>) -> Result<Self> {
        buffer.advance(1);

        let _ = VariableByteInteger::decode(buffer, None);
        let packet_id = u16::decode(buffer, None)?;
        let reason = ReasonCode::decode(buffer, None)?;
        let properties = Some(PubRelProperties::decode(buffer, None)?);

        Ok(PubRelPacket {
            packet_id,
            reason,
            properties,
        })
    }
}
#[cfg(test)]
mod tests {
    use bytes::{Bytes, BytesMut};

    use crate::{
        codec::{Decoder, Encoder},
        packets::pubrel::{PubRelPacket, PubRelProperties},
        reason::ReasonCode,
    };

    #[test]
    fn test_pubrel_packet_encode_decode() {
        let expected = vec![0x62, 0x04, 0x00, 0x01, 0x92, 0x00];

        let packet = PubRelPacket {
            packet_id: 1,
            reason: ReasonCode::PacketIdentifierNotFound,
            properties: PubRelProperties::default().into(),
        };

        let mut encoded = BytesMut::new();
        packet.encode(&mut encoded);

        assert_eq!(encoded, expected);

        let mut bytes = Bytes::from(expected);

        let new_packet = PubRelPacket::decode(&mut bytes, None).expect("Unexpected error");
        assert_eq!(packet, new_packet);
    }
}
