//! Column encodings shared by the SQL backends.

use bytes::BytesMut;
use mercurio_core::{codec::Encoder, message::MessageProperties, qos::QoS};

use crate::{StorageError, StoredSubscription};

pub(crate) fn qos_to_int(qos: QoS) -> i32 {
    match qos {
        QoS::AtMostOnce => 0,
        QoS::AtLeastOnce => 1,
        QoS::ExactlyOnce => 2,
    }
}

/// A stored QoS column that holds anything but 0, 1 or 2.
#[derive(Debug)]
pub(crate) struct InvalidQoS(i32);

impl std::fmt::Display for InvalidQoS {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid stored QoS {}", self.0)
    }
}

impl std::error::Error for InvalidQoS {}

impl From<InvalidQoS> for StorageError {
    fn from(e: InvalidQoS) -> Self {
        StorageError::SerializationError(e.to_string())
    }
}

/// Fails like any other unreadable column.
#[cfg(feature = "sqlite")]
impl From<InvalidQoS> for rusqlite::Error {
    fn from(e: InvalidQoS) -> Self {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Integer, Box::new(e))
    }
}

pub(crate) fn int_to_qos(val: i32) -> Result<QoS, InvalidQoS> {
    u8::try_from(val)
        .ok()
        .and_then(|n| QoS::try_from(n).ok())
        .ok_or(InvalidQoS(val))
}

pub(crate) fn encode_properties(properties: &MessageProperties) -> Vec<u8> {
    let mut buffer = BytesMut::new();
    properties.encode(&mut buffer);
    buffer.to_vec()
}

/// How sessions kept their subscriptions before they had a table of their
/// own: each subscription as three digits (QoS, No Local, Retain As
/// Published), the subscription identifier if any, `:` and the topic
/// filter. Subscriptions are separated by NUL: the only character a topic
/// filter cannot contain. Databases are moved off it as they are opened.
#[cfg(test)]
pub(crate) fn encode_subscriptions(subscriptions: &[StoredSubscription]) -> String {
    subscriptions
        .iter()
        .map(|s| {
            let identifier = s
                .subscription_identifier
                .map(|id| id.to_string())
                .unwrap_or_default();
            format!(
                "{}{}{}{}:{}",
                s.qos as u8,
                s.no_local as u8,
                s.retain_as_published as u8,
                identifier,
                s.topic_filter
            )
        })
        .collect::<Vec<_>>()
        .join("\0")
}

pub(crate) fn decode_subscriptions(encoded: &str) -> Vec<StoredSubscription> {
    encoded
        .split('\0')
        .filter_map(|s| {
            let (flags, rest) = s.split_at_checked(3)?;
            let (identifier, topic_filter) = rest.split_once(':')?;
            let flags: Vec<u8> = flags.bytes().map(|b| b.wrapping_sub(b'0')).collect();
            Some(StoredSubscription {
                topic_filter: topic_filter.to_string(),
                qos: QoS::try_from(flags[0]).ok()?,
                no_local: flags[1] == 1,
                retain_as_published: flags[2] == 1,
                subscription_identifier: identifier.parse().ok(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stored_qos_outside_0_to_2_is_an_error() {
        for qos in [QoS::AtMostOnce, QoS::AtLeastOnce, QoS::ExactlyOnce] {
            assert_eq!(int_to_qos(qos_to_int(qos)).unwrap(), qos);
        }
        for stored in [-1, 3, 256] {
            assert!(int_to_qos(stored).is_err(), "{stored}");
        }
    }

    #[test]
    fn test_legacy_subscription_with_a_bad_qos_is_skipped() {
        let decoded = decode_subscriptions("100:a\u{0}300:b");
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].topic_filter, "a");
    }
}
