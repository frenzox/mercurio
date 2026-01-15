use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use tokio::sync::broadcast;

use crate::topic_tree::TopicTree;
use mercurio_core::{message::Message, Result};

/// Message broker that handles pub/sub routing.
///
/// The broker maintains a topic tree for efficient message routing with wildcard support.
/// It also stores retained messages per topic for delivery to new subscribers.
/// It is thread-safe and can be cloned cheaply (uses Arc internally).
#[derive(Debug, Clone)]
pub(crate) struct Broker {
    subscriptions: TopicTree<Message>,
    /// Retained messages indexed by exact topic name
    retained: Arc<RwLock<HashMap<String, Message>>>,
}

impl Broker {
    pub(crate) fn new() -> Broker {
        Broker {
            subscriptions: TopicTree::new(),
            retained: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Subscribe to a topic filter. Returns a receiver for matching messages
    /// and any retained messages that match the filter.
    pub(crate) fn subscribe(&self, topic: &str) -> (broadcast::Receiver<Message>, Vec<Message>) {
        let receiver = self.subscriptions.subscribe(topic);
        let retained_messages = self.get_matching_retained(topic);
        (receiver, retained_messages)
    }

    /// Publish a message to a topic. Delivers to all matching subscribers.
    /// If the retain flag is set, stores or clears the retained message for this topic.
    pub(crate) fn publish(&self, topic: &str, message: Message) -> Result<()> {
        // Handle retained message storage
        if message.retain {
            let mut retained = self.retained.write().unwrap();
            if message.payload.as_ref().is_none_or(|p| p.is_empty()) {
                // Empty payload clears the retained message
                retained.remove(topic);
            } else {
                // Store the retained message (with retain flag set to false for delivery)
                let mut retained_msg = message.clone();
                retained_msg.retain = false; // Delivered retained messages have retain=false
                retained.insert(topic.to_string(), retained_msg);
            }
        }

        // Deliver to current subscribers
        self.subscriptions.publish(topic, message);
        Ok(())
    }

    /// Get all retained messages matching a topic filter (supports wildcards).
    fn get_matching_retained(&self, filter: &str) -> Vec<Message> {
        let retained = self.retained.read().unwrap();
        let filter_parts: Vec<&str> = filter.split('/').collect();

        retained
            .iter()
            .filter(|(topic, _)| topic_matches_filter(topic, &filter_parts))
            .map(|(_, msg)| {
                // Set retain flag to true when delivering retained messages to new subscribers
                let mut msg = msg.clone();
                msg.retain = true;
                msg
            })
            .collect()
    }
}

/// Check if a topic matches a filter (with wildcard support).
fn topic_matches_filter(topic: &str, filter_parts: &[&str]) -> bool {
    let topic_parts: Vec<&str> = topic.split('/').collect();
    let mut topic_idx = 0;
    let mut filter_idx = 0;

    while filter_idx < filter_parts.len() {
        let filter_part = filter_parts[filter_idx];

        if filter_part == "#" {
            // Multi-level wildcard matches everything remaining
            return true;
        }

        if topic_idx >= topic_parts.len() {
            // Topic is shorter than filter (and no # wildcard)
            return false;
        }

        if filter_part == "+" {
            // Single-level wildcard matches any single level
            topic_idx += 1;
            filter_idx += 1;
        } else if filter_part == topic_parts[topic_idx] {
            // Exact match
            topic_idx += 1;
            filter_idx += 1;
        } else {
            // No match
            return false;
        }
    }

    // Both must be fully consumed for a match
    topic_idx == topic_parts.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use mercurio_core::qos::QoS;
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

    #[test]
    fn test_topic_matches_filter_exact() {
        let filter: Vec<&str> = "a/b/c".split('/').collect();
        assert!(topic_matches_filter("a/b/c", &filter));
        assert!(!topic_matches_filter("a/b", &filter));
        assert!(!topic_matches_filter("a/b/c/d", &filter));
        assert!(!topic_matches_filter("a/b/x", &filter));
    }

    #[test]
    fn test_topic_matches_filter_single_wildcard() {
        let filter: Vec<&str> = "a/+/c".split('/').collect();
        assert!(topic_matches_filter("a/b/c", &filter));
        assert!(topic_matches_filter("a/x/c", &filter));
        assert!(!topic_matches_filter("a/b/c/d", &filter));
        assert!(!topic_matches_filter("a/b/x", &filter));
    }

    #[test]
    fn test_topic_matches_filter_multi_wildcard() {
        let filter: Vec<&str> = "a/#".split('/').collect();
        assert!(topic_matches_filter("a/b", &filter));
        assert!(topic_matches_filter("a/b/c", &filter));
        assert!(topic_matches_filter("a/b/c/d/e", &filter));
        assert!(!topic_matches_filter("b/c", &filter));
    }

    #[test]
    fn test_topic_matches_filter_combined() {
        let filter: Vec<&str> = "a/+/c/#".split('/').collect();
        assert!(topic_matches_filter("a/b/c", &filter));
        assert!(topic_matches_filter("a/x/c/d", &filter));
        assert!(topic_matches_filter("a/y/c/d/e/f", &filter));
        assert!(!topic_matches_filter("a/b/x", &filter));
    }

    #[test]
    fn test_retained_message_stored() {
        let broker = Broker::new();
        let msg = create_test_message("test/topic", "hello", true);

        broker.publish("test/topic", msg).unwrap();

        // Subscribe and check retained message is returned
        let (_, retained) = broker.subscribe("test/topic");
        assert_eq!(retained.len(), 1);
        assert_eq!(
            retained[0].payload.as_ref().unwrap().as_ref(),
            b"hello".as_ref()
        );
        assert!(retained[0].retain); // Retained flag should be true on delivery
    }

    #[test]
    fn test_retained_message_not_stored_without_flag() {
        let broker = Broker::new();
        let msg = create_test_message("test/topic", "hello", false);

        broker.publish("test/topic", msg).unwrap();

        let (_, retained) = broker.subscribe("test/topic");
        assert!(retained.is_empty());
    }

    #[test]
    fn test_retained_message_cleared_with_empty_payload() {
        let broker = Broker::new();

        // First, store a retained message
        let msg = create_test_message("test/topic", "hello", true);
        broker.publish("test/topic", msg).unwrap();

        // Verify it's stored
        let (_, retained) = broker.subscribe("test/topic");
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
        broker.publish("test/topic", clear_msg).unwrap();

        // Verify it's cleared
        let (_, retained) = broker.subscribe("test/topic");
        assert!(retained.is_empty());
    }

    #[test]
    fn test_retained_message_replaced() {
        let broker = Broker::new();

        // Store first message
        let msg1 = create_test_message("test/topic", "first", true);
        broker.publish("test/topic", msg1).unwrap();

        // Replace with second message
        let msg2 = create_test_message("test/topic", "second", true);
        broker.publish("test/topic", msg2).unwrap();

        // Verify only latest is returned
        let (_, retained) = broker.subscribe("test/topic");
        assert_eq!(retained.len(), 1);
        assert_eq!(
            retained[0].payload.as_ref().unwrap().as_ref(),
            b"second".as_ref()
        );
    }

    #[test]
    fn test_retained_message_wildcard_subscription() {
        let broker = Broker::new();

        // Store retained messages on multiple topics
        broker
            .publish("sensors/temp/room1", create_test_message("sensors/temp/room1", "22", true))
            .unwrap();
        broker
            .publish("sensors/temp/room2", create_test_message("sensors/temp/room2", "24", true))
            .unwrap();
        broker
            .publish("sensors/humidity/room1", create_test_message("sensors/humidity/room1", "50", true))
            .unwrap();

        // Subscribe with single-level wildcard
        let (_, retained) = broker.subscribe("sensors/temp/+");
        assert_eq!(retained.len(), 2);

        // Subscribe with multi-level wildcard
        let (_, retained) = broker.subscribe("sensors/#");
        assert_eq!(retained.len(), 3);

        // Exact match
        let (_, retained) = broker.subscribe("sensors/temp/room1");
        assert_eq!(retained.len(), 1);
    }
}
