//! Retained messages owed to new subscriptions, queued a page at a time as the
//! client's queue drains.

use std::{collections::VecDeque, sync::atomic::Ordering};

use mercurio_core::{message::Message, qos::QoS};
use mercurio_storage::RetainedMessageStore;
use tracing::warn;

use super::Session;
use crate::{
    acl::Access,
    broker::Broker,
    plugin::{is_control, Chain, ClientInfo},
    queue::Subscriber,
    sys::DropCause,
};

/// Retained messages owed to new subscriptions are queued while fewer than
/// this many messages wait, so a large set never floods the queue.
pub(super) const RETAINED_PAGE: usize = 100;

/// The retained messages a new subscription is owed and not yet queued.
pub(super) struct RetainedDelivery {
    pub(super) filter: String,
    pub(super) subscriber: Subscriber,
    /// The page looked up last, what is left of it
    pub(super) messages: VecDeque<Message>,
    /// The topic of the last message looked up, after which the next page
    /// starts
    pub(super) after: Option<String>,
    /// The last page has been looked up
    pub(super) done: bool,
}

/// Whether the client that published `message` may still publish to its
/// topic. Messages from the broker itself, such as `$SYS` ones, always may.
async fn publisher_may_write(chain: &Chain, message: &Message) -> bool {
    let Some(origin) = &message.origin else {
        return true;
    };
    let publisher = ClientInfo::new(origin.client_id.clone(), origin.username.clone());
    match is_control(&message.topic) {
        true => chain.may_control(&publisher, &message.topic).await,
        false => chain.may(&publisher, &message.topic, Access::Write).await,
    }
}

impl Session {
    /// Whether new subscriptions are owed retained messages not yet queued.
    pub(crate) fn owes_retained(&self) -> bool {
        self.shared.owes_retained.load(Ordering::Relaxed)
    }

    /// Queues retained messages owed to new subscriptions while fewer than
    /// a page of messages wait to be sent. Called from the connection loop
    /// as the queue drains, so a subscription matching many retained
    /// messages gets every one of them whatever the queue's limits.
    pub(crate) async fn feed_retained<S: RetainedMessageStore>(
        &self,
        broker: &Broker<S>,
        chain: &Chain,
    ) {
        let queue = &self.shared.queue;
        while queue.len() < RETAINED_PAGE {
            let (mut delivery, check_source) = {
                let mut state = self.shared.state.lock().await;
                let Some(delivery) = state.retained.pop_front() else {
                    self.shared.owes_retained.store(false, Ordering::Relaxed);
                    return;
                };
                (delivery, state.check_retain_source)
            };
            // One page at a time, so a subscription matching many retained
            // messages never has them all in memory
            if delivery.messages.is_empty() && !delivery.done {
                let page = broker
                    .retained_page(&delivery.filter, delivery.after.as_deref(), RETAINED_PAGE)
                    .await;
                match page {
                    Ok(page) => {
                        delivery.done = page.len() < RETAINED_PAGE;
                        if let Some(last) = page.last() {
                            delivery.after = Some(last.topic.to_string());
                        }
                        delivery.messages = page.into();
                    }
                    Err(e) => {
                        warn!(
                            "Can't look up retained messages for `{}`: {}",
                            delivery.filter, e
                        );
                        continue;
                    }
                }
            }
            let messages = &mut delivery.messages;

            while queue.len() < RETAINED_PAGE {
                let Some(message) = messages.pop_front() else {
                    break;
                };
                if check_source && !publisher_may_write(chain, &message).await {
                    continue;
                }
                if !delivery.subscriber.deliver_retained(message.clone()) {
                    // An empty queue that refuses it never takes it
                    if queue.len() == 0 {
                        self.count_drop(match message.qos {
                            QoS::AtMostOnce => DropCause::QueueFullQos0,
                            _ => DropCause::QueueFull,
                        });
                        warn!(
                            "Retained message on `{}` doesn't fit the message queue, dropping it",
                            message.topic
                        );
                        continue;
                    }
                    messages.push_front(message);
                    break;
                }
            }

            let queue_full = !delivery.messages.is_empty();
            if queue_full || !delivery.done {
                // Unless the client unsubscribed or subscribed again meanwhile
                let mut state = self.shared.state.lock().await;
                if state.subscriptions.contains_key(&delivery.filter)
                    && !state.retained.iter().any(|d| d.filter == delivery.filter)
                {
                    state.retained.push_front(delivery);
                }
                if queue_full {
                    return;
                }
            }
        }
    }
}
