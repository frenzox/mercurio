//! Conversions between the plugin interface's types and the server's.

use bytes::Bytes;
use mercurio_core::{
    codec::Decoder,
    message::MessageProperties,
    properties::{
        ContentType, CorrelationData, MessageExpiryInterval, PayloadFormatIndicator, ResponseTopic,
        UserProperty,
    },
    qos::QoS,
    reason::ReasonCode,
};
use mercurio_server::plugin::{Access, AuthResult, ClientInfo, Decision, Event, PluginMessage};

use super::runtime::bindings::mercurio::plugin::types as wit;

pub(crate) fn client(client: &ClientInfo) -> wit::ClientInfo {
    wit::ClientInfo {
        client_id: client.client_id.clone(),
        username: client.username.clone(),
        peer: client.peer.map(|peer| peer.to_string()),
        connection: client.connection.as_deref().map(|c| wit::Connection {
            listener: c.listener.to_string(),
            protocol_version: c.protocol_version.protocol_level(),
            tls: c.tls,
            certificate: c.certificate.clone(),
            mount_point: c.mount_point.clone(),
        }),
    }
}

/// None for an access the plugin interface has no counterpart for.
pub(crate) fn access(access: Access) -> Option<wit::Access> {
    match access {
        Access::Read => Some(wit::Access::Read),
        Access::Write => Some(wit::Access::Write),
        Access::Subscribe => Some(wit::Access::Subscribe),
        _ => None,
    }
}

pub(crate) fn decision(decision: wit::Decision) -> Decision {
    match decision {
        wit::Decision::Allow => Decision::Allow,
        wit::Decision::Deny => Decision::Deny,
        wit::Decision::Defer => Decision::Defer,
    }
}

fn qos(qos: QoS) -> wit::Qos {
    match qos {
        QoS::AtLeastOnce => wit::Qos::AtLeastOnce,
        QoS::ExactlyOnce => wit::Qos::ExactlyOnce,
        QoS::AtMostOnce => wit::Qos::AtMostOnce,
    }
}

fn qos_from(qos: wit::Qos) -> QoS {
    match qos {
        wit::Qos::AtMostOnce => QoS::AtMostOnce,
        wit::Qos::AtLeastOnce => QoS::AtLeastOnce,
        wit::Qos::ExactlyOnce => QoS::ExactlyOnce,
    }
}

pub(crate) fn message(message: &PluginMessage) -> wit::Message {
    let properties = &message.properties;
    wit::Message {
        topic: message.topic.clone(),
        payload: message.payload.to_vec(),
        qos: qos(message.qos),
        retain: message.retain,
        payload_is_utf8: properties
            .payload_format_indicator
            .as_ref()
            .is_some_and(|indicator| indicator.value == 1),
        message_expiry: properties
            .message_expiry_interval
            .as_ref()
            .map(|interval| interval.value),
        content_type: properties.content_type.as_ref().map(|p| p.value.clone()),
        response_topic: properties.response_topic.as_ref().map(|p| p.value.clone()),
        correlation_data: properties
            .correlation_data
            .as_ref()
            .map(|p| p.value.to_vec()),
        user_properties: properties
            .user_property
            .iter()
            .flatten()
            .map(|p| (p.key.clone(), p.value.clone()))
            .collect(),
    }
}

pub(crate) fn plugin_message(message: wit::Message) -> PluginMessage {
    let properties = MessageProperties {
        payload_format_indicator: message
            .payload_is_utf8
            .then(|| PayloadFormatIndicator::new(1)),
        message_expiry_interval: message.message_expiry.map(MessageExpiryInterval::new),
        content_type: message.content_type.map(ContentType::new),
        response_topic: message.response_topic.map(ResponseTopic::new),
        correlation_data: message
            .correlation_data
            .map(|data| CorrelationData::new(data.into())),
        user_property: (!message.user_properties.is_empty()).then(|| {
            message
                .user_properties
                .into_iter()
                .map(|(key, value)| UserProperty::new(key, value))
                .collect()
        }),
    };
    PluginMessage::new(
        message.topic,
        Bytes::from(message.payload),
        qos_from(message.qos),
        message.retain,
        properties,
    )
}

/// None for an event the plugin interface has no counterpart for.
pub(crate) fn event(event: &Event) -> Option<wit::Event> {
    Some(match event {
        Event::Connected { client: c, .. } => wit::Event::Connected(client(c)),
        Event::Disconnected {
            client: c, reason, ..
        } => wit::Event::Disconnected(wit::Disconnected {
            client: client(c),
            reason: reason.map(|reason| reason.code()),
        }),
        Event::Subscribed {
            client: c,
            filter,
            qos: q,
            ..
        } => wit::Event::Subscribed(wit::Subscribed {
            client: client(c),
            filter: filter.clone(),
            qos: qos(*q),
        }),
        Event::Unsubscribed {
            client: c, filter, ..
        } => wit::Event::Unsubscribed(wit::Unsubscribed {
            client: client(c),
            filter: filter.clone(),
        }),
        Event::SessionExpired {
            client_id,
            mount_point,
            ..
        } => wit::Event::SessionExpired(wit::SessionExpired {
            client_id: client_id.clone(),
            mount_point: mount_point.clone(),
        }),
        _ => return None,
    })
}

/// The server's result of an enhanced authentication step, and the state
/// to pass to the next step when there is one.
pub(crate) fn auth_result(result: wit::AuthResult) -> (AuthResult, Option<Vec<u8>>) {
    match result {
        wit::AuthResult::Continue(challenge) => (
            AuthResult::Continue(challenge.data.into()),
            Some(challenge.state),
        ),
        wit::AuthResult::Success(success) => (
            AuthResult::Success {
                username: success.username,
                data: success.data.map(Bytes::from),
            },
            None,
        ),
        wit::AuthResult::Failed(failed) => (
            AuthResult::Failed {
                reason: failure(failed.reason),
                reason_string: failed.reason_string,
            },
            None,
        ),
    }
}

/// The reason code a plugin failed authentication with. A code that isn't
/// an error (below 0x80) or that MQTT doesn't define becomes Not
/// authorized.
fn failure(code: u8) -> ReasonCode {
    match ReasonCode::decode(&mut &[code][..]) {
        Ok(reason) if code >= 0x80 => reason,
        _ => ReasonCode::NotAuthorized,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_messages_keep_every_property_both_ways() {
        let original = PluginMessage::new(
            "a/b",
            Bytes::from_static(b"payload"),
            QoS::ExactlyOnce,
            true,
            MessageProperties {
                payload_format_indicator: Some(PayloadFormatIndicator::new(1)),
                message_expiry_interval: Some(MessageExpiryInterval::new(60)),
                content_type: Some(ContentType::new("text/plain".into())),
                response_topic: Some(ResponseTopic::new("reply/here".into())),
                correlation_data: Some(CorrelationData::new(Bytes::from_static(b"id"))),
                user_property: Some(vec![
                    UserProperty::new("k".into(), "v".into()),
                    UserProperty::new("k".into(), "w".into()),
                ]),
            },
        );
        let converted = message(&original);
        assert!(converted.payload_is_utf8);
        assert_eq!(converted.user_properties.len(), 2);
        assert_eq!(plugin_message(converted), original);
    }

    #[test]
    fn test_a_message_without_properties_stays_without() {
        let original = PluginMessage::new(
            "t",
            Bytes::new(),
            QoS::AtMostOnce,
            false,
            MessageProperties::default(),
        );
        assert_eq!(plugin_message(message(&original)), original);
    }

    #[test]
    fn test_client_info_carries_the_peer_when_known() {
        let mut info = ClientInfo::new("c", Some("alice".into()));
        let converted = client(&info);
        assert_eq!(converted.client_id, "c");
        assert_eq!(converted.username.as_deref(), Some("alice"));
        assert_eq!(converted.peer, None);
        info.peer = Some("10.0.0.1:5555".parse().unwrap());
        assert_eq!(client(&info).peer.as_deref(), Some("10.0.0.1:5555"));
    }

    #[test]
    fn test_qos_converts_both_ways() {
        for q in [QoS::AtMostOnce, QoS::AtLeastOnce, QoS::ExactlyOnce] {
            assert_eq!(qos_from(qos(q)), q);
        }
    }

    #[test]
    fn test_failed_authentication_codes() {
        assert_eq!(failure(0x8c), ReasonCode::BadAuthenticationMethod);
        assert_eq!(failure(0x00), ReasonCode::NotAuthorized);
        assert_eq!(failure(0xff), ReasonCode::NotAuthorized);
        let (result, state) = auth_result(wit::AuthResult::Continue(wit::Challenge {
            data: b"nonce".to_vec(),
            state: b"s".to_vec(),
        }));
        assert_eq!(result, AuthResult::Continue(Bytes::from_static(b"nonce")));
        assert_eq!(state.as_deref(), Some(&b"s"[..]));
    }

    #[test]
    fn test_failed_authentication_keeps_its_reason_string() {
        let (result, state) = auth_result(wit::AuthResult::Failed(wit::Failure {
            reason: 0x87,
            reason_string: Some("token expired".into()),
        }));
        assert_eq!(
            result,
            AuthResult::Failed {
                reason: ReasonCode::NotAuthorized,
                reason_string: Some("token expired".into()),
            }
        );
        assert_eq!(state, None);
    }
}
