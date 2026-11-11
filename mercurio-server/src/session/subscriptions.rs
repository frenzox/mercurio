//! What the client subscribes to: SUBSCRIBE and UNSUBSCRIBE with their checks
//! and limits, the subscriptions' place in the topic tree, and the changes to
//! store.

use std::{
    collections::VecDeque,
    sync::{atomic::Ordering, Arc},
};

use mercurio_core::{
    qos::QoS,
    reason::ReasonCode,
    topic::{parse_shared_subscription, validate_subscribe_filter},
};
use mercurio_packets::{
    suback::{SubAckPacket, SubAckPayload},
    subscribe::RetainHandling,
    unsuback::{UnsubAckPacket, UnsubAckPayload},
    unsubscribe::UnsubscribePacket,
    ControlPacket,
};
use mercurio_storage::{RetainedMessageStore, StoredSubscription, Write};
use tracing::warn;

use super::{RetainedDelivery, Session, State};
use crate::{
    acl::Access,
    broker::Broker,
    error::Result,
    plugin::{Chain, Event},
    queue::Subscriber,
};

impl Session {
    /// Returns the subscriptions of this session with their granted QoS and
    /// options.
    pub(crate) async fn get_subscriptions(&self) -> Vec<StoredSubscription> {
        let session = self.shared.state.lock().await;
        session.subscriptions.values().cloned().collect()
    }

    /// What SUBSCRIBE and UNSUBSCRIBE packets changed since this was last
    /// called, as writes to store them.
    pub(crate) async fn take_subscription_changes(&self) -> Vec<Write> {
        std::mem::take(&mut self.shared.state.lock().await.subscription_changes)
    }

    /// Restore subscriptions by re-subscribing to the broker.
    /// Called when resuming a persisted session.
    pub(crate) async fn restore_subscriptions<S: RetainedMessageStore>(
        &mut self,
        subscriptions: Vec<StoredSubscription>,
        broker: &Broker<S>,
    ) -> Result<()> {
        let mut session = self.shared.state.lock().await;

        session.topics = Some(broker.topics());
        for subscription in subscriptions {
            // Stored by an older version that accepted it, such as one
            // deeper than the level limit
            if let Err(e) = validate_subscribe_filter(&subscription.topic_filter) {
                warn!(
                    "Dropping stored subscription of `{}` to an invalid filter: {}",
                    session.client_id, e
                );
                continue;
            }
            // Retained messages are only sent for a new SUBSCRIBE, not when an
            // existing subscription is resumed [MQTT-3.3.1-9]
            broker.subscribe(
                &subscription.topic_filter,
                self.shared.subscriber_id,
                self.subscriber(&subscription),
            );
            session
                .subscriptions
                .insert(subscription.topic_filter.clone(), subscription);
        }

        Ok(())
    }

    pub(super) async fn handle_subscribe<S: RetainedMessageStore>(
        &mut self,
        packet: mercurio_packets::subscribe::SubscribePacket,
        broker: &Broker<S>,
        chain: &Chain,
    ) -> Result<Option<ControlPacket>> {
        let subscription_identifier = packet
            .properties
            .as_ref()
            .and_then(|p| p.subscription_id.as_ref())
            .map(|id| id.value());

        // A client can't opt out of its own messages on a shared subscription
        // [MQTT-3.8.3-4]
        if packet.payload.iter().any(|sub| {
            sub.subs_opt.no_local && parse_shared_subscription(&sub.topic_filter).is_some()
        }) {
            return Err(ReasonCode::ProtocolError.into());
        }

        // Access control may wait on a plugin, so it runs before the
        // session state is locked
        let mut refused = Vec::with_capacity(packet.payload.len());
        let identity = self.identity().await;
        let max_length = self.shared.state.lock().await.max_topic_filter_length;
        for sub in &packet.payload {
            refused.push(
                if sub.topic_filter.len() > max_length
                    || validate_subscribe_filter(&sub.topic_filter).is_err()
                {
                    Some(ReasonCode::TopicFilterInvalid)
                } else if !chain.is_empty() && !chain.is_trusted() {
                    // The share group decides who gets each message, not which
                    // messages flow, so only the topic filter is checked, as
                    // mosquitto does
                    let filter = parse_shared_subscription(&sub.topic_filter)
                        .map_or(sub.topic_filter.as_str(), |(_, filter)| filter);
                    if chain.may(&identity, filter, Access::Subscribe).await {
                        None
                    } else {
                        warn!(
                            "Client `{}` may not subscribe to `{}`",
                            identity.client_id, sub.topic_filter
                        );
                        Some(ReasonCode::NotAuthorized)
                    }
                } else {
                    None
                },
            );
        }

        let mut session = self.shared.state.lock().await;
        let mut ack = SubAckPacket {
            packet_id: packet.packet_id,
            properties: None,
            payload: Vec::with_capacity(packet.payload.len()),
        };

        for (sub, refused) in packet.payload.iter().zip(refused) {
            if let Some(reason_code) = refused {
                ack.payload.push(SubAckPayload { reason_code });
                continue;
            }

            let at_limit = session
                .max_subscriptions
                .is_some_and(|max| session.subscriptions.len() >= max);
            if at_limit && !session.subscriptions.contains_key(&sub.topic_filter) {
                warn!(
                    "Client `{}` holds the most subscriptions allowed",
                    session.client_id
                );
                ack.payload.push(SubAckPayload {
                    reason_code: ReasonCode::QuotaExceeded,
                });
                continue;
            }

            let granted_qos = if sub.subs_opt.qos as u8 > session.maximum_qos as u8 {
                session.maximum_qos
            } else {
                sub.subs_opt.qos
            };
            let subscription = StoredSubscription {
                topic_filter: sub.topic_filter.clone(),
                qos: granted_qos,
                no_local: sub.subs_opt.no_local,
                retain_as_published: sub.subs_opt.retain_as_pub,
                subscription_identifier,
            };
            let subscriber = self.subscriber(&subscription);
            let existed = session.subscriptions.contains_key(&sub.topic_filter);
            broker.subscribe(
                &sub.topic_filter,
                self.shared.subscriber_id,
                subscriber.clone(),
            );
            if !chain.is_empty() {
                chain.emit(Event::Subscribed {
                    client: session.client_info(),
                    filter: sub.topic_filter.clone(),
                    qos: granted_qos,
                });
            }

            // Shared subscriptions are never sent retained messages
            let shared = parse_shared_subscription(&sub.topic_filter).is_some();
            let send_retained = !shared
                && match sub.subs_opt.retain_handling {
                    RetainHandling::SendRetained => true,
                    RetainHandling::SendRetainedIfNonExisting => !existed,
                    _ => false,
                };
            // Queued a page at a time by the connection loop. A repeated
            // subscription starts over, or else sends what the earlier one
            // was still owed with the new options.
            if send_retained {
                session.retained.retain(|d| d.filter != sub.topic_filter);
                session.retained.push_back(RetainedDelivery {
                    filter: sub.topic_filter.clone(),
                    subscriber,
                    messages: VecDeque::new(),
                    after: None,
                    done: false,
                });
                self.shared.owes_retained.store(true, Ordering::Relaxed);
            } else if let Some(owed) = session
                .retained
                .iter_mut()
                .find(|d| d.filter == sub.topic_filter)
            {
                owed.subscriber = subscriber;
            }
            ack.payload.push(SubAckPayload {
                reason_code: match granted_qos {
                    QoS::AtLeastOnce => ReasonCode::GrantedQoS1,
                    QoS::ExactlyOnce => ReasonCode::GrantedQoS2,
                    _ => ReasonCode::GrantedQoS0,
                },
            });

            session.topics = Some(broker.topics());
            session
                .subscription_changes
                .push(Write::AddSubscription(subscription.clone()));
            session
                .subscriptions
                .insert(sub.topic_filter.clone(), subscription);
        }

        Ok(Some(ControlPacket::SubAck(ack)))
    }

    pub(super) async fn handle_unsubscribe<S: RetainedMessageStore>(
        &mut self,
        packet: UnsubscribePacket,
        broker: &Broker<S>,
        chain: &Chain,
    ) -> Result<Option<ControlPacket>> {
        let mut session = self.shared.state.lock().await;
        let mut ack = UnsubAckPacket {
            packet_id: packet.packet_id,
            properties: None,
            payload: Vec::with_capacity(packet.payload.len()),
        };

        for unsub in &packet.payload {
            if validate_subscribe_filter(&unsub.topic_filter).is_err() {
                ack.payload.push(UnsubAckPayload {
                    reason_code: ReasonCode::TopicFilterInvalid,
                });
                continue;
            }

            broker.unsubscribe(&unsub.topic_filter, self.shared.subscriber_id);
            session.retained.retain(|d| d.filter != unsub.topic_filter);
            let reason_code = if session.subscriptions.remove(&unsub.topic_filter).is_some() {
                session
                    .subscription_changes
                    .push(Write::RemoveSubscription(unsub.topic_filter.clone()));
                ReasonCode::Success
            } else {
                ReasonCode::NoSubscriptionExisted
            };
            if reason_code == ReasonCode::Success && !chain.is_empty() {
                chain.emit(Event::Unsubscribed {
                    client: session.client_info(),
                    filter: unsub.topic_filter.clone(),
                });
            }

            ack.payload.push(UnsubAckPayload { reason_code });
        }

        Ok(Some(ControlPacket::UnsubAck(ack)))
    }

    fn subscriber(&self, subscription: &StoredSubscription) -> Subscriber {
        Subscriber {
            queue: Arc::clone(&self.shared.queue),
            session_id: self.shared.subscriber_id,
            granted_qos: subscription.qos,
            no_local: subscription.no_local,
            retain_as_published: subscription.retain_as_published,
            subscription_identifier: subscription.subscription_identifier,
            share: parse_shared_subscription(&subscription.topic_filter)
                .is_some()
                .then(|| Arc::from(subscription.topic_filter.as_str())),
        }
    }

    /// Removes every subscription of this session from the broker. Called
    /// when the session ends, so nothing is queued for it anymore.
    pub(crate) async fn unsubscribe_all(&self) {
        let mut session = self.shared.state.lock().await;
        session.unsubscribe_all(self.shared.subscriber_id);
    }
}

impl State {
    /// Removes every subscription of the session `subscriber_id` from the
    /// topic tree.
    pub(super) fn unsubscribe_all(&mut self, subscriber_id: u64) {
        if let Some(topics) = self.topics.take() {
            for topic_filter in self.subscriptions.keys() {
                topics.unsubscribe(topic_filter, subscriber_id);
            }
        }
        self.subscriptions.clear();
        self.retained.clear();
    }
}
