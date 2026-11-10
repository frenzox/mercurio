//! PUBREL, the second step of a QoS 2 delivery, answering PUBREC
//! (section 3.6).

#[cfg(not(feature = "std"))]
use alloc::vec::Vec;

use bytes::{Buf, BufMut, BytesMut};

use mercurio_core::{
    codec::{Decoder, Encoder, VariableByteInteger},
    properties::*,
    reason::ReasonCode,
};

use crate::PacketType;

#[derive(Default, Debug, PartialEq, Eq)]
pub struct PubRelProperties {
    pub reason_string: Option<ReasonString>,
    pub user_property: Option<Vec<UserProperty>>,
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
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        use Property::*;

        let mut properties = PubRelProperties::default();

        decode_property_list(buffer, |property| match property {
            ReasonString(v) => set_once(&mut properties.reason_string, v),
            UserProperty(v) => {
                push_user_property(&mut properties.user_property, v);
                Ok(())
            }
            _ => Err(ReasonCode::MalformedPacket.into()),
        })?;

        Ok(properties)
    }
}

#[derive(Default, Debug, PartialEq, Eq)]
pub struct PubRelPacket {
    pub packet_id: u16,
    /// Decodes as `Success` when left out. Left out on encode when `Success`
    /// with no properties, which is the only form MQTT 3.x has.
    pub reason: ReasonCode,
    pub properties: Option<PubRelProperties>,
}

const PACKET_TYPE: u8 = 0x06;

impl Encoder for PubRelPacket {
    fn encode(&self, buffer: &mut BytesMut) {
        let mut remaining_len = 0;

        buffer.put_u8((PACKET_TYPE << 4) | 0x02);

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

    fn encoded_size(&self) -> usize {
        let mut buffer = BytesMut::new();
        self.encode(&mut buffer);
        buffer.len()
    }
}

impl Decoder for PubRelPacket {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        crate::decode_packet(buffer, PacketType::PubRel, |_, buffer| {
            let packet_id = u16::decode(buffer)?;

            // Reason and properties may be left out (section 3.4.2.1)
            let reason = if buffer.has_remaining() {
                ReasonCode::decode(buffer)?
            } else {
                ReasonCode::Success
            };
            let properties = if buffer.has_remaining() {
                crate::present(PubRelProperties::decode(buffer)?)
            } else {
                None
            };

            Ok(PubRelPacket {
                packet_id,
                reason,
                properties,
            })
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

    use crate::pubrel::PubRelPacket;

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

        let new_packet = PubRelPacket::decode(&mut bytes).expect("Unexpected error");
        assert_eq!(packet, new_packet);
    }
}
