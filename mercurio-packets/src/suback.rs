//! SUBACK, the server's answer to SUBSCRIBE, with one result per topic
//! filter (section 3.9).

#[cfg(not(feature = "std"))]
use alloc::vec::Vec;

use bytes::{Buf, BytesMut};

use mercurio_core::{
    codec::{Decoder, Encoder},
    error::EncodeError,
    properties::*,
    protocol::ProtocolVersion,
    reason::ReasonCode,
};

use crate::{push_user_property, set_once, Packet, PacketType};

#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct SubAckProperties {
    pub reason_string: Option<ReasonString>,
    pub user_property: Option<Vec<UserProperty>>,
}

impl Encoder for SubAckProperties {
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

impl Decoder for SubAckProperties {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        use Property::*;

        let mut properties = SubAckProperties::default();

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubAckPayload {
    /// Result for the topic filter at the same position in the SUBSCRIBE. In
    /// MQTT 3.x every failure is encoded as 0x80, and decoded as
    /// `UnspecifiedError`.
    pub reason_code: ReasonCode,
}

impl Encoder for SubAckPayload {
    fn encode(&self, buffer: &mut bytes::BytesMut) -> Result<(), EncodeError> {
        crate::encode_reason(self.reason_code, PacketType::SubAck, buffer)
    }

    fn encoded_size(&self) -> usize {
        let mut len = 0;

        len += self.reason_code.encoded_size();

        len
    }
}

impl Decoder for SubAckPayload {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        let reason_code = crate::decode_reason(buffer, PacketType::SubAck)?;

        Ok(SubAckPayload { reason_code })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubAckPacket {
    pub packet_id: u16,
    /// MQTT 5 only.
    pub properties: Option<SubAckProperties>,
    pub payload: Vec<SubAckPayload>,
}

const PACKET_TYPE: u8 = 0x09;

impl Packet for SubAckPacket {
    fn encode(
        &self,
        buffer: &mut bytes::BytesMut,
        version: ProtocolVersion,
    ) -> Result<(), EncodeError> {
        crate::write_packet(buffer, |buffer| {
            if self.payload.is_empty() {
                return Err(EncodeError::EmptyPayload(PacketType::SubAck));
            }
            // Fixed header
            let fixed_header: u8 = PACKET_TYPE << 4;
            fixed_header.encode(buffer)?;

            crate::encode_length(self.remaining_len(version), buffer)?;

            crate::encode_packet_id(self.packet_id, buffer)?;

            if version.supports_properties() {
                crate::encode_length(self.properties.encoded_size(), buffer)?;
                self.properties.encode(buffer)?;
                self.payload.encode(buffer)?;
            } else {
                // MQTT 3.x only has the granted QoS return codes and 0x80 for failure
                for p in &self.payload {
                    let return_code: u8 = match p.reason_code {
                        ReasonCode::GrantedQoS0 => 0x00,
                        ReasonCode::GrantedQoS1 => 0x01,
                        ReasonCode::GrantedQoS2 => 0x02,
                        _ => 0x80,
                    };
                    return_code.encode(buffer)?;
                }
            }
            Ok(())
        })
    }

    fn encoded_size(&self, version: ProtocolVersion) -> usize {
        crate::packet_size(self.remaining_len(version))
    }

    fn decode<T: Buf>(buffer: &mut T, version: ProtocolVersion) -> crate::Result<Self> {
        crate::decode_packet(buffer, PacketType::SubAck, version, |_, buffer| {
            let packet_id = crate::decode_packet_id(buffer)?;
            let properties = if version.supports_properties() {
                crate::present(SubAckProperties::decode(buffer)?)
            } else {
                None
            };

            // One code per filter of the SUBSCRIBE, which has at least one
            if !buffer.has_remaining() {
                return Err(ReasonCode::ProtocolError.into());
            }

            let mut payload = Vec::new();
            while buffer.has_remaining() {
                let reason_code = if version.supports_reason_codes() {
                    crate::decode_reason(buffer, PacketType::SubAck)?
                } else {
                    // MQTT 3.x return codes: the granted QoS, or 0x80
                    match u8::decode(buffer)? {
                        0x00 => ReasonCode::GrantedQoS0,
                        0x01 => ReasonCode::GrantedQoS1,
                        0x02 => ReasonCode::GrantedQoS2,
                        0x80 => ReasonCode::UnspecifiedError,
                        // Reserved [MQTT-3.9.3-2]
                        _ => return Err(ReasonCode::MalformedPacket.into()),
                    }
                };
                payload.push(SubAckPayload { reason_code });
            }

            Ok(SubAckPacket {
                packet_id,
                properties,
                payload,
            })
        })
    }
}

impl SubAckPacket {
    fn remaining_len(&self, version: ProtocolVersion) -> usize {
        // Every return code is one byte, in MQTT 3.x as in MQTT 5
        let mut len = self.packet_id.encoded_size() + self.payload.encoded_size();
        if version.supports_properties() {
            len += crate::property_list_size(&self.properties);
        }
        len
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_v3_encode_maps_failures_to_0x80() {
        let packet = SubAckPacket {
            packet_id: 1,
            properties: None,
            payload: vec![
                SubAckPayload {
                    reason_code: ReasonCode::GrantedQoS1,
                },
                SubAckPayload {
                    reason_code: ReasonCode::TopicFilterInvalid,
                },
            ],
        };

        let mut buffer = BytesMut::new();
        packet.encode(&mut buffer, ProtocolVersion::V3_1_1).unwrap();
        assert_eq!(&buffer[..], &[0x90, 0x04, 0x00, 0x01, 0x01, 0x80]);
    }

    use bytes::{Bytes, BytesMut};

    use crate::suback::*;

    #[test]
    fn test_v5_decode_reads_0x00_as_granted_qos_0() {
        let mut bytes = Bytes::from_static(&[0x90, 0x04, 0x00, 0x01, 0x00, 0x00]);
        let packet = SubAckPacket::decode(&mut bytes, ProtocolVersion::V5).unwrap();
        assert_eq!(packet.payload[0].reason_code, ReasonCode::GrantedQoS0);
    }

    #[test]
    fn test_suback_packet_encode_decode() {
        let expected = vec![0x90, 0x04, 0x00, 0x01, 0x00, 0x01];

        let packet = SubAckPacket {
            packet_id: 1,
            properties: None,
            payload: vec![SubAckPayload {
                reason_code: ReasonCode::GrantedQoS1,
            }],
        };

        let mut encoded = BytesMut::new();
        packet.encode(&mut encoded, ProtocolVersion::V5).unwrap();

        assert_eq!(encoded.to_vec(), expected);

        let mut bytes = Bytes::from(expected);

        let new_packet =
            SubAckPacket::decode(&mut bytes, ProtocolVersion::V5).expect("Unexpected error");
        assert_eq!(packet, new_packet);
    }
}
