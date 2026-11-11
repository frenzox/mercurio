//! What the session sends the client: QoS 1 and 2 messages in flight until
//! acknowledged, their packet identifiers and order, retransmission on resume,
//! and the messages a share group may take back.

use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};

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
use mercurio_storage::{InflightMessage, Write};
use tracing::warn;

use super::{message_properties, Handled, Session, State};
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

/// The QoS 1 and 2 messages sent to the client whose exchange hasn't
/// ended, with the order they were sent in and the packet identifiers they
/// hold.
#[derive(Default)]
pub(super) struct Inflight {
    /// Awaiting PUBACK or PUBREC, by packet id
    pub(super) unacknowledged_messages: HashMap<u16, Unacknowledged>,
    /// QoS 2 messages awaiting PUBCOMP, by packet id, with the sequence
    /// they were sent in
    pub(super) pubrecs: HashMap<u16, u64>,
    /// Unacknowledged messages a resumed session has yet to resend on this
    /// connection, in order, held back by the client's Receive Maximum
    unsent: VecDeque<u16>,
    /// Sequence number of the next message sent
    next_sequence: u64,
    /// Last packet identifier assigned
    last_packet_id: u16,
}

impl Inflight {
    /// Messages sent and not acknowledged, released QoS 2 ones included.
    pub(super) fn len(&self) -> usize {
        self.unacknowledged_messages.len() + self.pubrecs.len()
    }

    /// QoS 1 and 2 messages sent on this connection or an earlier one and
    /// not yet acknowledged, leaving out those still to be resent.
    fn in_flight(&self) -> usize {
        self.unacknowledged_messages
            .len()
            .saturating_sub(self.unsent.len())
            + self.pubrecs.len()
    }

    /// Allocates the next packet identifier not currently in flight.
    /// Identifiers are per session, so they can't be copied from the publisher.
    fn next_packet_id(&mut self) -> Option<u16> {
        for _ in 0..u16::MAX {
            self.last_packet_id = self.last_packet_id.checked_add(1).unwrap_or(1);
            let id = self.last_packet_id;
            if !self.unacknowledged_messages.contains_key(&id) && !self.pubrecs.contains_key(&id) {
                return Some(id);
            }
        }
        None
    }

    /// Tracks `publish`, sent with `packet_id`, until it is acknowledged,
    /// and returns the sequence it was sent in.
    fn send(
        &mut self,
        packet_id: u16,
        publish: PublishPacket,
        expires_at: Option<u64>,
        share_group: Option<Arc<str>>,
    ) -> u64 {
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        self.unacknowledged_messages.insert(
            packet_id,
            Unacknowledged {
                sequence,
                publish,
                expires_at,
                share_group,
            },
        );
        sequence
    }

    /// PUBACK: the client took the QoS 1 message.
    fn acknowledge(&mut self, packet_id: u16) -> Write {
        self.unacknowledged_messages.remove(&packet_id);
        self.unsent.retain(|id| *id != packet_id);
        Write::RemoveInflight(packet_id)
    }

    /// PUBREC: the client took the QoS 2 message, which awaits PUBCOMP
    /// from now on. False if `packet_id` had no exchange in progress.
    fn release(&mut self, packet_id: u16) -> bool {
        let sequence = match self.unacknowledged_messages.remove(&packet_id) {
            Some(sent) => Some(sent.sequence),
            None => self.pubrecs.get(&packet_id).copied(),
        };
        self.unsent.retain(|id| *id != packet_id);
        match sequence {
            Some(sequence) => {
                self.pubrecs.insert(packet_id, sequence);
                true
            }
            None => false,
        }
    }

    /// A PUBREC with a failure reason: the client won't take the message,
    /// which ends its exchange.
    fn refuse(&mut self, packet_id: u16) -> Write {
        self.unacknowledged_messages.remove(&packet_id);
        self.unsent.retain(|id| *id != packet_id);
        self.pubrecs.remove(&packet_id);
        Write::RemoveInflight(packet_id)
    }

    /// PUBCOMP: the QoS 2 exchange is over.
    fn complete(&mut self, packet_id: u16) -> Write {
        self.pubrecs.remove(&packet_id);
        Write::RemoveInflight(packet_id)
    }

    /// Lets go of an unacknowledged message. False if it wasn't held.
    fn give_up(&mut self, packet_id: u16) -> bool {
        self.unsent.retain(|id| *id != packet_id);
        self.unacknowledged_messages.remove(&packet_id).is_some()
    }

    /// The next message held back to resend, flagged as a duplicate.
    fn next_unsent(&mut self) -> Option<PublishPacket> {
        while let Some(packet_id) = self.unsent.pop_front() {
            if let Some(sent) = self.unacknowledged_messages.get(&packet_id) {
                let mut publish = sent.publish.clone();
                publish.dup = true;
                return Some(publish);
            }
        }
        None
    }

    /// Adds a message loaded from storage.
    fn restore(&mut self, message: InflightMessage) {
        self.next_sequence = self.next_sequence.max(message.sequence + 1);
        if message.released {
            self.pubrecs.insert(message.packet_id, message.sequence);
            return;
        }
        let publish = PublishPacket {
            dup: false,
            qos_level: message.qos,
            retain: message.retain,
            topic_name: message.topic,
            packet_id: Some(message.packet_id),
            properties: publish_properties(&message.properties, &message.subscription_identifiers),
            payload: message.payload,
        };
        self.unacknowledged_messages.insert(
            message.packet_id,
            Unacknowledged {
                sequence: message.sequence,
                publish,
                expires_at: message.expires_at,
                share_group: message.share_group.map(Arc::from),
            },
        );
    }

    /// The writes that store every message in flight.
    pub(super) fn stored(&self) -> impl Iterator<Item = Write> + '_ {
        let unacknowledged = self
            .unacknowledged_messages
            .iter()
            .map(|(packet_id, sent)| {
                let properties = sent.publish.properties.as_ref();
                Write::StoreInflight(InflightMessage {
                    packet_id: *packet_id,
                    topic: sent.publish.topic_name.clone(),
                    payload: sent.publish.payload.clone(),
                    qos: sent.publish.qos_level,
                    dup: false,
                    retain: sent.publish.retain,
                    expires_at: sent.expires_at,
                    sequence: sent.sequence,
                    released: false,
                    properties: message_properties(properties),
                    subscription_identifiers: properties
                        .map(|p| {
                            p.subscription_identifiers
                                .iter()
                                .map(|id| id.value())
                                .collect()
                        })
                        .unwrap_or_default(),
                    share_group: sent.share_group.as_deref().map(str::to_string),
                })
            });
        let released = self.pubrecs.iter().map(|(packet_id, sequence)| {
            Write::StoreInflight(InflightMessage {
                packet_id: *packet_id,
                topic: String::new(),
                payload: None,
                qos: QoS::ExactlyOnce,
                dup: false,
                retain: false,
                expires_at: None,
                sequence: *sequence,
                released: true,
                properties: MessageProperties::default(),
                subscription_identifiers: Vec::new(),
                share_group: None,
            })
        });
        unacknowledged.chain(released)
    }
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
    pub(crate) async fn handle_puback(&mut self, packet: PubAckPacket) -> Result<Handled> {
        let mut session = self.shared.state.lock().await;
        Ok(Handled {
            response: None,
            write: Some(session.inflight.acknowledge(packet.packet_id)),
        })
    }

    pub(crate) async fn handle_pubrec(&mut self, packet: PubRecPacket) -> Result<Handled> {
        let mut session = self.shared.state.lock().await;
        let packet_id = packet.packet_id;

        // A failure reason ends the exchange: the client won't take the message
        if packet.reason.code() >= 0x80 {
            return Ok(Handled {
                response: None,
                write: Some(session.inflight.refuse(packet_id)),
            });
        }

        let reason = if session.inflight.release(packet_id) {
            ReasonCode::Success
        } else {
            session.unknown_packet_id_reason()
        };

        // After PUBREC only PUBREL remains to be sent for a QoS 2 message
        Ok(Handled {
            response: Some(ControlPacket::PubRel(PubRelPacket {
                packet_id,
                reason,
                properties: None,
            })),
            write: (reason == ReasonCode::Success).then_some(Write::ReleaseInflight(packet_id)),
        })
    }

    pub(crate) async fn handle_pubcomp(&mut self, packet: PubCompPacket) -> Result<Handled> {
        let mut session = self.shared.state.lock().await;
        Ok(Handled {
            response: None,
            write: Some(session.inflight.complete(packet.packet_id)),
        })
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
                .inflight
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
                state.inflight.give_up(packet_id)
            }
        }
    }

    /// Whether another QoS 1 or 2 message may be sent without exceeding the
    /// client's Receive Maximum [MQTT-3.3.4-9] or the broker's in-flight
    /// limit.
    pub(crate) async fn can_send_more(&self) -> bool {
        let session = self.shared.state.lock().await;
        session.inflight.in_flight() < session.window()
    }

    /// The next unacknowledged message a resumed session holds back, once
    /// the window has room for it. Resent before anything newer, keeping
    /// the order messages were first sent in [MQTT-4.6.0-1].
    pub(crate) async fn next_retransmission(&self) -> Option<ControlPacket> {
        let mut session = self.shared.state.lock().await;
        if session.inflight.in_flight() >= session.window() {
            return None;
        }
        session.inflight.next_unsent().map(ControlPacket::Publish)
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
                _ => match session.inflight.next_packet_id() {
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
                let sequence = session.inflight.send(
                    packet_id,
                    publish.clone(),
                    message.expires_at,
                    message.share_group.clone(),
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
            session.inflight.restore(message);
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
        session
            .inflight
            .unacknowledged_messages
            .retain(|packet_id, sent| {
                let expired = sent.expires_at.is_some_and(|deadline| now >= deadline);
                if expired {
                    resend.dropped.push(*packet_id);
                }
                !expired
            });
        let mut pending: Vec<(u64, Option<u16>, ControlPacket)> = session
            .inflight
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
            .chain(
                session
                    .inflight
                    .pubrecs
                    .iter()
                    .map(|(packet_id, sequence)| {
                        let pubrel = PubRelPacket {
                            packet_id: *packet_id,
                            reason: ReasonCode::Success,
                            properties: None,
                        };
                        (*sequence, None, ControlPacket::PubRel(pubrel))
                    }),
            )
            .collect();
        pending.sort_by_key(|(sequence, _, _)| *sequence);

        session.inflight.unsent.clear();
        let mut room = session
            .window()
            .saturating_sub(session.inflight.pubrecs.len());
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
                session.inflight.unacknowledged_messages.remove(&packet_id);
                resend.dropped.push(packet_id);
            } else if room > 0 {
                room -= 1;
                resend.packets.push(packet);
            } else {
                session.inflight.unsent.push_back(packet_id);
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

    /// Whether `publish` is larger than the client accepts.
    fn too_big(&self, publish: &PublishPacket) -> bool {
        self.client_maximum_packet_size()
            .is_some_and(|maximum| publish.encoded_size(self.client.protocol_version) > maximum)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn publish(packet_id: u16) -> PublishPacket {
        PublishPacket {
            qos_level: QoS::ExactlyOnce,
            topic_name: "t".into(),
            packet_id: Some(packet_id),
            ..Default::default()
        }
    }

    fn sent(inflight: &mut Inflight) -> u16 {
        let packet_id = inflight.next_packet_id().unwrap();
        inflight.send(packet_id, publish(packet_id), None, None);
        packet_id
    }

    #[test]
    fn test_next_packet_id_skips_in_flight_and_wraps() {
        let mut inflight = Inflight {
            last_packet_id: u16::MAX - 1,
            ..Default::default()
        };
        for packet_id in [u16::MAX, 1] {
            inflight.send(packet_id, publish(packet_id), None, None);
        }

        assert_eq!(inflight.next_packet_id(), Some(2));
    }

    #[test]
    fn test_released_messages_keep_their_packet_id_until_pubcomp() {
        let mut inflight = Inflight::default();
        let packet_id = sent(&mut inflight);

        assert!(inflight.release(packet_id));
        assert_eq!(inflight.len(), 1);
        assert_ne!(inflight.next_packet_id(), Some(packet_id));
        // PUBREC again, as after a reconnect, keeps the exchange going
        assert!(inflight.release(packet_id));

        inflight.complete(packet_id);
        assert_eq!(inflight.len(), 0);
        assert!(!inflight.release(packet_id));
    }

    #[test]
    fn test_acknowledgements_and_refusals_end_the_exchange() {
        let mut inflight = Inflight::default();
        let acknowledged = sent(&mut inflight);
        let refused = sent(&mut inflight);
        let released = sent(&mut inflight);
        assert!(inflight.release(released));

        inflight.acknowledge(acknowledged);
        inflight.refuse(refused);
        inflight.refuse(released);
        assert_eq!(inflight.len(), 0);
        assert_eq!(inflight.in_flight(), 0);
    }

    #[test]
    fn test_stored_messages_come_back_in_their_order() {
        let mut inflight = Inflight::default();
        let first = sent(&mut inflight);
        let second = sent(&mut inflight);
        assert!(inflight.release(first));

        let mut restored = Inflight::default();
        for write in inflight.stored() {
            let Write::StoreInflight(message) = write else {
                panic!("expected StoreInflight");
            };
            restored.restore(message);
        }
        assert_eq!(restored.pubrecs.get(&first), Some(&0));
        assert_eq!(restored.unacknowledged_messages[&second].sequence, 1);
        // Messages sent from now on come after them
        let third = sent(&mut restored);
        assert_eq!(restored.unacknowledged_messages[&third].sequence, 2);
    }

    #[test]
    fn test_given_up_messages_are_not_resent() {
        let mut inflight = Inflight::default();
        let packet_id = sent(&mut inflight);
        inflight.unsent.push_back(packet_id);

        assert!(inflight.give_up(packet_id));
        assert!(!inflight.give_up(packet_id));
        assert!(inflight.next_unsent().is_none());
    }
}
