//! What the client publishes: topic alias resolution, capability and access
//! checks, the plugins' message hook, `$CONTROL` requests, and exactly-once
//! delivery of QoS 2 messages until PUBREL.

use std::sync::Arc;

use mercurio_core::{
    message::{Message, MessageOrigin, MessageProperties},
    qos::QoS,
    reason::ReasonCode,
    topic::validate_publish_topic,
};
use mercurio_packets::{
    puback::PubAckPacket, pubcomp::PubCompPacket, publish::PublishPacket, pubrec::PubRecPacket,
    pubrel::PubRelPacket, ControlPacket,
};
use mercurio_storage::RetainedMessageStore;
use tracing::warn;

use super::{message_properties, Session};
use crate::{
    acl::Access,
    broker::Broker,
    error::{Error, Result},
    plugin::{is_control, Chain, MessageAction, PluginMessage},
};

/// Ends the connection of a client whose PUBLISH couldn't be stored, such
/// as a retained message, without acknowledging it, so the client sends it
/// again rather than lose it.
fn storage_failure(error: Error) -> Error {
    warn!("Failed to store a published message: {}", error);
    ReasonCode::ImplementationSpecificError.into()
}

impl Session {
    pub(crate) async fn handle_publish<S: RetainedMessageStore>(
        &mut self,
        mut packet: PublishPacket,
        broker: &Broker<S>,
        chain: &Chain,
    ) -> Result<Option<ControlPacket>> {
        let topic_alias = packet
            .properties
            .as_ref()
            .and_then(|p| p.topic_alias.as_ref())
            .map(|alias| alias.value);
        if let Some(alias) = topic_alias {
            let mut session = self.shared.state.lock().await;
            if alias == 0 || alias > session.topic_alias_maximum {
                return Err(ReasonCode::TopicAliasInvalid.into());
            }
            // An empty topic name refers to the alias, a non-empty one sets it
            if packet.topic_name.is_empty() {
                match session.topic_aliases.get(&alias) {
                    Some(topic) => packet.topic_name = topic.clone(),
                    None => return Err(ReasonCode::ProtocolError.into()),
                }
            } else {
                session
                    .topic_aliases
                    .insert(alias, packet.topic_name.clone());
            }
        }

        let max_retained = {
            let session = self.shared.state.lock().await;
            // [MQTT-3.2.2-11]
            if packet.qos_level as u8 > session.maximum_qos as u8 {
                return Err(ReasonCode::QoSNotSupported.into());
            }
            // [MQTT-3.2.2-14]
            if packet.retain && !session.retain_available {
                return Err(ReasonCode::RetainNotSupported.into());
            }
            session.max_retained_messages
        };

        // Validate topic name (no wildcards, no null chars, etc.)
        if validate_publish_topic(&packet.topic_name).is_err() {
            return Err(ReasonCode::TopicNameInvalid.into());
        }

        // $SYS topics carry the broker's own statistics
        let sys = packet.topic_name == "$SYS" || packet.topic_name.starts_with("$SYS/");
        if sys && !chain.is_trusted() {
            warn!("Client publish to `{}` refused", packet.topic_name);
            return self
                .refuse_publish(&packet, ReasonCode::NotAuthorized)
                .await;
        }

        let control = is_control(&packet.topic_name);
        // A trusted chain allows everything, and an empty one only refuses
        // $CONTROL, so the session lock is taken only when needed
        if control || (!chain.is_empty() && !chain.is_trusted()) {
            let identity = self.identity().await;
            let allowed = match control {
                true => chain.may_control(&identity, &packet.topic_name).await,
                false => {
                    chain
                        .may(&identity, &packet.topic_name, Access::Write)
                        .await
                }
            };
            if !allowed {
                warn!(
                    "Client `{}` may not publish to `{}`",
                    identity.client_id, packet.topic_name
                );
                return self
                    .refuse_publish(&packet, ReasonCode::NotAuthorized)
                    .await;
            }
        }

        // Only the server sets subscription identifiers [MQTT-3.3.4-6]
        if packet
            .properties
            .as_ref()
            .is_some_and(|p| !p.subscription_identifiers.is_empty())
        {
            return Err(ReasonCode::ProtocolError.into());
        }

        // Retaining on a new topic once the store is full; updating or
        // clearing a retained topic is always allowed
        let retains = packet.retain && packet.payload.as_ref().is_some_and(|p| !p.is_empty());
        if let Some(max) = max_retained.filter(|_| retains) {
            let full = broker
                .retained_full(&packet.topic_name, max)
                .await
                .map_err(storage_failure)?;
            if full {
                warn!(
                    "Retained messages full, refusing one on `{}`",
                    packet.topic_name
                );
                return self
                    .refuse_publish(&packet, ReasonCode::QuotaExceeded)
                    .await;
            }
        }

        // A payload declared as UTF-8 text has to be valid UTF-8; QoS 0 has
        // no acknowledgement to say so, so it costs the connection
        let declares_text = packet
            .properties
            .as_ref()
            .and_then(|p| p.payload_format_indicator.as_ref())
            .is_some_and(|f| f.value == 1);
        if declares_text
            && packet
                .payload
                .as_ref()
                .is_some_and(|p| std::str::from_utf8(p).is_err())
        {
            if packet.qos_level == QoS::AtMostOnce {
                return Err(ReasonCode::PayloadFormatInvalid.into());
            }
            return self
                .refuse_publish(&packet, ReasonCode::PayloadFormatInvalid)
                .await;
        }

        let response = match packet.qos_level {
            QoS::AtMostOnce => Ok(None),
            QoS::AtLeastOnce => {
                if let Some(packet_id) = packet.packet_id {
                    Ok(Some(ControlPacket::PubAck(PubAckPacket {
                        packet_id,
                        reason: ReasonCode::Success,
                        properties: None,
                    })))
                } else {
                    Err(ReasonCode::ProtocolError.into())
                }
            }
            QoS::ExactlyOnce => {
                if let Some(packet_id) = packet.packet_id {
                    let session = self.shared.state.lock().await;
                    let pubrec = ControlPacket::PubRec(PubRecPacket {
                        packet_id,
                        reason: ReasonCode::Success,
                        properties: None,
                    });
                    // A retransmission before PUBREL was already delivered
                    // once; forwarding it again would break exactly-once.
                    if session.incoming_qos2.contains(&packet_id) {
                        return Ok(Some(pubrec));
                    }
                    // The client may only have as many QoS 2 messages
                    // awaiting PUBREL as advertised [MQTT-3.3.4-8]
                    if session.incoming_qos2.len() >= usize::from(session.receive_maximum) {
                        return Err(ReasonCode::ReceiveMaximumExceeded.into());
                    }
                    Ok(Some(pubrec))
                } else {
                    Err(ReasonCode::ProtocolError.into())
                }
            }
        };

        // Recorded as delivered only once the broker took the message, so
        // the client sends it again if it didn't
        let new_qos2 = packet
            .packet_id
            .filter(|_| packet.qos_level == QoS::ExactlyOnce && response.is_ok());
        let result = self.forward(packet, broker, chain, response).await;
        if let (Ok(_), Some(packet_id)) = (&result, new_qos2) {
            self.shared
                .state
                .lock()
                .await
                .incoming_qos2
                .insert(packet_id);
        }
        result
    }

    /// Hands a PUBLISH the client may make to the plugins and the broker,
    /// passing `response` on as its acknowledgement.
    async fn forward<S: RetainedMessageStore>(
        &mut self,
        mut packet: PublishPacket,
        broker: &Broker<S>,
        chain: &Chain,
        response: Result<Option<ControlPacket>>,
    ) -> Result<Option<ControlPacket>> {
        let mut hooked_properties = None;
        if !chain.is_empty() {
            // Acknowledged whatever the plugins decide, so a dropped message
            // isn't sent again
            let identity = self.identity().await;
            // Access to `$` topics was checked against the topic the client
            // named, so a plugin may not move a message to another one
            let reserved = packet
                .topic_name
                .starts_with('$')
                .then(|| packet.topic_name.clone());
            let mut message = PluginMessage {
                topic: std::mem::take(&mut packet.topic_name),
                payload: packet.payload.take().unwrap_or_default(),
                qos: packet.qos_level,
                retain: packet.retain,
                properties: message_properties(packet.properties.as_ref()),
            };
            if chain.on_message(&identity, &mut message).await == MessageAction::Drop {
                return response;
            }
            if validate_publish_topic(&message.topic).is_err() {
                warn!(
                    "A plugin set the invalid topic `{}`, dropping the message",
                    message.topic
                );
                return response;
            }
            if message.topic.starts_with('$') && reserved.as_deref() != Some(&message.topic) {
                warn!(
                    "Plugins {} moved a message to the reserved topic `{}`, dropping it",
                    chain.names(),
                    message.topic
                );
                return response;
            }
            packet.topic_name = message.topic;
            packet.payload = (!message.payload.is_empty()).then_some(message.payload);
            packet.qos_level = message.qos;
            packet.retain = message.retain;
            hooked_properties = Some(message.properties);
        }

        if packet.topic_name.starts_with("$CONTROL/") {
            let identity = self.identity().await;
            let payload = packet.payload.clone().unwrap_or_default();
            if let Some(answer) = chain.control(&identity, &packet.topic_name, payload).await {
                if let Some(answer) = answer {
                    let request = hooked_properties
                        .take()
                        .unwrap_or_else(|| message_properties(packet.properties.as_ref()));
                    // MQTT 5 request/response, with a fixed topic for
                    // clients that name none, such as MQTT 3.1.1 ones
                    let topic: Arc<str> = match request.response_topic {
                        Some(topic) => topic.value.into(),
                        None => format!("{}/response", packet.topic_name).into(),
                    };
                    let reply = Message {
                        packet_id: None,
                        topic: Arc::clone(&topic),
                        dup: false,
                        qos: QoS::AtLeastOnce,
                        retain: false,
                        payload: Some(answer),
                        properties: MessageProperties {
                            correlation_data: request.correlation_data,
                            ..Default::default()
                        },
                        expires_at: None,
                        subscription_identifiers: Vec::new(),
                        origin: None,
                        share_group: None,
                    };
                    broker.publish_to(&topic, reply, self.shared.subscriber_id);
                }
                return response;
            }
        }

        // Only retained messages are checked against their publisher later
        let origin = match packet.retain {
            true => {
                let identity = self.identity().await;
                Some(Arc::new(MessageOrigin {
                    client_id: identity.client_id,
                    username: identity.username,
                }))
            }
            false => None,
        };

        // Publish to broker - convert topic to Arc<str> for cheap cloning to subscribers
        let topic: Arc<str> = packet.topic_name.into();
        let properties =
            hooked_properties.unwrap_or_else(|| message_properties(packet.properties.as_ref()));
        let message = Message {
            packet_id: packet.packet_id,
            topic: Arc::clone(&topic),
            dup: packet.dup,
            retain: packet.retain,
            qos: packet.qos_level,
            payload: packet.payload,
            expires_at: Message::expiry_deadline(&properties),
            properties,
            subscription_identifiers: Vec::new(),
            origin,
            share_group: None,
        };

        let subscribers = broker
            .publish_from(&topic, message, Some(self.publisher()))
            .await
            .map_err(storage_failure)?;

        // MQTT 5 publishers learn when nobody received the message
        let reports_reasons = self
            .shared
            .state
            .lock()
            .await
            .client
            .protocol_version
            .supports_reason_codes();
        if subscribers == 0 && reports_reasons {
            return response.map(|ack| {
                ack.map(|ack| match ack {
                    ControlPacket::PubAck(mut puback) => {
                        puback.reason = ReasonCode::NoMatchingSubscribers;
                        ControlPacket::PubAck(puback)
                    }
                    ControlPacket::PubRec(mut pubrec) => {
                        pubrec.reason = ReasonCode::NoMatchingSubscribers;
                        ControlPacket::PubRec(pubrec)
                    }
                    other => other,
                })
            });
        }

        response
    }

    /// Acknowledges a PUBLISH the client was not allowed to make without
    /// forwarding it. MQTT 3.x has no way to report the refusal, so those
    /// clients get a normal acknowledgement.
    async fn refuse_publish(
        &self,
        packet: &PublishPacket,
        reason: ReasonCode,
    ) -> Result<Option<ControlPacket>> {
        let mut session = self.shared.state.lock().await;
        let reports_reasons = session.client.protocol_version.supports_reason_codes();
        let reason = if reports_reasons {
            reason
        } else {
            ReasonCode::Success
        };

        match (packet.qos_level, packet.packet_id) {
            (QoS::AtMostOnce, _) => Ok(None),
            (QoS::AtLeastOnce, Some(packet_id)) => Ok(Some(ControlPacket::PubAck(PubAckPacket {
                packet_id,
                reason,
                properties: None,
            }))),
            (QoS::ExactlyOnce, Some(packet_id)) => {
                // A successful PUBREC still expects the PUBREL exchange
                if !reports_reasons {
                    session.incoming_qos2.insert(packet_id);
                }
                Ok(Some(ControlPacket::PubRec(PubRecPacket {
                    packet_id,
                    reason,
                    properties: None,
                })))
            }
            _ => Err(ReasonCode::ProtocolError.into()),
        }
    }

    pub(crate) async fn handle_pubrel(
        &mut self,
        packet: PubRelPacket,
    ) -> Result<Option<ControlPacket>> {
        let mut session = self.shared.state.lock().await;
        let reason = if session.incoming_qos2.remove(&packet.packet_id) {
            ReasonCode::Success
        } else {
            session.unknown_packet_id_reason()
        };

        Ok(Some(ControlPacket::PubComp(PubCompPacket {
            packet_id: packet.packet_id,
            reason,
            properties: None,
        })))
    }

    /// Adds the packet ids of incoming QoS 2 messages awaiting PUBREL,
    /// loaded from storage, to a restored session.
    pub(crate) async fn restore_incoming_qos2(&self, packet_ids: Vec<u16>) {
        self.shared
            .state
            .lock()
            .await
            .incoming_qos2
            .extend(packet_ids);
    }
}
