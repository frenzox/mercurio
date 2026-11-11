//! MQTT message types for internal routing.

#[cfg(not(feature = "std"))]
use alloc::{string::String, sync::Arc, vec::Vec};
#[cfg(feature = "std")]
use std::sync::Arc;

use bytes::{Buf, Bytes, BytesMut};

use crate::{
    codec::{Decoder, Encoder},
    error::EncodeError,
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
    /// Packet identifier of the PUBLISH the message arrived in. `None` at
    /// QoS 0 and for messages the broker creates itself, such as will
    /// messages. Each delivery to a subscriber uses its own identifier.
    pub packet_id: Option<u16>,
    /// Topic name - uses `Arc<str>` for cheap cloning when broadcasting to multiple subscribers
    pub topic: Arc<str>,
    /// DUP flag of the PUBLISH the message arrived in: the sender may have
    /// sent it before (section 3.3.1.1).
    pub dup: bool,
    /// The publisher's QoS. The server adjusts it per delivery to the QoS
    /// granted to the matching subscription [MQTT-3.8.4-8].
    pub qos: QoS,
    pub retain: bool,
    /// Payload data - Bytes is already reference-counted for cheap cloning
    pub payload: Option<Bytes>,
    pub properties: MessageProperties,
    /// Unix time in milliseconds after which the message must no longer be
    /// delivered, from its Message Expiry Interval
    pub expires_at: Option<u64>,
    /// Set per delivery: the identifiers of the subscriptions it matched
    pub subscription_identifiers: Vec<u32>,
    /// The client that published it, when it came from a client
    pub origin: Option<Arc<MessageOrigin>>,
    /// Set per delivery: the shared subscription, `$share/{name}/{filter}`,
    /// the message reached the session through, so it can go to another
    /// member of the group when it can't reach this one
    pub share_group: Option<Arc<str>>,
}

/// Who published a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageOrigin {
    pub client_id: String,
    /// The user name the client verified, if any
    pub username: Option<String>,
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

    /// Whether `expires_at` has passed. A message without a deadline never
    /// expires.
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
    fn encode(&self, buffer: &mut BytesMut) -> Result<(), EncodeError> {
        self.payload_format_indicator.encode(buffer)?;
        self.message_expiry_interval.encode(buffer)?;
        self.content_type.encode(buffer)?;
        self.response_topic.encode(buffer)?;
        self.correlation_data.encode(buffer)?;
        self.user_property.encode(buffer)
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

        decode_properties(buffer, |property| match property {
            Property::PayloadFormatIndicator(v) => {
                set_once(&mut properties.payload_format_indicator, v)
            }
            Property::MessageExpiryInterval(v) => {
                set_once(&mut properties.message_expiry_interval, v)
            }
            Property::ContentType(v) => set_once(&mut properties.content_type, v),
            Property::ResponseTopic(v) => set_once(&mut properties.response_topic, v),
            Property::CorrelationData(v) => set_once(&mut properties.correlation_data, v),
            Property::UserProperty(v) => {
                push_user_property(&mut properties.user_property, v);
                Ok(())
            }
            _ => Err(ReasonCode::MalformedPacket.into()),
        })?;

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
        properties.encode(&mut buffer).unwrap();
        assert_eq!(buffer.len(), properties.encoded_size());
        assert_eq!(MessageProperties::decode(&mut buffer).unwrap(), properties);
    }
}
