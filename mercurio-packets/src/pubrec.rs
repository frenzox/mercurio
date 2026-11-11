//! PUBREC, the first answer to a QoS 2 PUBLISH (section 3.5).

#[cfg(not(feature = "std"))]
use alloc::vec::Vec;

use bytes::{Buf, BufMut, BytesMut};

use mercurio_core::{
    codec::{Decoder, Encoder, VariableByteInteger},
    properties::*,
    protocol::ProtocolVersion,
    reason::ReasonCode,
};

use crate::{Packet, PacketType};

#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct PubRecProperties {
    pub reason_string: Option<ReasonString>,
    pub user_property: Option<Vec<UserProperty>>,
}

impl Encoder for PubRecProperties {
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

impl Decoder for PubRecProperties {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        use Property::*;

        let mut properties = PubRecProperties::default();

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

#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct PubRecPacket {
    pub packet_id: u16,
    /// Decodes as `Success` when left out. Left out on encode when `Success`
    /// with no properties, which is the only form MQTT 3.x has.
    pub reason: ReasonCode,
    pub properties: Option<PubRecProperties>,
}

const FIRST_BYTE: u8 = 0x50;

impl Packet for PubRecPacket {
    /// In MQTT 3.x only the packet id is written: the reason code and
    /// properties don't exist there.
    fn encode(&self, buffer: &mut BytesMut, version: ProtocolVersion) {
        buffer.put_u8(FIRST_BYTE);
        VariableByteInteger(self.remaining_len(version) as u32).encode(buffer);

        self.packet_id.encode(buffer);

        if self.has_reason(version) {
            self.reason.encode(buffer);
            VariableByteInteger(self.properties.encoded_size() as u32).encode(buffer);
            self.properties.encode(buffer);
        }
    }

    fn encoded_size(&self, version: ProtocolVersion) -> usize {
        crate::packet_size(self.remaining_len(version))
    }

    fn decode<T: Buf>(buffer: &mut T, version: ProtocolVersion) -> crate::Result<Self> {
        crate::decode_packet(buffer, PacketType::PubRec, version, |_, buffer| {
            let packet_id = crate::decode_packet_id(buffer)?;

            // Reason and properties may be left out (section 3.5.2.1), and
            // MQTT 3.x has neither
            let reason = if version.supports_reason_codes() && buffer.has_remaining() {
                crate::decode_reason(buffer, PacketType::PubRec)?
            } else {
                ReasonCode::Success
            };
            let properties = if version.supports_properties() && buffer.has_remaining() {
                crate::present(PubRecProperties::decode(buffer)?)
            } else {
                None
            };

            Ok(PubRecPacket {
                packet_id,
                reason,
                properties,
            })
        })
    }
}

impl PubRecPacket {
    /// Whether the reason and properties are written: in MQTT 5, unless the
    /// reason is `Success` and there are no properties.
    fn has_reason(&self, version: ProtocolVersion) -> bool {
        version.supports_reason_codes()
            && (self.properties.is_some() || self.reason != ReasonCode::Success)
    }

    fn remaining_len(&self, version: ProtocolVersion) -> usize {
        let mut len = self.packet_id.encoded_size();
        if self.has_reason(version) {
            len += self.reason.encoded_size() + crate::property_list_size(&self.properties);
        }
        len
    }
}

#[cfg(test)]
mod tests {
    use bytes::{Bytes, BytesMut};

    use mercurio_core::{protocol::ProtocolVersion, reason::ReasonCode};

    use crate::{pubrec::PubRecPacket, Packet};

    #[test]
    fn test_pubrec_packet_encode_decode() {
        let expected = vec![0x50, 0x04, 0x00, 0x01, 0x10, 0x00];

        let packet = PubRecPacket {
            packet_id: 1,
            reason: ReasonCode::NoMatchingSubscribers,
            properties: None,
        };

        let mut encoded = BytesMut::new();
        packet.encode(&mut encoded, ProtocolVersion::V5);

        assert_eq!(encoded.to_vec(), expected);

        let mut bytes = Bytes::from(expected);

        let new_packet =
            PubRecPacket::decode(&mut bytes, ProtocolVersion::V5).expect("Unexpected error");
        assert_eq!(packet, new_packet);
    }
}
