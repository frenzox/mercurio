use crate::control_packet::*;
use crate::endec::{Decoder, Encoder, VariableByteInteger};
use crate::properties::*;
use crate::qos::QoS;
use crate::reason::ReasonCode;
use bytes::{Buf, Bytes, BytesMut};

#[derive(Default, Debug, PartialEq)]
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

        if let Some(props) = &self.user_property {
            for property in props {
                property.encode(buffer);
            }
        }

        self.subscription_identifier.encode(buffer);
        self.content_type.encode(buffer);
    }

    fn get_encoded_size(&self) -> usize {
        let mut len = 0;

        len += self.payload_format_indicator.get_encoded_size();
        len += self.message_expiry_interval.get_encoded_size();
        len += self.topic_alias.get_encoded_size();
        len += self.response_topic.get_encoded_size();
        len += self.correlation_data.get_encoded_size();

        if let Some(props) = &self.user_property {
            for property in props {
                property.get_encoded_size();
            }
        }

        len += self.subscription_identifier.get_encoded_size();
        len += self.content_type.get_encoded_size();
        len
    }
}

impl Decoder for PublishProperties {
    fn decode<T: Buf>(buffer: &mut T) -> Result<Option<Self>, ReasonCode> {
        let len = VariableByteInteger::decode(buffer)?.unwrap();
        if len.0 == 0 {
            return Ok(None);
        } else if (buffer.remaining() as u32) < len.0 {
            return Err(ReasonCode::MalformedPacket);
        }

        let mut encoded_properties = buffer.take(len.0 as usize);
        let mut publish_properties = PublishProperties::default();

        loop {
            let p = Property::decode(&mut encoded_properties)?.unwrap();

            match p {
                Property::PayloadFormatIndicator => {
                    publish_properties.payload_format_indicator =
                        PayloadFormatIndicator::decode(&mut encoded_properties)?
                }

                Property::MessageExpiryInterval => {
                    publish_properties.message_expiry_interval =
                        MessageExpiryInterval::decode(&mut encoded_properties)?
                }

                Property::TopicAlias => {
                    publish_properties.topic_alias = TopicAlias::decode(&mut encoded_properties)?
                }

                Property::ResponseTopic => {
                    publish_properties.response_topic =
                        ResponseTopic::decode(&mut encoded_properties)?
                }

                Property::CorrelationData => {
                    publish_properties.correlation_data =
                        CorrelationData::decode(&mut encoded_properties)?
                }

                Property::UserProperty => {
                    let user_property = UserProperty::decode(&mut encoded_properties)?.unwrap();

                    if let Some(v) = &mut publish_properties.user_property {
                        v.push(user_property);
                    } else {
                        let v = vec![user_property];
                        publish_properties.user_property = Some(v);
                    }
                }

                Property::SubscriptionIdentifier => {
                    publish_properties.subscription_identifier =
                        SubscriptionIdentifier::decode(&mut encoded_properties)?
                }

                Property::ContentType => {
                    publish_properties.content_type = ContentType::decode(&mut encoded_properties)?
                }

                _ => return Err(ReasonCode::MalformedPacket),
            }

            if !encoded_properties.has_remaining() {
                break;
            }
        }

        Ok(Some(publish_properties))
    }
}

#[derive(Default, Debug, PartialEq)]
pub struct PublishPacket {
    dup: bool,
    qos_level: QoS,
    retain: bool,
    topic_name: String,
    packet_id: Option<u16>,
    properties: PublishProperties,
    payload: Option<Bytes>,
}

impl ControlPacket for PublishPacket {
    const PACKET_TYPE: ControlPacketType = ControlPacketType::Publish;
}

impl Encoder for PublishPacket {
    fn encode(&self, buffer: &mut BytesMut) {
        let mut fixed_header: u8;
        let mut remaining_len = 0;

        // Fixed header
        fixed_header = (Self::PACKET_TYPE as u8) << 4;
        fixed_header |= (self.dup as u8) << 3;
        fixed_header |= (self.qos_level as u8) << 2;
        fixed_header |= self.retain as u8;
        fixed_header.encode(buffer);

        remaining_len += self.topic_name.get_encoded_size();
        remaining_len += self.packet_id.get_encoded_size();
        remaining_len +=
            VariableByteInteger(self.properties.get_encoded_size() as u32).get_encoded_size();
        remaining_len += self.properties.get_encoded_size();
        remaining_len += self.payload.get_encoded_size();
        VariableByteInteger(remaining_len as u32).encode(buffer);

        // Variable header
        self.topic_name.encode(buffer);
        self.packet_id.encode(buffer);
        VariableByteInteger(self.properties.get_encoded_size() as u32).encode(buffer);
        self.properties.encode(buffer);

        // Payload. Here it goes raw, shouldn't be encoded
        if let Some(payload) = &self.payload {
            buffer.extend(payload);
        }
    }
}

impl Decoder for PublishPacket {
    fn decode<T: Buf>(buffer: &mut T) -> Result<Option<Self>, ReasonCode> {
        // Fixed header
        let fixed_header = buffer.get_u8();
        let dup = (fixed_header & 0b0000_1000) != 0;
        let qos_level = QoS::from((fixed_header & 0b0000_0110) >> 2);
        let retain = (fixed_header & 0b0000_0001) != 0;
        let remaining_len = VariableByteInteger::decode(buffer)?.unwrap().0 as usize;

        // Variable header
        let topic_name = String::decode(buffer)?.unwrap();
        let packet_id = u16::decode(buffer)?;
        let properties = PublishProperties::decode(buffer)?.unwrap();

        // Payload
        let payload_len = remaining_len
            - (topic_name.get_encoded_size()
                + packet_id.get_encoded_size()
                + properties.get_encoded_size());
        if buffer.remaining() != payload_len {
            return Err(ReasonCode::MalformedPacket);
        }
        let payload = Some(buffer.copy_to_bytes(payload_len));

        Ok(Some(PublishPacket {
            dup,
            qos_level,
            retain,
            topic_name,
            packet_id,
            properties,
            payload,
        }))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_publish_packet_encode_decode() {}
}
