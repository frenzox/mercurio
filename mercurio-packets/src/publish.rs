//! PUBLISH, an application message, in either direction (section 3.3).

#[cfg(not(feature = "std"))]
use alloc::{string::String, vec::Vec};

use bytes::{Buf, Bytes, BytesMut};

use mercurio_core::{
    codec::{Decoder, Encoder},
    error::EncodeError,
    error::Error,
    properties::*,
    protocol::ProtocolVersion,
    qos::QoS,
    reason::ReasonCode,
};

use crate::{Packet, PacketType};

#[derive(Default, Debug, PartialEq, Eq, Clone)]
/// MQTT 5 only.
pub struct PublishProperties {
    /// 1 means the payload is UTF-8 text. Absent or 0 means unspecified
    /// bytes.
    pub payload_format_indicator: Option<PayloadFormatIndicator>,
    /// Lifetime of the message in seconds. Absent means it doesn't expire.
    pub message_expiry_interval: Option<MessageExpiryInterval>,
    /// Number that stands for the topic name on this connection, which then
    /// may be empty (section 3.3.2.3.4).
    pub topic_alias: Option<TopicAlias>,
    pub response_topic: Option<ResponseTopic>,
    pub correlation_data: Option<CorrelationData>,
    pub user_property: Option<Vec<UserProperty>>,
    /// One per matching subscription that has an identifier [MQTT-3.3.4-4]
    pub subscription_identifiers: Vec<SubscriptionIdentifier>,
    pub content_type: Option<ContentType>,
}

impl Encoder for PublishProperties {
    fn encode(&self, buffer: &mut bytes::BytesMut) -> Result<(), EncodeError> {
        self.payload_format_indicator.encode(buffer)?;
        self.message_expiry_interval.encode(buffer)?;
        self.topic_alias.encode(buffer)?;
        self.response_topic.encode(buffer)?;
        self.correlation_data.encode(buffer)?;
        self.user_property.encode(buffer)?;
        self.subscription_identifiers.encode(buffer)?;
        self.content_type.encode(buffer)?;
        Ok(())
    }

    fn encoded_size(&self) -> usize {
        let mut len = 0;

        len += self.payload_format_indicator.encoded_size();
        len += self.message_expiry_interval.encoded_size();
        len += self.topic_alias.encoded_size();
        len += self.response_topic.encoded_size();
        len += self.correlation_data.encoded_size();
        len += self.user_property.encoded_size();
        len += self.subscription_identifiers.encoded_size();
        len += self.content_type.encoded_size();
        len
    }
}

impl Decoder for PublishProperties {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        use Property::*;

        let mut properties = PublishProperties::default();

        decode_property_list(buffer, |property| match property {
            PayloadFormatIndicator(v) => set_once(&mut properties.payload_format_indicator, v),
            MessageExpiryInterval(v) => set_once(&mut properties.message_expiry_interval, v),
            TopicAlias(v) => set_once(&mut properties.topic_alias, v),
            ResponseTopic(v) => set_once(&mut properties.response_topic, v),
            CorrelationData(v) => set_once(&mut properties.correlation_data, v),
            UserProperty(v) => {
                push_user_property(&mut properties.user_property, v);
                Ok(())
            }
            SubscriptionIdentifier(v) => {
                properties.subscription_identifiers.push(v);
                Ok(())
            }
            ContentType(v) => set_once(&mut properties.content_type, v),
            _ => Err(ReasonCode::MalformedPacket.into()),
        })?;

        Ok(properties)
    }
}

#[derive(Default, Debug, PartialEq, Eq, Clone)]
pub struct PublishPacket {
    /// Set on a resend of a QoS 1 or 2 message. Never set at QoS 0
    /// [MQTT-3.3.1-2].
    pub dup: bool,
    pub qos_level: QoS,
    /// From a client, store the message as the topic's retained message, or
    /// clear it if the payload is empty. From the server, see
    /// `SubscriptionOptions::retain_as_pub`.
    pub retain: bool,
    /// May be empty when `PublishProperties::topic_alias` is set.
    pub topic_name: String,
    /// Set at QoS 1 and 2, `None` at QoS 0.
    pub packet_id: Option<u16>,
    pub properties: Option<PublishProperties>,
    /// `None` encodes as an empty payload. Decoding always gives `Some`.
    pub payload: Option<Bytes>,
}

const PACKET_TYPE: u8 = 0x03;

impl Packet for PublishPacket {
    fn encode(&self, buffer: &mut BytesMut, version: ProtocolVersion) -> Result<(), EncodeError> {
        crate::write_packet(buffer, |buffer| {
            match (self.qos_level, self.packet_id) {
                (QoS::AtMostOnce, None) => {}
                (QoS::AtMostOnce, Some(_)) | (_, None) => {
                    return Err(EncodeError::PacketIdMismatch)
                }
                (_, Some(0)) => return Err(EncodeError::PacketIdZero),
                (_, Some(_)) => {}
            }
            if self.dup && self.qos_level == QoS::AtMostOnce {
                return Err(EncodeError::DupAtQoS0);
            }
            self.encode_inner(buffer, version.supports_properties())?;
            Ok(())
        })
    }

    fn encoded_size(&self, version: ProtocolVersion) -> usize {
        crate::packet_size(self.remaining_len(version.supports_properties()))
    }

    fn decode<T: Buf>(buffer: &mut T, version: ProtocolVersion) -> crate::Result<Self> {
        crate::decode_packet(buffer, PacketType::Publish, version, |flags, body| {
            Self::decode_body(flags, body, version.supports_properties())
        })
    }
}

impl PublishPacket {
    fn remaining_len(&self, with_properties: bool) -> usize {
        let mut len = self.topic_name.encoded_size() + self.packet_id.encoded_size();
        if with_properties {
            len += crate::property_list_size(&self.properties);
        }
        len + self.payload.as_ref().map_or(0, |payload| payload.len())
    }

    fn encode_inner(
        &self,
        buffer: &mut BytesMut,
        with_properties: bool,
    ) -> Result<(), EncodeError> {
        // Fixed header
        let mut fixed_header: u8 = PACKET_TYPE << 4;
        fixed_header |= (self.dup as u8) << 3;
        fixed_header |= (self.qos_level as u8) << 1;
        fixed_header |= self.retain as u8;
        fixed_header.encode(buffer)?;

        crate::encode_length(self.remaining_len(with_properties), buffer)?;

        // Variable header
        self.topic_name.encode(buffer)?;
        self.packet_id.encode(buffer)?;
        if with_properties {
            crate::encode_length(self.properties.encoded_size(), buffer)?;
            self.properties.encode(buffer)?;
        }

        // Payload. Here it goes raw, shouldn't be encoded
        if let Some(payload) = &self.payload {
            buffer.extend(payload);
        }
        Ok(())
    }

    fn decode_body<T: Buf>(
        flags: u8,
        buffer: &mut T,
        with_properties: bool,
    ) -> crate::Result<Self> {
        let dup = (flags & 0b0000_1000) != 0;
        let qos_level = QoS::try_from((flags & 0b0000_0110) >> 1)
            .map_err(|_| Error::from(ReasonCode::MalformedPacket))?;
        let retain = (flags & 0b0000_0001) != 0;

        // Variable header
        let topic_name = String::decode(buffer)?;
        let packet_id = match qos_level {
            QoS::AtMostOnce => None,
            _ => Some(crate::decode_packet_id(buffer)?),
        };
        // A QoS 0 message is never retransmitted [MQTT-3.3.1-2]
        if dup && qos_level == QoS::AtMostOnce {
            return Err(ReasonCode::MalformedPacket.into());
        }

        let properties = if with_properties {
            Some(PublishProperties::decode(buffer)?)
        } else {
            None
        };

        // The payload is the rest of the packet
        let payload = Some(buffer.copy_to_bytes(buffer.remaining()));

        Ok(PublishPacket {
            dup,
            qos_level,
            retain,
            topic_name,
            packet_id,
            properties,
            payload,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{string::ToString, vec::Vec};

    use crate::publish::*;

    #[test]
    fn test_publish_packet_encode_decode() {
        let expected = vec![
            0x32, 0x28, 0x00, 0x0a, 0x74, 0x65, 0x73, 0x74, 0x5f, 0x74, 0x6f, 0x70, 0x69, 0x63,
            0x00, 0x01, 0x0d, 0x26, 0x00, 0x03, 0x6b, 0x65, 0x79, 0x00, 0x05, 0x76, 0x61, 0x6c,
            0x75, 0x65, 0x74, 0x65, 0x73, 0x74, 0x5f, 0x6d, 0x65, 0x73, 0x73, 0x61, 0x67, 0x65,
        ];

        let packet = PublishPacket {
            dup: false,
            qos_level: QoS::AtLeastOnce,
            retain: false,
            topic_name: "test_topic".to_string(),
            packet_id: Some(1),
            properties: PublishProperties {
                user_property: vec![UserProperty::new("key".to_string(), "value".to_string())]
                    .into(),
                ..Default::default()
            }
            .into(),
            payload: Bytes::from("test_message").into(),
        };

        let mut encoded = BytesMut::new();
        packet.encode(&mut encoded, ProtocolVersion::V5).unwrap();

        assert_eq!(encoded, expected);

        let mut bytes = Bytes::from(expected);

        let new_packet =
            PublishPacket::decode(&mut bytes, ProtocolVersion::V5).expect("Unexpected error");
        assert_eq!(packet, new_packet);
    }

    #[test]
    fn test_publish_keeps_every_subscription_identifier() {
        // QoS 0 PUBLISH on "t" with identifiers 3 and 9, empty payload
        let mut bytes =
            Bytes::from_static(&[0x30, 0x08, 0x00, 0x01, b't', 0x04, 0x0b, 0x03, 0x0b, 0x09]);

        let packet = PublishPacket::decode(&mut bytes, ProtocolVersion::V5).unwrap();
        let identifiers: Vec<u32> = packet
            .properties
            .unwrap()
            .subscription_identifiers
            .iter()
            .map(|s| s.value.0)
            .collect();
        assert_eq!(identifiers, [3, 9]);
    }
}
