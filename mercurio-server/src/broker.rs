use std::{collections::HashMap, sync::Arc};

use crate::{queue::Subscriber, topic_tree::TopicTree};
use mercurio_core::{message::Message, Result};
use mercurio_storage::RetainedMessageStore;

/// Message broker that handles pub/sub routing.
///
/// The broker maintains a topic tree for efficient message routing with wildcard support.
/// It uses a pluggable storage backend for retained messages.
/// It is thread-safe and can be cloned cheaply (uses Arc internally).
pub(crate) struct Broker<S: RetainedMessageStore> {
    subscriptions: TopicTree<Subscriber>,
    /// Storage backend for retained messages
    storage: Arc<S>,
}

impl<S: RetainedMessageStore> Clone for Broker<S> {
    fn clone(&self) -> Self {
        Broker {
            subscriptions: self.subscriptions.clone(),
            storage: Arc::clone(&self.storage),
        }
    }
}

impl<S: RetainedMessageStore> Broker<S> {
    pub(crate) fn new(storage: Arc<S>) -> Broker<S> {
        Broker {
            subscriptions: TopicTree::new(),
            storage,
        }
    }

    /// Subscribes `id` to a topic filter, replacing any previous subscription
    /// of `id` to the same filter. Returns the retained messages matching it.
    pub(crate) async fn subscribe(
        &self,
        filter: &str,
        id: u64,
        subscriber: Subscriber,
    ) -> Result<Vec<Message>> {
        self.subscriptions.subscribe(filter, id, subscriber);
        self.storage
            .get_retained(filter)
            .await
            .map_err(|e| mercurio_core::error::Error::Storage(e.to_string()))
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

        let matches = self.subscriptions.matching(topic);
        // A session with overlapping subscriptions gets one copy; shared
        // subscriptions are delivered on their own
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
        for subscriber in &matches.shared {
            Subscriber::deliver(std::slice::from_ref(subscriber), message.clone(), origin);
        }
        Ok(matches.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use mercurio_core::qos::QoS;
    use mercurio_storage::memory::MemoryStore;
    use std::sync::Arc;

    fn subscriber() -> Subscriber {
        Subscriber {
            queue: Default::default(),
            session_id: 0,
            granted_qos: QoS::AtMostOnce,
            no_local: false,
            retain_as_published: false,
            subscription_identifier: None,
        }
    }

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

    #[tokio::test]
    async fn test_retained_message_stored() {
        let broker = Broker::new(Arc::new(MemoryStore::new()));
        let msg = create_test_message("test/topic", "hello", true);

        broker.publish("test/topic", msg).await.unwrap();

        // Subscribe and check retained message is returned
        let retained = broker
            .subscribe("test/topic", 0, subscriber())
            .await
            .unwrap();
        assert_eq!(retained.len(), 1);
        assert_eq!(
            retained[0].payload.as_ref().unwrap().as_ref(),
            b"hello".as_ref()
        );
        assert!(retained[0].retain); // Retained flag should be true on delivery
    }

    #[tokio::test]
    async fn test_retained_message_not_stored_without_flag() {
        let broker = Broker::new(Arc::new(MemoryStore::new()));
        let msg = create_test_message("test/topic", "hello", false);

        broker.publish("test/topic", msg).await.unwrap();

        let retained = broker
            .subscribe("test/topic", 0, subscriber())
            .await
            .unwrap();
        assert!(retained.is_empty());
    }

    #[tokio::test]
    async fn test_retained_message_cleared_with_empty_payload() {
        let broker = Broker::new(Arc::new(MemoryStore::new()));

        // First, store a retained message
        let msg = create_test_message("test/topic", "hello", true);
        broker.publish("test/topic", msg).await.unwrap();

        // Verify it's stored
        let retained = broker
            .subscribe("test/topic", 0, subscriber())
            .await
            .unwrap();
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
        let retained = broker
            .subscribe("test/topic", 0, subscriber())
            .await
            .unwrap();
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
        let retained = broker
            .subscribe("test/topic", 0, subscriber())
            .await
            .unwrap();
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
        let retained = broker
            .subscribe("sensors/temp/+", 0, subscriber())
            .await
            .unwrap();
        assert_eq!(retained.len(), 2);

        // Subscribe with multi-level wildcard
        let retained = broker
            .subscribe("sensors/#", 0, subscriber())
            .await
            .unwrap();
        assert_eq!(retained.len(), 3);

        // Exact match
        let retained = broker
            .subscribe("sensors/temp/room1", 0, subscriber())
            .await
            .unwrap();
        assert_eq!(retained.len(), 1);
    }
}
