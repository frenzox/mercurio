//! PUBACK, the acknowledgement of a QoS 1 PUBLISH (section 3.4).

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
pub struct PubAckProperties {
    pub reason_string: Option<ReasonString>,
    pub user_property: Option<Vec<UserProperty>>,
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

        let mut properties = PubAckProperties::default();

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
pub struct PubAckPacket {
    pub packet_id: u16,
    /// Decodes as `Success` when left out. Left out on encode when `Success`
    /// with no properties, which is the only form MQTT 3.x has.
    pub reason: ReasonCode,
    pub properties: Option<PubAckProperties>,
}

const PACKET_TYPE: u8 = 0x04;

impl Encoder for PubAckPacket {
    fn encode(&self, buffer: &mut BytesMut) {
        buffer.put_u8(PACKET_TYPE << 4);
        VariableByteInteger(self.remaining_len() as u32).encode(buffer);

        self.packet_id.encode(buffer);

        if self.has_reason() {
            self.reason.encode(buffer);
            VariableByteInteger(self.properties.encoded_size() as u32).encode(buffer);
            self.properties.encode(buffer);
        }
    }

    fn encoded_size(&self) -> usize {
        crate::packet_size(self.remaining_len())
    }
}

impl PubAckPacket {
    /// Whether the reason and properties are written: they are left out
    /// when the reason is `Success` and there are no properties.
    fn has_reason(&self) -> bool {
        self.properties.is_some() || self.reason != ReasonCode::Success
    }

    fn remaining_len(&self) -> usize {
        let mut len = self.packet_id.encoded_size();
        if self.has_reason() {
            len += self.reason.encoded_size() + crate::property_list_size(&self.properties);
        }
        len
    }
}

impl Decoder for PubAckPacket {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        crate::decode_packet(buffer, PacketType::PubAck, |_, buffer| {
            let packet_id = crate::decode_packet_id(buffer)?;

            // Reason and properties may be left out (section 3.4.2.1)
            let reason = if buffer.has_remaining() {
                ReasonCode::decode(buffer)?
            } else {
                ReasonCode::Success
            };
            let properties = if buffer.has_remaining() {
                crate::present(PubAckProperties::decode(buffer)?)
            } else {
                None
            };

            Ok(PubAckPacket {
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

    use crate::puback::PubAckPacket;

    #[test]
    fn test_puback_packet_encode_decode() {
        let expected = vec![0x40, 0x04, 0x00, 0x01, 0x10, 0x00];

        let packet = PubAckPacket {
            packet_id: 1,
            reason: ReasonCode::NoMatchingSubscribers,
            properties: None,
        };

        let mut encoded = BytesMut::new();
        packet.encode(&mut encoded);

        assert_eq!(encoded.to_vec(), expected);

        let mut bytes = Bytes::from(expected);

        let new_packet = PubAckPacket::decode(&mut bytes).expect("Unexpected error");
        assert_eq!(packet, new_packet);
    }
}
