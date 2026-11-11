//! Column encodings shared by the SQL backends.

use bytes::BytesMut;
use mercurio_core::{codec::Encoder, message::MessageProperties, qos::QoS};

use crate::StoredSubscription;

pub(crate) fn qos_to_int(qos: QoS) -> i32 {
    match qos {
        QoS::AtMostOnce => 0,
        QoS::AtLeastOnce => 1,
        QoS::ExactlyOnce => 2,
        QoS::Invalid => -1,
    }
}

pub(crate) fn int_to_qos(val: i32) -> QoS {
    match val {
        0 => QoS::AtMostOnce,
        1 => QoS::AtLeastOnce,
        2 => QoS::ExactlyOnce,
        _ => QoS::Invalid,
    }
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
                qos: flags[0].into(),
                no_local: flags[1] == 1,
                retain_as_published: flags[2] == 1,
                subscription_identifier: identifier.parse().ok(),
            })
        })
        .collect()
}
