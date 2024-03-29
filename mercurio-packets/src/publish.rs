use bytes::{Buf, Bytes, BytesMut};

use mercurio_core::{
    codec::{Decoder, Encoder, VariableByteInteger},
    error::Error,
    properties::*,
    qos::QoS,
    reason::ReasonCode,
};

#[derive(Default, Debug, PartialEq, Eq, Clone)]
pub struct PublishProperties {
    payload_format_indicator: Option<PayloadFormatIndicator>,
    message_expiry_interval: Option<MessageExpiryInterval>,
    topic_alias: Option<TopicAlias>,
    response_topic: Option<ResponseTopic>,
    correlation_data: Option<CorrelationData>,
    user_property: Option<Vec<UserProperty>>,
    subscription_identifier: Option<SubscriptionIdentifier>,
    content_type: Option<ContentType>,
}

impl Encoder for PublishProperties {
    fn encode(&self, buffer: &mut bytes::BytesMut) {
        self.payload_format_indicator.encode(buffer);
        self.message_expiry_interval.encode(buffer);
        self.topic_alias.encode(buffer);
        self.response_topic.encode(buffer);
        self.correlation_data.encode(buffer);
        self.user_property.encode(buffer);
        self.subscription_identifier.encode(buffer);
        self.content_type.encode(buffer);
    }

    fn encoded_size(&self) -> usize {
        let mut len = 0;

        len += self.payload_format_indicator.encoded_size();
        len += self.message_expiry_interval.encoded_size();
        len += self.topic_alias.encoded_size();
        len += self.response_topic.encoded_size();
        len += self.correlation_data.encoded_size();
        len += self.user_property.encoded_size();
        len += self.subscription_identifier.encoded_size();
        len += self.content_type.encoded_size();
        len
    }
}

impl Decoder for PublishProperties {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        use Property::*;

        let len = VariableByteInteger::decode(buffer)?;
        let mut properties = PublishProperties::default();

        if len.0 == 0 {
            return Ok(properties);
        } else if (buffer.remaining() as u32) < len.0 {
            return Err(Error::PacketIncomplete);
        }

        let mut encoded_properties = buffer.take(len.0 as usize);

        while encoded_properties.has_remaining() {
            match Property::decode(&mut encoded_properties)? {
                PayloadFormatIndicator(v) => properties.payload_format_indicator = Some(v),
                MessageExpiryInterval(v) => properties.message_expiry_interval = Some(v),
                TopicAlias(v) => properties.topic_alias = Some(v),
                ResponseTopic(v) => properties.response_topic = Some(v),
                CorrelationData(v) => properties.correlation_data = Some(v),
                UserProperty(v) => {
                    if let Some(vec) = &mut properties.user_property {
                        vec.push(v);
                    } else {
                        let vec = vec![v];
                        properties.user_property = Some(vec);
                    }
                }
                SubscriptionIdentifier(v) => properties.subscription_identifier = Some(v),
                ContentType(v) => properties.content_type = Some(v),
                _ => return Err(ReasonCode::MalformedPacket.into()),
            }
        }

        Ok(properties)
    }
}

#[derive(Default, Debug, PartialEq, Eq, Clone)]
pub struct PublishPacket {
    pub dup: bool,
    pub qos_level: QoS,
    pub retain: bool,
    pub topic_name: String,
    pub packet_id: Option<u16>,
    pub properties: Option<PublishProperties>,
    pub payload: Option<Bytes>,
}

const PACKET_TYPE: u8 = 0x03;

impl Encoder for PublishPacket {
    fn encode(&self, buffer: &mut BytesMut) {
        let mut remaining_len = 0;

        // Fixed header
        let mut fixed_header: u8 = PACKET_TYPE << 4;
        fixed_header |= (self.dup as u8) << 3;
        fixed_header |= (self.qos_level as u8) << 1;
        fixed_header |= self.retain as u8;
        fixed_header.encode(buffer);

        remaining_len += self.topic_name.encoded_size();
        remaining_len += self.packet_id.encoded_size();
        remaining_len += VariableByteInteger(self.properties.encoded_size() as u32).encoded_size();
        remaining_len += self.properties.encoded_size();

        if let Some(payload) = &self.payload {
            remaining_len += payload.len();
        }

        VariableByteInteger(remaining_len as u32).encode(buffer);

        // Variable header
        self.topic_name.encode(buffer);
        self.packet_id.encode(buffer);
        VariableByteInteger(self.properties.encoded_size() as u32).encode(buffer);
        self.properties.encode(buffer);

        // Payload. Here it goes raw, shouldn't be encoded
        if let Some(payload) = &self.payload {
            buffer.extend(payload);
        }
    }
}

impl Decoder for PublishPacket {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        // Fixed header
        let fixed_header = buffer.get_u8();
        let dup = (fixed_header & 0b0000_1000) != 0;
        let qos_level = QoS::from((fixed_header & 0b0000_0110) >> 1);
        let retain = (fixed_header & 0b0000_0001) != 0;
        let remaining_len = VariableByteInteger::decode(buffer)?.0 as usize;

        // Variable header
        let topic_name = String::decode(buffer)?;
        let packet_id = match qos_level {
            QoS::AtMostOnce => None,
            QoS::Invalid => return Err(ReasonCode::MalformedPacket.into()),
            _ => Some(u16::decode(buffer)?),
        };

        let properties = Some(PublishProperties::decode(buffer)?);

        // Payload
        let payload_len = remaining_len
            - (topic_name.encoded_size()
                + packet_id.encoded_size()
                + properties.encoded_size()
                + VariableByteInteger(properties.encoded_size() as u32).encoded_size());

        if buffer.remaining() < payload_len {
            return Err(ReasonCode::MalformedPacket.into());
        }

        let payload = Some(buffer.copy_to_bytes(payload_len));

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
        packet.encode(&mut encoded);

        assert_eq!(encoded, expected);

        let mut bytes = Bytes::from(expected);

        let new_packet = PublishPacket::decode(&mut bytes).expect("Unexpected error");
        assert_eq!(packet, new_packet);
    }
}
