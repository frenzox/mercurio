use tokio::sync::broadcast;

use crate::topic_tree::TopicTree;
use mercurio_core::{message::Message, Result};

/// Message broker that handles pub/sub routing.
///
/// The broker maintains a topic tree for efficient message routing with wildcard support.
/// It is thread-safe and can be cloned cheaply (uses Arc internally).
#[derive(Debug, Clone)]
pub(crate) struct Broker {
    subscriptions: TopicTree<Message>,
}

impl Broker {
    pub(crate) fn new() -> Broker {
        Broker {
            subscriptions: TopicTree::new(),
        }
    }

    /// Subscribe to a topic filter. Returns a receiver for matching messages.
    pub(crate) fn subscribe(&self, topic: &str) -> broadcast::Receiver<Message> {
        self.subscriptions.subscribe(topic)
    }

    /// Publish a message to a topic. Delivers to all matching subscribers.
    pub(crate) fn publish(&self, topic: &str, message: Message) -> Result<()> {
        self.subscriptions.publish(topic, message);
        Ok(())
    }
}
