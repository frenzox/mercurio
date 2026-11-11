//! UNSUBSCRIBE, a client's request to stop receiving some topic filters'
//! messages (section 3.10).

#[cfg(not(feature = "std"))]
use alloc::{string::String, vec::Vec};

use bytes::Buf;

use mercurio_core::{
    codec::{Decoder, Encoder},
    error::EncodeError,
    properties::*,
    protocol::ProtocolVersion,
    reason::ReasonCode,
};

use crate::{Packet, PacketType};

#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct UnsubscribeProperties {
    pub user_property: Option<Vec<UserProperty>>,
}

impl Encoder for UnsubscribeProperties {
    fn encode(&self, buffer: &mut bytes::BytesMut) -> Result<(), EncodeError> {
        self.user_property.encode(buffer)?;
        Ok(())
    }

    fn encoded_size(&self) -> usize {
        let mut len = 0;

        len += self.user_property.encoded_size();

        len
    }
}

impl Decoder for UnsubscribeProperties {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        use Property::*;

        let mut properties = UnsubscribeProperties::default();

        decode_property_list(buffer, |property| match property {
            UserProperty(v) => {
                push_user_property(&mut properties.user_property, v);
                Ok(())
            }
            _ => Err(ReasonCode::MalformedPacket.into()),
        })?;

        Ok(properties)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnsubscribePayload {
    pub topic_filter: String,
}

impl Encoder for UnsubscribePayload {
    fn encode(&self, buffer: &mut bytes::BytesMut) -> Result<(), EncodeError> {
        self.topic_filter.encode(buffer)?;
        Ok(())
    }

    fn encoded_size(&self) -> usize {
        let mut len = 0;

        len += self.topic_filter.encoded_size();

        len
    }
}

impl Decoder for UnsubscribePayload {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        let topic_filter = String::decode(buffer)?;

        Ok(UnsubscribePayload { topic_filter })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnsubscribePacket {
    pub packet_id: u16,
    pub properties: Option<UnsubscribeProperties>,
    pub payload: Vec<UnsubscribePayload>,
}

const PACKET_TYPE: u8 = 0x0a;

impl Packet for UnsubscribePacket {
    fn encode(
        &self,
        buffer: &mut bytes::BytesMut,
        version: ProtocolVersion,
    ) -> Result<(), EncodeError> {
        crate::write_packet(buffer, |buffer| {
            if self.payload.is_empty() {
                return Err(EncodeError::EmptyPayload(PacketType::Unsubscribe));
            }
            self.encode_inner(buffer, version.supports_properties())?;
            Ok(())
        })
    }

    fn encoded_size(&self, version: ProtocolVersion) -> usize {
        crate::packet_size(self.remaining_len(version.supports_properties()))
    }

    fn decode<T: Buf>(buffer: &mut T, version: ProtocolVersion) -> crate::Result<Self> {
        crate::decode_packet(buffer, PacketType::Unsubscribe, version, |flags, body| {
            Self::decode_body(flags, body, version.supports_properties())
        })
    }
}

impl UnsubscribePacket {
    fn remaining_len(&self, with_properties: bool) -> usize {
        let mut len = self.packet_id.encoded_size() + self.payload.encoded_size();
        if with_properties {
            len += crate::property_list_size(&self.properties);
        }
        len
    }

    fn encode_inner(
        &self,
        buffer: &mut bytes::BytesMut,
        with_properties: bool,
    ) -> Result<(), EncodeError> {
        // Fixed header
        let mut fixed_header: u8 = PACKET_TYPE << 4;
        fixed_header |= 0b0000_0010;
        fixed_header.encode(buffer)?;

        crate::encode_length(self.remaining_len(with_properties), buffer)?;

        crate::encode_packet_id(self.packet_id, buffer)?;
        if with_properties {
            crate::encode_length(self.properties.encoded_size(), buffer)?;
            self.properties.encode(buffer)?;
        }
        self.payload.encode(buffer)?;
        Ok(())
    }

    fn decode_body<T: Buf>(_: u8, buffer: &mut T, with_properties: bool) -> crate::Result<Self> {
        let packet_id = crate::decode_packet_id(buffer)?;
        let properties = if with_properties {
            crate::present(UnsubscribeProperties::decode(buffer)?)
        } else {
            None
        };

        if !buffer.has_remaining() {
            return Err(ReasonCode::ProtocolError.into());
        }

        let mut payload = Vec::new();

        while buffer.has_remaining() {
            payload.push(UnsubscribePayload::decode(buffer)?);
        }

        Ok(UnsubscribePacket {
            packet_id,
            properties,
            payload,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::string::ToString;

    use bytes::{Bytes, BytesMut};

    use crate::unsubscribe::*;

    #[test]
    fn test_unsubscribe_packet_encode_decode() {
        let expected = vec![
            0xa2, 0x0f, 0x00, 0x01, 0x00, 0x00, 0x0a, 0x74, 0x65, 0x73, 0x74, 0x5f, 0x74, 0x6f,
            0x70, 0x69, 0x63,
        ];

        let packet = UnsubscribePacket {
            packet_id: 1,
            properties: None,
            payload: vec![UnsubscribePayload {
                topic_filter: "test_topic".to_string(),
            }],
        };

        let mut encoded = BytesMut::new();
        packet.encode(&mut encoded, ProtocolVersion::V5).unwrap();

        assert_eq!(encoded.to_vec(), expected);

        let mut bytes = Bytes::from(expected);

        let new_packet =
            UnsubscribePacket::decode(&mut bytes, ProtocolVersion::V5).expect("Unexpected error");
        assert_eq!(packet, new_packet);
    }
}
