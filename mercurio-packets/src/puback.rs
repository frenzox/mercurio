//! PUBACK, the acknowledgement of a QoS 1 PUBLISH (section 3.4).

#[cfg(not(feature = "std"))]
use alloc::vec::Vec;

use bytes::{Buf, BufMut, BytesMut};

use mercurio_core::{
    codec::{Decoder, Encoder},
    error::EncodeError,
    properties::*,
    protocol::ProtocolVersion,
    reason::ReasonCode,
};

use crate::{push_user_property, set_once, Packet, PacketType};

#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct PubAckProperties {
    pub reason_string: Option<ReasonString>,
    pub user_property: Option<Vec<UserProperty>>,
}

impl Encoder for PubAckProperties {
    fn encode(&self, buffer: &mut BytesMut) -> Result<(), EncodeError> {
        self.reason_string.encode(buffer)?;
        self.user_property.encode(buffer)?;
        Ok(())
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

#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct PubAckPacket {
    pub packet_id: u16,
    /// Decodes as `Success` when left out. Left out on encode when `Success`
    /// with no properties, which is the only form MQTT 3.x has.
    pub reason: ReasonCode,
    pub properties: Option<PubAckProperties>,
}

const FIRST_BYTE: u8 = 0x40;

impl Packet for PubAckPacket {
    /// In MQTT 3.x only the packet id is written: the reason code and
    /// properties don't exist there.
    fn encode(&self, buffer: &mut BytesMut, version: ProtocolVersion) -> Result<(), EncodeError> {
        crate::write_packet(buffer, |buffer| {
            buffer.put_u8(FIRST_BYTE);
            crate::encode_length(self.remaining_len(version), buffer)?;

            crate::encode_packet_id(self.packet_id, buffer)?;

            if self.has_reason(version) {
                crate::encode_reason(self.reason, PacketType::PubAck, buffer)?;
                crate::encode_length(self.properties.encoded_size(), buffer)?;
                self.properties.encode(buffer)?;
            }
            Ok(())
        })
    }

    fn encoded_size(&self, version: ProtocolVersion) -> usize {
        crate::packet_size(self.remaining_len(version))
    }

    fn decode<T: Buf>(buffer: &mut T, version: ProtocolVersion) -> crate::Result<Self> {
        crate::decode_packet(buffer, PacketType::PubAck, version, |_, buffer| {
            let packet_id = crate::decode_packet_id(buffer)?;

            // Reason and properties may be left out (section 3.4.2.1), and
            // MQTT 3.x has neither
            let reason = if version.supports_reason_codes() && buffer.has_remaining() {
                crate::decode_reason(buffer, PacketType::PubAck)?
            } else {
                ReasonCode::Success
            };
            let properties = if version.supports_properties() && buffer.has_remaining() {
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

impl PubAckPacket {
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

    use crate::{puback::PubAckPacket, Packet};

    #[test]
    fn test_puback_packet_encode_decode() {
        let expected = vec![0x40, 0x04, 0x00, 0x01, 0x10, 0x00];

        let packet = PubAckPacket {
            packet_id: 1,
            reason: ReasonCode::NoMatchingSubscribers,
            properties: None,
        };

        let mut encoded = BytesMut::new();
        packet.encode(&mut encoded, ProtocolVersion::V5).unwrap();

        assert_eq!(encoded.to_vec(), expected);

        let mut bytes = Bytes::from(expected);

        let new_packet =
            PubAckPacket::decode(&mut bytes, ProtocolVersion::V5).expect("Unexpected error");
        assert_eq!(packet, new_packet);
    }
}
