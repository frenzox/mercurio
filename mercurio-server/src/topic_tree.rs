use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use smallvec::SmallVec;
use tokio::sync::broadcast;

#[derive(Debug)]
struct TopicNode<T: Clone> {
    channel: broadcast::Sender<T>,
    children: HashMap<String, TopicNode<T>>,
}

impl<T: Clone> TopicNode<T> {
    fn new() -> TopicNode<T> {
        // TODO: Make channel capacity configurable
        let (sender, _) = broadcast::channel(16);

        TopicNode {
            channel: sender,
            children: HashMap::new(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct TopicTree<T: Clone> {
    shared: Arc<Shared<T>>,
}

#[derive(Debug)]
struct Shared<T: Clone> {
    state: RwLock<State<T>>,
}

#[derive(Debug)]
struct State<T: Clone> {
    root: TopicNode<T>,
}

impl<T: Clone> Clone for TopicTree<T> {
    fn clone(&self) -> Self {
        TopicTree {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<T: Clone> TopicTree<T> {
    pub fn new() -> TopicTree<T> {
        TopicTree {
            shared: Arc::new(Shared {
                state: RwLock::new(State {
                    root: TopicNode::new(),
                }),
            }),
        }
    }

    /// Subscribe to a topic filter. Returns a receiver for messages matching the filter.
    ///
    /// This method acquires a write lock as it may create new nodes in the tree.
    pub fn subscribe(&self, topic: &str) -> broadcast::Receiver<T> {
        let mut state = self.shared.state.write().unwrap();
        let mut node = &mut state.root;

        for level in topic.split('/') {
            node = node
                .children
                .entry(level.to_string())
                .or_insert_with(TopicNode::new);
        }

        node.channel.subscribe()
    }

    /// Publish a message to a topic.
    ///
    /// This method only acquires a read lock since it doesn't modify the tree structure.
    /// Messages are sent to all subscribers whose topic filters match the topic.
    pub fn publish(&self, topic: &str, value: T) {
        let state = self.shared.state.read().unwrap();

        // Use SmallVec to avoid heap allocation for typical topic depths (< 8 levels)
        // Stack holds (node reference, current depth in topic)
        let mut stack: SmallVec<[(&TopicNode<T>, usize); 8]> = SmallVec::new();

        // Collect topic levels into SmallVec (avoids heap for < 8 levels)
        let levels: SmallVec<[&str; 8]> = topic.split('/').collect();
        let target_depth = levels.len();

        stack.push((&state.root, 0));

        while let Some((node, depth)) = stack.pop() {
            if depth == target_depth {
                // Reached the target depth - send to subscribers at this node
                let _ = node.channel.send(value.clone());

                // Also send to multi-level wildcard (#) subscribers at the next level
                if let Some(wildcard) = node.children.get("#") {
                    let _ = wildcard.channel.send(value.clone());
                }
                continue;
            }

            let level = levels[depth];
            let next_depth = depth + 1;

            // Check for exact match
            if let Some(child) = node.children.get(level) {
                stack.push((child, next_depth));
            }

            // Check for single-level wildcard (+)
            if let Some(child) = node.children.get("+") {
                stack.push((child, next_depth));
            }

            // Multi-level wildcard (#) matches the remainder of the topic
            if let Some(child) = node.children.get("#") {
                let _ = child.channel.send(value.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time::timeout;

    use super::TopicTree;

    #[tokio::test]
    async fn test_pubsub_normal_topics() {
        let tree = TopicTree::<String>::new();
        let mut subscriber = tree.subscribe("a/b/c");
        let mut subscriber2 = tree.subscribe("/a/b/c");

        tree.publish("a/b/c", "test_message".to_string());
        tree.publish("/a/b/c", "test_message2".to_string());

        assert_eq!(
            timeout(Duration::from_millis(10), subscriber.recv())
                .await
                .unwrap()
                .unwrap(),
            "test_message".to_string()
        );

        assert_eq!(
            timeout(Duration::from_millis(10), subscriber2.recv())
                .await
                .unwrap()
                .unwrap(),
            "test_message2".to_string()
        );

        timeout(Duration::from_millis(10), subscriber.recv())
            .await
            .expect_err("Expected Elapsed error");

        timeout(Duration::from_millis(10), subscriber2.recv())
            .await
            .expect_err("Expected Elapsed error");
    }

    #[tokio::test]
    async fn test_pubsub_multi_level_wildcard() {
        let tree = TopicTree::<String>::new();

        let mut subscriber = tree.subscribe("sport/tennis/player1/#");
        tree.publish("sport/tennis/player1", "test_message".into());

        tree.publish("sport/tennis/player1/ranking", "test_message_1".into());

        tree.publish(
            "sport/tennis/player1/score/wimbledon",
            "test_message_2".into(),
        );

        assert_eq!(
            timeout(Duration::from_millis(10), subscriber.recv())
                .await
                .unwrap()
                .unwrap(),
            "test_message".to_string()
        );

        assert_eq!(
            timeout(Duration::from_millis(10), subscriber.recv())
                .await
                .unwrap()
                .unwrap(),
            "test_message_1".to_string()
        );

        assert_eq!(
            timeout(Duration::from_millis(10), subscriber.recv())
                .await
                .unwrap()
                .unwrap(),
            "test_message_2".to_string()
        );

        let mut subscriber = tree.subscribe("sport/#");
        tree.publish("sport", "test_message_3".into());

        assert_eq!(
            timeout(Duration::from_millis(10), subscriber.recv())
                .await
                .unwrap()
                .unwrap(),
            "test_message_3".to_string()
        );
    }

    #[tokio::test]
    async fn test_pubsub_single_level_wildcard() {
        let tree = TopicTree::<String>::new();
        let mut subscriber = tree.subscribe("sport/tennis/+");
        let mut subscriber2 = tree.subscribe("sport/tennis/+/ranking");
        tree.publish("sport/tennis/player1", "test_message".into());
        tree.publish("sport/tennis/player1/ranking", "test_message".into());
        tree.publish("sport/tennis", "test_message".into());

        assert_eq!(
            timeout(Duration::from_millis(10), subscriber.recv())
                .await
                .unwrap()
                .unwrap(),
            "test_message".to_string()
        );

        assert_eq!(
            timeout(Duration::from_millis(10), subscriber2.recv())
                .await
                .unwrap()
                .unwrap(),
            "test_message".to_string()
        );

        timeout(Duration::from_millis(10), subscriber.recv())
            .await
            .expect_err("Expected Elapsed error");

        timeout(Duration::from_millis(10), subscriber.recv())
            .await
            .expect_err("Expected Elapsed error");

        tree.publish("sport/tennis/", "test_message".into());

        assert_eq!(
            timeout(Duration::from_millis(10), subscriber.recv())
                .await
                .unwrap()
                .unwrap(),
            "test_message".to_string()
        );
    }
}
