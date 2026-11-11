//! Column encodings shared by the SQL backends.

use bytes::BytesMut;
use mercurio_core::{codec::Encoder, error::EncodeError, message::MessageProperties, qos::QoS};

use crate::{Result, StorageError, StoredSubscription};

pub(crate) fn qos_to_int(qos: QoS) -> i32 {
    match qos {
        QoS::AtMostOnce => 0,
        QoS::AtLeastOnce => 1,
        QoS::ExactlyOnce => 2,
    }
}

/// Fails for a value no version stores, rather than read it as a QoS the
/// broker would then have to refuse.
pub(crate) fn int_to_qos(val: i32) -> Result<QoS> {
    match val {
        0 => Ok(QoS::AtMostOnce),
        1 => Ok(QoS::AtLeastOnce),
        2 => Ok(QoS::ExactlyOnce),
        _ => Err(StorageError::SerializationError(format!(
            "{val} is not a QoS"
        ))),
    }
}

/// A message's properties as a column value. Properties MQTT can't carry
/// fail the statement, like any other value the database refuses.
#[derive(Debug)]
pub(crate) struct EncodedProperties(std::result::Result<Vec<u8>, EncodeError>);

pub(crate) fn encode_properties(properties: &MessageProperties) -> EncodedProperties {
    let mut buffer = BytesMut::new();
    EncodedProperties(properties.encode(&mut buffer).map(|()| buffer.to_vec()))
}

#[cfg(feature = "sqlite")]
impl rusqlite::ToSql for EncodedProperties {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        match &self.0 {
            Ok(bytes) => bytes.to_sql(),
            Err(e) => Err(rusqlite::Error::ToSqlConversionFailure(Box::new(*e))),
        }
    }
}

#[cfg(feature = "postgres")]
impl tokio_postgres::types::ToSql for EncodedProperties {
    fn to_sql(
        &self,
        ty: &tokio_postgres::types::Type,
        out: &mut BytesMut,
    ) -> std::result::Result<tokio_postgres::types::IsNull, Box<dyn std::error::Error + Sync + Send>>
    {
        match &self.0 {
            Ok(bytes) => bytes.to_sql(ty, out),
            Err(e) => Err(Box::new(*e)),
        }
    }

    fn accepts(ty: &tokio_postgres::types::Type) -> bool {
        <Vec<u8> as tokio_postgres::types::ToSql>::accepts(ty)
    }

    tokio_postgres::types::to_sql_checked!();
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

/// Fails for subscriptions that can't be read whole, as every other
/// stored row does, rather than leave them out.
pub(crate) fn decode_subscriptions(encoded: &str) -> Result<Vec<StoredSubscription>> {
    encoded
        .split('\0')
        .map(|s| {
            let invalid =
                || StorageError::SerializationError(format!("invalid stored subscription `{s}`"));
            let (flags, rest) = s.split_at_checked(3).ok_or_else(invalid)?;
            let (identifier, topic_filter) = rest.split_once(':').ok_or_else(invalid)?;
            let flag = |index: usize| match flags.as_bytes()[index] {
                b'0' => Ok(false),
                b'1' => Ok(true),
                _ => Err(invalid()),
            };
            let qos = flags.as_bytes()[0].wrapping_sub(b'0');
            Ok(StoredSubscription {
                topic_filter: topic_filter.to_string(),
                qos: int_to_qos(i32::from(qos)).map_err(|_| invalid())?,
                no_local: flag(1)?,
                retain_as_published: flag(2)?,
                subscription_identifier: match identifier {
                    "" => None,
                    identifier => Some(identifier.parse().map_err(|_| invalid())?),
                },
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_subscriptions_that_cannot_be_read_are_an_error() {
        let subscriptions = vec![
            StoredSubscription {
                no_local: true,
                retain_as_published: false,
                subscription_identifier: Some(7),
                ..StoredSubscription::new("a/+:x", QoS::ExactlyOnce)
            },
            StoredSubscription::new("b", QoS::AtMostOnce),
        ];
        let encoded = encode_subscriptions(&subscriptions);
        assert_eq!(decode_subscriptions(&encoded).unwrap(), subscriptions);

        for bad in [
            "9007:a",
            "1x0:a",
            "100",
            "100x:a",
            &format!("{encoded}\0junk"),
        ] {
            assert!(
                matches!(
                    decode_subscriptions(bad),
                    Err(StorageError::SerializationError(_))
                ),
                "{bad:?}"
            );
        }
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn test_properties_mqtt_cannot_carry_fail_the_statement() {
        use mercurio_core::properties::ResponseTopic;
        use rusqlite::ToSql;

        let properties = MessageProperties {
            response_topic: Some(ResponseTopic::new("a/#".into())),
            ..Default::default()
        };
        assert!(encode_properties(&properties).to_sql().is_err());
        assert!(encode_properties(&MessageProperties::default())
            .to_sql()
            .is_ok());
    }

    #[test]
    fn test_unknown_qos_values_are_an_error() {
        assert_eq!(int_to_qos(2).unwrap(), QoS::ExactlyOnce);
        assert!(int_to_qos(3).is_err());
        assert!(int_to_qos(-1).is_err());
    }
}
