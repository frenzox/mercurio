use bytes::{Buf, BufMut, BytesMut};

use mercurio_core::{
    codec::{Decoder, Encoder, VariableByteInteger},
    error::Error,
    properties::*,
    reason::ReasonCode,
};

#[derive(Default, Debug, PartialEq, Eq)]
pub struct PubAckProperties {
    reason_string: Option<ReasonString>,
    user_property: Option<Vec<UserProperty>>,
}

impl Encoder for PubAckProperties {
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

impl Decoder for PubAckProperties {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        use Property::*;

        let len = VariableByteInteger::decode(buffer)?;
        let mut properties = PubAckProperties::default();

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

#[derive(Default, Debug, PartialEq, Eq)]
pub struct PubAckPacket {
    pub packet_id: u16,
    pub reason: ReasonCode,
    pub properties: Option<PubAckProperties>,
}

const PACKET_TYPE: u8 = 0x04;

impl Encoder for PubAckPacket {
    fn encode(&self, buffer: &mut BytesMut) {
        let mut remaining_len = 0;

        buffer.put_u8(PACKET_TYPE << 4);

        remaining_len += self.packet_id.encoded_size();

        if self.properties.is_some() || self.reason != ReasonCode::Success {
            remaining_len += self.reason.encoded_size();
            remaining_len +=
                VariableByteInteger(self.properties.encoded_size() as u32).encoded_size();
            remaining_len += self.properties.encoded_size();
        }

        VariableByteInteger(remaining_len as u32).encode(buffer);

        self.packet_id.encode(buffer);

        if remaining_len == 2 {
            return;
        }

        self.reason.encode(buffer);
        VariableByteInteger(self.properties.encoded_size() as u32).encode(buffer);
        self.properties.encode(buffer);
    }
}

impl Decoder for PubAckPacket {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        buffer.advance(1);

        let remaining_len = VariableByteInteger::decode(buffer)?;
        let packet_id = u16::decode(buffer)?;

        if remaining_len.0 == 2 {
            return Ok(PubAckPacket {
                packet_id,
                reason: ReasonCode::Success,
                properties: None,
            });
        }

        let reason = ReasonCode::decode(buffer)?;
        let properties = Some(PubAckProperties::decode(buffer)?);

        Ok(PubAckPacket {
            packet_id,
            reason,
            properties,
        })
    }
}

#[cfg(test)]
mod tests {
    use bytes::{Bytes, BytesMut};

    use mercurio_core::{
        codec::{Decoder, Encoder},
        reason::ReasonCode,
    };

    use crate::puback::{PubAckPacket, PubAckProperties};

    #[test]
    fn test_puback_packet_encode_decode() {
        let expected = vec![0x40, 0x04, 0x00, 0x01, 0x10, 0x00];

        let packet = PubAckPacket {
            packet_id: 1,
            reason: ReasonCode::NoMatchingSubscribers,
            properties: PubAckProperties::default().into(),
        };

        let mut encoded = BytesMut::new();
        packet.encode(&mut encoded);

        assert_eq!(encoded.to_vec(), expected);

        let mut bytes = Bytes::from(expected);

        let new_packet = PubAckPacket::decode(&mut bytes).expect("Unexpected error");
        assert_eq!(packet, new_packet);
    }
}
