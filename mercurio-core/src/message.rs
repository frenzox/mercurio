//! MQTT message types for internal routing.

#[cfg(not(feature = "std"))]
use alloc::{sync::Arc, vec, vec::Vec};
#[cfg(feature = "std")]
use std::sync::Arc;

use bytes::{Buf, Bytes, BytesMut};

use crate::{
    codec::{Decoder, Encoder},
    properties::*,
    qos::QoS,
    reason::ReasonCode,
};

/// MQTT message for internal routing.
///
/// This struct is designed to be cheap to clone:
/// - `topic` uses `Arc<str>` for O(1) reference-counted cloning
/// - `payload` uses `Bytes` which is also reference-counted
#[derive(Clone, Debug)]
pub struct Message {
    pub packet_id: Option<u16>,
    /// Topic name - uses Arc<str> for cheap cloning when broadcasting to multiple subscribers
    pub topic: Arc<str>,
    pub dup: bool,
    pub qos: QoS,
    pub retain: bool,
    /// Payload data - Bytes is already reference-counted for cheap cloning
    pub payload: Option<Bytes>,
    pub properties: MessageProperties,
    /// Unix time in milliseconds after which the message must no longer be
    /// delivered, from its Message Expiry Interval
    pub expires_at: Option<u64>,
    /// Set per delivery: the identifier of the subscription it matched
    pub subscription_identifier: Option<u32>,
}

#[cfg(feature = "std")]
impl Message {
    /// Deadline for a message received now with these properties.
    pub fn expiry_deadline(properties: &MessageProperties) -> Option<u64> {
        properties
            .message_expiry_interval
            .as_ref()
            .map(|interval| unix_millis() + u64::from(interval.value) * 1000)
    }

    pub fn is_expired(&self) -> bool {
        self.expires_at
            .is_some_and(|deadline| unix_millis() >= deadline)
    }
}

/// Current Unix time in milliseconds.
#[cfg(feature = "std")]
pub fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// MQTT 5 properties that belong to the application message and are
/// forwarded unchanged from the publisher to every subscriber.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MessageProperties {
    pub payload_format_indicator: Option<PayloadFormatIndicator>,
    pub message_expiry_interval: Option<MessageExpiryInterval>,
    pub content_type: Option<ContentType>,
    pub response_topic: Option<ResponseTopic>,
    pub correlation_data: Option<CorrelationData>,
    pub user_property: Option<Vec<UserProperty>>,
}

/// Encoded as a bare property list, without the length prefix packets use.
impl Encoder for MessageProperties {
    fn encode(&self, buffer: &mut BytesMut) {
        self.payload_format_indicator.encode(buffer);
        self.message_expiry_interval.encode(buffer);
        self.content_type.encode(buffer);
        self.response_topic.encode(buffer);
        self.correlation_data.encode(buffer);
        self.user_property.encode(buffer);
    }

    fn encoded_size(&self) -> usize {
        self.payload_format_indicator.encoded_size()
            + self.message_expiry_interval.encoded_size()
            + self.content_type.encoded_size()
            + self.response_topic.encoded_size()
            + self.correlation_data.encoded_size()
            + self.user_property.encoded_size()
    }
}

/// Decodes a bare property list until the buffer is exhausted.
impl Decoder for MessageProperties {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        let mut properties = MessageProperties::default();

        while buffer.has_remaining() {
            match Property::decode(buffer)? {
                Property::PayloadFormatIndicator(v) => {
                    set_once(&mut properties.payload_format_indicator, v)?
                }
                Property::MessageExpiryInterval(v) => {
                    set_once(&mut properties.message_expiry_interval, v)?
                }
                Property::ContentType(v) => set_once(&mut properties.content_type, v)?,
                Property::ResponseTopic(v) => set_once(&mut properties.response_topic, v)?,
                Property::CorrelationData(v) => set_once(&mut properties.correlation_data, v)?,
                Property::UserProperty(v) => match &mut properties.user_property {
                    Some(user_properties) => user_properties.push(v),
                    None => properties.user_property = Some(vec![v]),
                },
                _ => return Err(ReasonCode::MalformedPacket.into()),
            }
        }

        Ok(properties)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_properties_round_trip() {
        let properties = MessageProperties {
            payload_format_indicator: Some(PayloadFormatIndicator::new(1)),
            response_topic: Some(ResponseTopic::new("reply".into())),
            correlation_data: Some(CorrelationData::new(Bytes::from_static(b"id"))),
            user_property: Some(vec![
                UserProperty::new("a".into(), "1".into()),
                UserProperty::new("a".into(), "2".into()),
            ]),
            ..Default::default()
        };

        let mut buffer = BytesMut::new();
        properties.encode(&mut buffer);
        assert_eq!(buffer.len(), properties.encoded_size());
        assert_eq!(MessageProperties::decode(&mut buffer).unwrap(), properties);
    }
}
