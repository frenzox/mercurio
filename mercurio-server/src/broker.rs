use std::{collections::HashMap, sync::Arc};

use crate::{
    queue::Subscriber,
    sys::{is_sys, DropCause, Stats, SysOverlay},
    topic_tree::TopicTree,
};
use mercurio_core::{message::Message, Result};
use mercurio_storage::RetainedMessageStore;
use tracing::warn;

/// Message broker that handles pub/sub routing.
///
/// The broker maintains a topic tree for efficient message routing with wildcard support.
/// It uses a pluggable storage backend for retained messages.
/// It is thread-safe and can be cloned cheaply (uses Arc internally).
pub(crate) struct Broker<S: RetainedMessageStore> {
    subscriptions: TopicTree<Subscriber>,
    /// Storage backend for retained messages, `$SYS` ones in memory
    storage: Arc<SysOverlay<S>>,
    stats: Arc<Stats>,
}

impl<S: RetainedMessageStore> Clone for Broker<S> {
    fn clone(&self) -> Self {
        Broker {
            subscriptions: self.subscriptions.clone(),
            storage: Arc::clone(&self.storage),
            stats: Arc::clone(&self.stats),
        }
    }
}

impl<S: RetainedMessageStore> Broker<S> {
    pub(crate) fn new(storage: Arc<S>) -> Broker<S> {
        Broker {
            subscriptions: TopicTree::new(),
            storage: Arc::new(SysOverlay::new(storage)),
            stats: Default::default(),
        }
    }

    /// The retained messages, `$SYS` ones included, for the admin API.
    pub(crate) fn retained_store(&self) -> Arc<dyn RetainedMessageStore>
    where
        S: 'static,
    {
        self.storage.clone()
    }

    /// Removes the `$SYS` messages an earlier version stored.
    pub(crate) async fn forget_stored_sys(&self) {
        if let Err(e) = self.storage.forget_stored().await {
            warn!("Can't remove the stored $SYS messages: {}", e);
        }
    }

    /// Counts the messages it loses in `stats`.
    pub(crate) fn counted_in(mut self, stats: Arc<Stats>) -> Self {
        self.stats = stats;
        self
    }

    /// Adds or replaces the subscription of `id` to a topic filter.
    pub(crate) fn subscribe(&self, filter: &str, id: u64, subscriber: Subscriber) {
        self.subscriptions.subscribe(filter, id, subscriber);
    }

    /// The retained messages matching a topic filter. Looked up apart from
    /// subscribing, as most subscriptions are sent none: shared ones, those
    /// restored, and those asking not to be.
    pub(crate) async fn retained(&self, filter: &str) -> Result<Vec<Message>> {
        self.storage
            .get_retained(filter)
            .await
            .map_err(|e| mercurio_core::error::Error::Storage(e.to_string()))
    }

    /// Whether `max` topics hold retained messages already, `topic` not one
    /// of them, so retaining another would exceed it.
    // ponytail: counts the store each time, a COUNT(*) on PostgreSQL; keep a
    // count in memory if retained publishes get frequent under a cap
    pub(crate) async fn retained_full(&self, topic: &str, max: usize) -> Result<bool> {
        if is_sys(topic) {
            return Ok(false);
        }
        let storage_error =
            |e: mercurio_storage::StorageError| mercurio_core::error::Error::Storage(e.to_string());
        if self.storage.count_retained().await.map_err(storage_error)? < max {
            return Ok(false);
        }
        let existing = self
            .storage
            .get_retained(topic)
            .await
            .map_err(storage_error)?;
        Ok(existing.is_empty())
    }

    /// Removes the subscription of `id` to a topic filter. Returns whether it
    /// existed.
    pub(crate) fn unsubscribe(&self, filter: &str, id: u64) -> bool {
        self.subscriptions.unsubscribe(filter, id)
    }

    /// Handle to the subscriptions, for removing them without a broker.
    /// How many retained messages are stored; 0 if the storage fails.
    pub(crate) async fn retained_count(&self) -> usize {
        self.storage.count_retained().await.unwrap_or(0)
    }

    pub(crate) fn topics(&self) -> TopicTree<Subscriber> {
        self.subscriptions.clone()
    }

    /// Publish a message to a topic. Delivers to all matching subscribers.
    /// If the retain flag is set, stores or clears the retained message for this topic.
    pub(crate) async fn publish(&self, topic: &str, message: Message) -> Result<()> {
        self.publish_from(topic, message, None).await.map(|_| ())
    }

    /// Publishes a message on behalf of the session `origin`, if any.
    /// Returns how many subscriptions matched it.
    pub(crate) async fn publish_from(
        &self,
        topic: &str,
        message: Message,
        origin: Option<u64>,
    ) -> Result<usize> {
        // Handle retained message storage
        if message.retain {
            if message.payload.as_ref().is_none_or(|p| p.is_empty()) {
                // Empty payload clears the retained message
                self.storage
                    .store_retained(topic, None)
                    .await
                    .map_err(|e| mercurio_core::error::Error::Storage(e.to_string()))?;
            } else {
                // Store the retained message (with retain flag set to false for storage)
                let mut retained_msg = message.clone();
                retained_msg.retain = false;
                self.storage
                    .store_retained(topic, Some(retained_msg))
                    .await
                    .map_err(|e| mercurio_core::error::Error::Storage(e.to_string()))?;
            }
        }

        // Each share group's message goes to the next connected member whose
        // queue takes it, and only to a disconnected one, whose queue waits
        // for its client to return, when no connected member can
        let matches = self.subscriptions.matching_with(topic, |members, turn| {
            let order = (0..members.len()).map(|i| (turn + i) % members.len());
            let offer = |want_connected: bool| {
                order.clone().find(|&i| {
                    let member = &members[i].1;
                    member.queue.is_connected() == want_connected
                        && Subscriber::offer(std::slice::from_ref(member), message.clone(), origin)
                })
            };
            let chosen = offer(true).or_else(|| offer(false));
            if chosen.is_none() {
                self.stats.dropped(DropCause::NoShareMember, 1, None);
            }
            chosen
        });
        // A session with overlapping subscriptions gets one copy
        let mut by_session: HashMap<u64, Vec<Subscriber>> = HashMap::new();
        for subscriber in &matches.regular {
            by_session
                .entry(subscriber.session_id)
                .or_default()
                .push(subscriber.clone());
        }
        for subscriptions in by_session.values() {
            Subscriber::deliver(subscriptions, message.clone(), origin);
        }
        Ok(matches.len())
    }

    /// Delivers a message only to the session `session_id`, through its
    /// subscriptions matching `topic`. Nothing is retained.
    pub(crate) fn publish_to(&self, topic: &str, message: Message, session_id: u64) {
        let own = self
            .subscriptions
            .matching_where(topic, |s| s.session_id == session_id);
        Subscriber::deliver(&own, message, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::{MessageQueue, QueueLimits};
    use bytes::Bytes;
    use mercurio_core::qos::QoS;
    use mercurio_storage::memory::MemoryStore;
    use std::sync::Arc;

    fn create_test_message(topic: &str, payload: &str, retain: bool) -> Message {
        Message {
            packet_id: None,
            topic: Arc::from(topic),
            dup: false,
            qos: QoS::AtMostOnce,
            retain,
            payload: Some(Bytes::from(payload.to_string())),
            properties: Default::default(),
            expires_at: None,
            subscription_identifiers: Vec::new(),
            origin: None,
        }
    }

    fn share_member(id: u64, limits: QueueLimits, connected: bool) -> Subscriber {
        let queue = Arc::new(MessageQueue::new(limits));
        queue.set_connected(connected);
        Subscriber {
            queue,
            session_id: id,
            granted_qos: QoS::AtLeastOnce,
            no_local: false,
            retain_as_published: false,
            subscription_identifier: None,
        }
    }

    #[tokio::test]
    async fn test_shared_subscriptions_prefer_connected_members() {
        let broker = Broker::new(Arc::new(MemoryStore::new()));
        let online = share_member(1, QueueLimits::default(), true);
        let offline = share_member(2, QueueLimits::default(), false);
        broker.subscribe("$share/g/jobs", 1, online.clone());
        broker.subscribe("$share/g/jobs", 2, offline.clone());

        for _ in 0..4 {
            let matched = broker
                .publish_from("jobs", create_test_message("jobs", "x", false), None)
                .await
                .unwrap();
            assert_eq!(matched, 1);
        }
        assert_eq!(online.queue.len(), 4);
        assert_eq!(offline.queue.len(), 0);
    }

    #[tokio::test]
    async fn test_shared_subscriptions_skip_full_members() {
        let broker = Broker::new(Arc::new(MemoryStore::new()));
        let small = QueueLimits {
            max_qos0: 1,
            ..Default::default()
        };
        let full = share_member(1, small, true);
        let roomy = share_member(2, QueueLimits::default(), true);
        let offline = share_member(3, QueueLimits::default(), false);
        broker.subscribe("$share/g/jobs", 1, full.clone());
        broker.subscribe("$share/g/jobs", 2, roomy.clone());
        broker.subscribe("$share/g/jobs", 3, offline.clone());

        for _ in 0..4 {
            broker
                .publish("jobs", create_test_message("jobs", "x", false))
                .await
                .unwrap();
        }
        // The first went to the small queue, the rest past it to the roomy one
        assert_eq!(full.queue.len(), 1);
        assert_eq!(roomy.queue.len(), 3);
        assert_eq!(offline.queue.len(), 0);

        // With no connected member able to take it, an offline one does
        roomy.queue.set_connected(false);
        broker
            .publish("jobs", create_test_message("jobs", "x", false))
            .await
            .unwrap();
        assert_eq!(full.queue.len(), 1);
        assert_eq!(roomy.queue.len() + offline.queue.len(), 4);
    }

    #[tokio::test]
    async fn test_message_no_share_member_takes_is_counted_once() {
        let stats = Arc::new(Stats::default());
        let broker = Broker::new(Arc::new(MemoryStore::new())).counted_in(Arc::clone(&stats));
        let small = QueueLimits {
            max_qos0: 1,
            ..Default::default()
        };
        for id in 1..=2 {
            let mut member = share_member(id, small, true);
            member.queue = Arc::new(MessageQueue::new(small).counted_in(Arc::clone(&stats)));
            member.queue.set_connected(true);
            broker.subscribe("$share/g/jobs", id, member);
        }
        for _ in 0..3 {
            broker
                .publish("jobs", create_test_message("jobs", "x", false))
                .await
                .unwrap();
        }

        let counters = crate::sys::counters(&stats, 0, 0, 0, 0, (0, 0));
        let stat = |name: &str| counters.iter().find(|(n, _)| *n == name).unwrap().1;
        assert_eq!(stat("publish/messages/dropped/no_share_member"), 1);
        assert_eq!(stat("publish/messages/dropped"), 1);
    }

    #[tokio::test]
    async fn test_shared_subscriptions_take_turns_among_eligible_members() {
        let broker = Broker::new(Arc::new(MemoryStore::new()));
        let members: Vec<Subscriber> = (1..=3)
            .map(|id| share_member(id, QueueLimits::default(), id != 2))
            .collect();
        for member in &members {
            broker.subscribe("$share/g/jobs", member.session_id, member.clone());
        }

        for _ in 0..6 {
            broker
                .publish("jobs", create_test_message("jobs", "x", false))
                .await
                .unwrap();
        }
        let counts: Vec<usize> = members.iter().map(|m| m.queue.len()).collect();
        assert_eq!(counts, [3, 0, 3]);
    }

    #[tokio::test]
    async fn test_retained_message_stored() {
        let broker = Broker::new(Arc::new(MemoryStore::new()));
        let msg = create_test_message("test/topic", "hello", true);

        broker.publish("test/topic", msg).await.unwrap();

        // Subscribe and check retained message is returned
        let retained = broker.retained("test/topic").await.unwrap();
        assert_eq!(retained.len(), 1);
        assert_eq!(
            retained[0].payload.as_ref().unwrap().as_ref(),
            b"hello".as_ref()
        );
        assert!(retained[0].retain); // Retained flag should be true on delivery
    }

    #[tokio::test]
    async fn test_sys_messages_are_retained_in_memory_only() {
        let storage = Arc::new(MemoryStore::new());
        let broker = Broker::new(Arc::clone(&storage));
        let topic = "$SYS/broker/connection/bridge/state";
        broker
            .publish(topic, create_test_message(topic, "1", true))
            .await
            .unwrap();

        assert_eq!(storage.count_retained().await.unwrap(), 0);
        let retained = broker.retained("$SYS/#").await.unwrap();
        assert_eq!(retained.len(), 1);
        assert!(retained[0].retain);
        assert!(broker.retained("#").await.unwrap().is_empty());
        // Not counted toward the limit, nor limited by it
        broker
            .publish("a", create_test_message("a", "x", true))
            .await
            .unwrap();
        assert!(!broker.retained_full("$SYS/broker/new", 1).await.unwrap());
        assert!(broker.retained_full("b", 1).await.unwrap());

        broker
            .publish(topic, create_test_message(topic, "", true))
            .await
            .unwrap();
        assert!(broker.retained("$SYS/#").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_retained_message_not_stored_without_flag() {
        let broker = Broker::new(Arc::new(MemoryStore::new()));
        let msg = create_test_message("test/topic", "hello", false);

        broker.publish("test/topic", msg).await.unwrap();

        let retained = broker.retained("test/topic").await.unwrap();
        assert!(retained.is_empty());
    }

    #[tokio::test]
    async fn test_retained_message_cleared_with_empty_payload() {
        let broker = Broker::new(Arc::new(MemoryStore::new()));

        // First, store a retained message
        let msg = create_test_message("test/topic", "hello", true);
        broker.publish("test/topic", msg).await.unwrap();

        // Verify it's stored
        let retained = broker.retained("test/topic").await.unwrap();
        assert_eq!(retained.len(), 1);

        // Clear with empty payload
        let clear_msg = Message {
            packet_id: None,
            topic: Arc::from("test/topic"),
            dup: false,
            qos: QoS::AtMostOnce,
            retain: true,
            payload: Some(Bytes::new()), // Empty payload
            properties: Default::default(),
            expires_at: None,
            subscription_identifiers: Vec::new(),
            origin: None,
        };
        broker.publish("test/topic", clear_msg).await.unwrap();

        // Verify it's cleared
        let retained = broker.retained("test/topic").await.unwrap();
        assert!(retained.is_empty());
    }

    #[tokio::test]
    async fn test_retained_message_replaced() {
        let broker = Broker::new(Arc::new(MemoryStore::new()));

        // Store first message
        let msg1 = create_test_message("test/topic", "first", true);
        broker.publish("test/topic", msg1).await.unwrap();

        // Replace with second message
        let msg2 = create_test_message("test/topic", "second", true);
        broker.publish("test/topic", msg2).await.unwrap();

        // Verify only latest is returned
        let retained = broker.retained("test/topic").await.unwrap();
        assert_eq!(retained.len(), 1);
        assert_eq!(
            retained[0].payload.as_ref().unwrap().as_ref(),
            b"second".as_ref()
        );
    }

    #[tokio::test]
    async fn test_retained_message_wildcard_subscription() {
        let broker = Broker::new(Arc::new(MemoryStore::new()));

        // Store retained messages on multiple topics
        broker
            .publish(
                "sensors/temp/room1",
                create_test_message("sensors/temp/room1", "22", true),
            )
            .await
            .unwrap();
        broker
            .publish(
                "sensors/temp/room2",
                create_test_message("sensors/temp/room2", "24", true),
            )
            .await
            .unwrap();
        broker
            .publish(
                "sensors/humidity/room1",
                create_test_message("sensors/humidity/room1", "50", true),
            )
            .await
            .unwrap();

        // Subscribe with single-level wildcard
        let retained = broker.retained("sensors/temp/+").await.unwrap();
        assert_eq!(retained.len(), 2);

        // Subscribe with multi-level wildcard
        let retained = broker.retained("sensors/#").await.unwrap();
        assert_eq!(retained.len(), 3);

        // Exact match
        let retained = broker.retained("sensors/temp/room1").await.unwrap();
        assert_eq!(retained.len(), 1);
    }
}
