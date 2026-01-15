use std::sync::Arc;

use tokio::sync::broadcast;

use crate::topic_tree::TopicTree;
use mercurio_core::{message::Message, Result};
use mercurio_storage::RetainedMessageStore;

/// Message broker that handles pub/sub routing.
///
/// The broker maintains a topic tree for efficient message routing with wildcard support.
/// It uses a pluggable storage backend for retained messages.
/// It is thread-safe and can be cloned cheaply (uses Arc internally).
#[derive(Clone)]
pub(crate) struct Broker<S: RetainedMessageStore> {
    subscriptions: TopicTree<Message>,
    /// Storage backend for retained messages
    storage: Arc<S>,
}

impl<S: RetainedMessageStore> Broker<S> {
    pub(crate) fn new(storage: Arc<S>) -> Broker<S> {
        Broker {
            subscriptions: TopicTree::new(),
            storage,
        }
    }

    /// Subscribe to a topic filter. Returns a receiver for matching messages
    /// and any retained messages that match the filter.
    pub(crate) async fn subscribe(
        &self,
        topic: &str,
    ) -> Result<(broadcast::Receiver<Message>, Vec<Message>)> {
        let receiver = self.subscriptions.subscribe(topic);
        let retained_messages = self
            .storage
            .get_retained(topic)
            .await
            .map_err(|e| mercurio_core::error::Error::Storage(e.to_string()))?;
        Ok((receiver, retained_messages))
    }

    /// Publish a message to a topic. Delivers to all matching subscribers.
    /// If the retain flag is set, stores or clears the retained message for this topic.
    pub(crate) async fn publish(&self, topic: &str, message: Message) -> Result<()> {
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

        // Deliver to current subscribers
        self.subscriptions.publish(topic, message);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        }
    }

    #[tokio::test]
    async fn test_retained_message_stored() {
        let broker = Broker::new(Arc::new(MemoryStore::new()));
        let msg = create_test_message("test/topic", "hello", true);

        broker.publish("test/topic", msg).await.unwrap();

        // Subscribe and check retained message is returned
        let (_, retained) = broker.subscribe("test/topic").await.unwrap();
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

        let (_, retained) = broker.subscribe("test/topic").await.unwrap();
        assert!(retained.is_empty());
    }

    #[tokio::test]
    async fn test_retained_message_cleared_with_empty_payload() {
        let broker = Broker::new(Arc::new(MemoryStore::new()));

        // First, store a retained message
        let msg = create_test_message("test/topic", "hello", true);
        broker.publish("test/topic", msg).await.unwrap();

        // Verify it's stored
        let (_, retained) = broker.subscribe("test/topic").await.unwrap();
        assert_eq!(retained.len(), 1);

        // Clear with empty payload
        let clear_msg = Message {
            packet_id: None,
            topic: Arc::from("test/topic"),
            dup: false,
            qos: QoS::AtMostOnce,
            retain: true,
            payload: Some(Bytes::new()), // Empty payload
        };
        broker.publish("test/topic", clear_msg).await.unwrap();

        // Verify it's cleared
        let (_, retained) = broker.subscribe("test/topic").await.unwrap();
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
        let (_, retained) = broker.subscribe("test/topic").await.unwrap();
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
        let (_, retained) = broker.subscribe("sensors/temp/+").await.unwrap();
        assert_eq!(retained.len(), 2);

        // Subscribe with multi-level wildcard
        let (_, retained) = broker.subscribe("sensors/#").await.unwrap();
        assert_eq!(retained.len(), 3);

        // Exact match
        let (_, retained) = broker.subscribe("sensors/temp/room1").await.unwrap();
        assert_eq!(retained.len(), 1);
    }
}
