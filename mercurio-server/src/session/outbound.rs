//! What the session sends the client: QoS 1 and 2 messages in flight until
//! acknowledged, their packet identifiers and order, retransmission on resume,
//! and the messages a share group may take back.

use std::sync::Arc;

use mercurio_core::{
    message::{unix_millis, Message, MessageProperties},
    properties::{MessageExpiryInterval, SubscriptionIdentifier, TopicAlias},
    qos::QoS,
    reason::ReasonCode,
};
use mercurio_packets::{
    puback::PubAckPacket,
    pubcomp::PubCompPacket,
    publish::{PublishPacket, PublishProperties},
    pubrec::PubRecPacket,
    pubrel::PubRelPacket,
    ControlPacket, Packet,
};
use mercurio_storage::InflightMessage;
use tracing::warn;

use super::{message_properties, Session, State};
use crate::{error::Result, sys::DropCause};

/// The PUBLISH properties forwarding a message's application properties,
/// along with the identifiers of the subscriptions it was delivered through.
fn publish_properties(
    properties: &MessageProperties,
    subscription_identifiers: &[u32],
) -> Option<PublishProperties> {
    Some(PublishProperties {
        subscription_identifiers: subscription_identifiers
            .iter()
            // A stored identifier outside the range can't be sent
            .filter_map(|id| SubscriptionIdentifier::new(*id))
            .collect(),
        payload_format_indicator: properties.payload_format_indicator.clone(),
        message_expiry_interval: properties.message_expiry_interval.clone(),
        content_type: properties.content_type.clone(),
        response_topic: properties.response_topic.clone(),
        correlation_data: properties.correlation_data.clone(),
        user_property: properties.user_property.clone(),
        ..Default::default()
    })
}

/// An outgoing QoS 1 or 2 message the client has yet to acknowledge.
pub(super) struct Unacknowledged {
    /// Position in the order messages were sent
    pub(super) sequence: u64,
    pub(super) publish: PublishPacket,
    /// Unix time in milliseconds when the message expires, if it does
    pub(super) expires_at: Option<u64>,
    /// The shared subscription it came through, if it did
    pub(super) share_group: Option<Arc<str>>,
}

/// A message that reached a session through a shared subscription and may
/// go to another member of the group.
pub(crate) struct Handback {
    pub message: Message,
    /// The shared subscription, `$share/{name}/{filter}`
    pub group: Arc<str>,
    pub held: Held,
}

/// Where a session holds a message it may hand back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Held {
    /// Queued, at this sequence of the queue
    Queued(u64),
    /// Sent at QoS 1 with this packet id, not acknowledged
    Unacknowledged(u16),
}

/// What a resumed session resends at once.
#[derive(Default)]
pub(crate) struct Retransmissions {
    pub packets: Vec<ControlPacket>,
    /// Packet ids of unacknowledged messages discarded as too large for
    /// the client or expired
    pub dropped: Vec<u16>,
}

/// A PUBLISH ready to send.
pub(crate) struct Outgoing {
    pub packet: ControlPacket,
    /// For QoS 1 and 2, the state to persist until the client acknowledges it
    pub inflight: Option<InflightMessage>,
}

impl Session {
    pub(crate) async fn handle_puback(
        &mut self,
        packet: PubAckPacket,
    ) -> Result<Option<ControlPacket>> {
        let mut session = self.shared.state.lock().await;
        session.unacknowledged_messages.remove(&packet.packet_id);
        session.unsent.retain(|id| *id != packet.packet_id);
        Ok(None)
    }

    pub(crate) async fn handle_pubrec(
        &mut self,
        packet: PubRecPacket,
    ) -> Result<Option<ControlPacket>> {
        let mut session = self.shared.state.lock().await;
        let packet_id = packet.packet_id;
        let sequence = match session.unacknowledged_messages.remove(&packet_id) {
            Some(sent) => Some(sent.sequence),
            None => session.pubrecs.get(&packet_id).copied(),
        };
        session.unsent.retain(|id| *id != packet_id);

        // A failure reason ends the exchange: the client won't take the message
        if packet.reason.code() >= 0x80 {
            session.pubrecs.remove(&packet_id);
            return Ok(None);
        }

        let reason = if let Some(sequence) = sequence {
            session.pubrecs.insert(packet_id, sequence);
            ReasonCode::Success
        } else {
            session.unknown_packet_id_reason()
        };

        Ok(Some(ControlPacket::PubRel(PubRelPacket {
            packet_id,
            reason,
            properties: None,
        })))
    }

    pub(crate) async fn handle_pubcomp(
        &mut self,
        packet: PubCompPacket,
    ) -> Result<Option<ControlPacket>> {
        let mut session = self.shared.state.lock().await;
        // O(1) removal
        session.pubrecs.remove(&packet.packet_id);
        Ok(None)
    }

    /// The messages that reached this session through a shared
    /// subscription and may go to another member of the group: the QoS 1
    /// messages sent and not acknowledged, in the order they were sent,
    /// then those queued. QoS 2 messages sent stay with the session
    /// [MQTT-4.8.2-5].
    pub(crate) async fn handbacks(&self) -> Vec<Handback> {
        let mut unacknowledged: Vec<(u64, Handback)> = {
            let state = self.shared.state.lock().await;
            state
                .unacknowledged_messages
                .iter()
                .filter(|(_, sent)| sent.publish.qos_level == QoS::AtLeastOnce)
                .filter_map(|(packet_id, sent)| {
                    let group = sent.share_group.clone()?;
                    let publish = &sent.publish;
                    let message = Message {
                        packet_id: None,
                        topic: Arc::from(publish.topic_name.as_str()),
                        dup: false,
                        qos: publish.qos_level,
                        retain: publish.retain,
                        payload: publish.payload.clone(),
                        properties: message_properties(publish.properties.as_ref()),
                        expires_at: sent.expires_at,
                        subscription_identifiers: Vec::new(),
                        origin: None,
                        share_group: None,
                    };
                    let held = Held::Unacknowledged(*packet_id);
                    Some((
                        sent.sequence,
                        Handback {
                            message,
                            group,
                            held,
                        },
                    ))
                })
                .collect()
        };
        unacknowledged.sort_by_key(|(sequence, _)| *sequence);
        let queued = self
            .shared
            .queue
            .shared()
            .into_iter()
            .filter_map(|(sequence, message)| {
                Some(Handback {
                    group: message.share_group.clone()?,
                    message,
                    held: Held::Queued(sequence),
                })
            });
        unacknowledged
            .into_iter()
            .map(|(_, handback)| handback)
            .chain(queued)
            .collect()
    }

    /// Lets go of a message another member of its group took. False if the
    /// session no longer held it.
    pub(crate) async fn give_up(&self, held: Held) -> bool {
        match held {
            Held::Queued(sequence) => self.shared.queue.remove(sequence),
            Held::Unacknowledged(packet_id) => {
                let mut state = self.shared.state.lock().await;
                state.unsent.retain(|id| *id != packet_id);
                state.unacknowledged_messages.remove(&packet_id).is_some()
            }
        }
    }

    /// Whether another QoS 1 or 2 message may be sent without exceeding the
    /// client's Receive Maximum [MQTT-3.3.4-9] or the broker's in-flight
    /// limit.
    pub(crate) async fn can_send_more(&self) -> bool {
        let session = self.shared.state.lock().await;
        session.in_flight() < session.window()
    }

    /// The next unacknowledged message a resumed session holds back, once
    /// the window has room for it. Resent before anything newer, keeping
    /// the order messages were first sent in [MQTT-4.6.0-1].
    pub(crate) async fn next_retransmission(&self) -> Option<ControlPacket> {
        let mut session = self.shared.state.lock().await;
        if session.in_flight() >= session.window() {
            return None;
        }
        while let Some(packet_id) = session.unsent.pop_front() {
            if let Some(sent) = session.unacknowledged_messages.get(&packet_id) {
                let mut publish = sent.publish.clone();
                publish.dup = true;
                return Some(ControlPacket::Publish(publish));
            }
        }
        None
    }

    /// Waits for the next message to send to the client. Cancel safe, and
    /// doesn't hold the session state while waiting. Also returns the
    /// message's sequence if it is stored in the queue's journal, from
    /// which it is to be removed once sent or dropped.
    pub(crate) async fn next_outgoing(&self, window_open: bool) -> (Message, Option<u64>) {
        self.shared.queue.pop(!window_open).await
    }

    /// Turns a queued message into the PUBLISH to send, tracking it until
    /// acknowledged. Returns None if the message had to be dropped.
    pub(crate) async fn prepare_publish(&mut self, mut message: Message) -> Option<Outgoing> {
        // Expired messages are never delivered, and the rest carry only the
        // time they have left [MQTT-3.3.2-5, MQTT-3.3.2-6]
        if let Some(deadline) = message.expires_at {
            let now = unix_millis();
            if now >= deadline {
                self.count_drop(DropCause::Expired);
                return None;
            }
            let remaining_secs = (deadline - now).div_ceil(1000);
            message.properties.message_expiry_interval = Some(MessageExpiryInterval::new(
                remaining_secs.try_into().unwrap_or(u32::MAX),
            ));
        }

        let mut session = self.shared.state.lock().await;

        {
            let packet_id = match message.qos {
                QoS::AtMostOnce => None,
                _ => match session.next_packet_id() {
                    Some(id) => Some(id),
                    None => {
                        // Only reachable when the client's Receive Maximum allows
                        // every identifier to be in flight at once
                        warn!(
                            "No free packet identifier for client `{}`, dropping message",
                            session.client_id
                        );
                        return None;
                    }
                },
            };

            let publish = PublishPacket {
                dup: false,
                qos_level: message.qos,
                retain: message.retain,
                topic_name: message.topic.to_string(),
                packet_id,
                properties: publish_properties(
                    &message.properties,
                    &message.subscription_identifiers,
                ),
                payload: message.payload,
            };

            // Packets the client can't take are discarded as if delivered
            // [MQTT-3.1.2-25]
            let Some(sent) = session.apply_topic_alias(publish.clone()) else {
                self.count_drop(DropCause::TooLarge);
                warn!(
                    "Message on `{}` exceeds the maximum packet size of client `{}`, dropping it",
                    publish.topic_name, session.client_id
                );
                return None;
            };

            let inflight = packet_id.map(|packet_id| {
                let sequence = session.next_sequence;
                session.next_sequence += 1;
                session.unacknowledged_messages.insert(
                    packet_id,
                    Unacknowledged {
                        sequence,
                        publish: publish.clone(),
                        expires_at: message.expires_at,
                        share_group: message.share_group.clone(),
                    },
                );
                InflightMessage {
                    packet_id,
                    topic: publish.topic_name.clone(),
                    payload: publish.payload.clone(),
                    qos: publish.qos_level,
                    dup: false,
                    retain: publish.retain,
                    expires_at: message.expires_at,
                    sequence,
                    released: false,
                    properties: message_properties(publish.properties.as_ref()),
                    subscription_identifiers: message.subscription_identifiers.clone(),
                    share_group: message.share_group.as_deref().map(str::to_string),
                }
            });

            Some(Outgoing {
                packet: ControlPacket::Publish(sent),
                inflight,
            })
        }
    }

    /// Adds in-flight messages loaded from storage to a restored session.
    pub(crate) async fn restore_inflight(&self, inflight: Vec<InflightMessage>) {
        let mut session = self.shared.state.lock().await;

        for message in inflight {
            session.next_sequence = session.next_sequence.max(message.sequence + 1);
            if message.released {
                session.pubrecs.insert(message.packet_id, message.sequence);
            } else {
                let publish = PublishPacket {
                    dup: false,
                    qos_level: message.qos,
                    retain: message.retain,
                    topic_name: message.topic,
                    packet_id: Some(message.packet_id),
                    properties: publish_properties(
                        &message.properties,
                        &message.subscription_identifiers,
                    ),
                    payload: message.payload,
                };
                session.unacknowledged_messages.insert(
                    message.packet_id,
                    Unacknowledged {
                        sequence: message.sequence,
                        publish,
                        expires_at: message.expires_at,
                        share_group: message.share_group.map(Arc::from),
                    },
                );
            }
        }
    }

    /// Packets to resend when a session resumes: unacknowledged PUBLISH
    /// packets flagged as duplicates and pending PUBRELs, in the order they
    /// were first sent [MQTT-4.4.0-1]. Only as many PUBLISH packets as the
    /// client's Receive Maximum allows are returned [MQTT-3.3.4-9]; the
    /// rest wait for `next_retransmission`. Those larger than the client's
    /// Maximum Packet Size are discarded as if delivered [MQTT-3.1.2-25],
    /// and those past their expiry are discarded too [MQTT-3.3.2-5]; the
    /// others carry the time they have left.
    pub(crate) async fn pending_retransmissions(&self) -> Retransmissions {
        let mut session = self.shared.state.lock().await;
        let session = &mut *session;
        let now = unix_millis();

        let mut resend = Retransmissions::default();
        session.unacknowledged_messages.retain(|packet_id, sent| {
            let expired = sent.expires_at.is_some_and(|deadline| now >= deadline);
            if expired {
                resend.dropped.push(*packet_id);
            }
            !expired
        });
        let mut pending: Vec<(u64, Option<u16>, ControlPacket)> = session
            .unacknowledged_messages
            .iter_mut()
            .map(|(packet_id, sent)| {
                if let Some(deadline) = sent.expires_at {
                    let remaining_secs = (deadline - now).div_ceil(1000);
                    sent.publish
                        .properties
                        .get_or_insert_with(Default::default)
                        .message_expiry_interval = Some(MessageExpiryInterval::new(
                        remaining_secs.try_into().unwrap_or(u32::MAX),
                    ));
                }
                let mut publish = sent.publish.clone();
                publish.dup = true;
                (
                    sent.sequence,
                    Some(*packet_id),
                    ControlPacket::Publish(publish),
                )
            })
            .chain(session.pubrecs.iter().map(|(packet_id, sequence)| {
                let pubrel = PubRelPacket {
                    packet_id: *packet_id,
                    reason: ReasonCode::Success,
                    properties: None,
                };
                (*sequence, None, ControlPacket::PubRel(pubrel))
            }))
            .collect();
        pending.sort_by_key(|(sequence, _, _)| *sequence);

        session.unsent.clear();
        let mut room = session.window().saturating_sub(session.pubrecs.len());
        for (_, packet_id, packet) in pending {
            let (Some(packet_id), ControlPacket::Publish(publish)) = (packet_id, &packet) else {
                resend.packets.push(packet);
                continue;
            };
            if session.too_big(publish) {
                self.count_drop(DropCause::TooLarge);
                warn!(
                    "Message on `{}` exceeds the maximum packet size of client `{}`, dropping it",
                    publish.topic_name, session.client_id
                );
                session.unacknowledged_messages.remove(&packet_id);
                resend.dropped.push(packet_id);
            } else if room > 0 {
                room -= 1;
                resend.packets.push(packet);
            } else {
                session.unsent.push_back(packet_id);
            }
        }
        resend
    }
}

impl State {
    /// Replaces the topic of an outgoing PUBLISH with an alias where the
    /// client accepts them, assigning new aliases until its maximum is
    /// reached. Only the copy sent is aliased: aliases end with the
    /// connection, so a retransmission must carry the full topic. Returns
    /// None if the packet is larger than the client accepts. A new alias
    /// adds its property to a packet that still carries the topic, so it is
    /// only assigned when that packet fits; otherwise the packet goes
    /// without one.
    fn apply_topic_alias(&mut self, publish: PublishPacket) -> Option<PublishPacket> {
        let maximum = self
            .client
            .topic_alias_maximum
            .filter(|_| self.send_topic_aliases)
            .unwrap_or(0);

        let with_alias = |mut publish: PublishPacket, alias: u16| {
            publish
                .properties
                .get_or_insert_with(Default::default)
                .topic_alias = Some(TopicAlias::new(alias));
            publish
        };

        let sent = match self.outgoing_aliases.get(&publish.topic_name) {
            // Already known to the client: the alias alone stands for it
            Some(&alias) => {
                let mut publish = with_alias(publish, alias);
                publish.topic_name.clear();
                publish
            }
            None if self.outgoing_aliases.len() < usize::from(maximum) => {
                let alias = self.outgoing_aliases.len() as u16 + 1;
                let topic = publish.topic_name.clone();
                let aliased = with_alias(publish.clone(), alias);
                if self.too_big(&aliased) {
                    publish
                } else {
                    self.outgoing_aliases.insert(topic, alias);
                    aliased
                }
            }
            None => publish,
        };
        (!self.too_big(&sent)).then_some(sent)
    }

    /// Most QoS 1 and 2 messages that may be in flight: the client's
    /// Receive Maximum, capped by the broker's limit.
    fn window(&self) -> usize {
        let receive_maximum = self.client.receive_maximum.unwrap_or(u16::MAX);
        usize::from(receive_maximum.min(self.max_inflight))
    }

    /// QoS 1 and 2 messages sent on this connection or an earlier one and
    /// not yet acknowledged, leaving out those still to be resent.
    fn in_flight(&self) -> usize {
        self.unacknowledged_messages
            .len()
            .saturating_sub(self.unsent.len())
            + self.pubrecs.len()
    }

    /// Whether `publish` is larger than the client accepts.
    fn too_big(&self, publish: &PublishPacket) -> bool {
        self.client_maximum_packet_size()
            .is_some_and(|maximum| publish.encoded_size(self.client.protocol_version) > maximum)
    }

    /// Allocates the next packet identifier not currently in flight.
    /// Identifiers are per session, so they can't be copied from the publisher.
    pub(super) fn next_packet_id(&mut self) -> Option<u16> {
        for _ in 0..u16::MAX {
            self.last_packet_id = self.last_packet_id.checked_add(1).unwrap_or(1);
            let id = self.last_packet_id;
            if !self.unacknowledged_messages.contains_key(&id) && !self.pubrecs.contains_key(&id) {
                return Some(id);
            }
        }
        None
    }
}
