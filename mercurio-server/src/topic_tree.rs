use std::{
    collections::{HashMap, HashSet, VecDeque},
    hash::{Hash, Hasher},
    sync::{Arc, Mutex},
};

use tokio::sync::broadcast;
use tracing::error;

#[derive(Debug)]
struct TopicNode<T: Clone> {
    channel: broadcast::Sender<T>,
    children: HashMap<String, TopicNode<T>>,
    level: usize,
}

impl<T: Clone> TopicNode<T> {
    pub fn new(level: usize) -> TopicNode<T> {
        let (sender, _) = broadcast::channel(5); // TODO: What size should this actually be?

        TopicNode {
            channel: sender,
            children: HashMap::new(),
            level,
        }
    }
}

impl<T: Clone> Hash for TopicNode<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let children_ptr: *const HashMap<String, TopicNode<T>> = &self.children;
        children_ptr.hash(state);
    }
}
impl<T: Clone> Eq for TopicNode<T> {}
impl<T: Clone> PartialEq for TopicNode<T> {
    fn eq(&self, other: &Self) -> bool {
        self.children.eq(&other.children)
    }
}

#[derive(Debug)]
pub(crate) struct TopicTree<T: Clone> {
    shared: Arc<Shared<T>>,
}

#[derive(Debug)]
struct Shared<T: Clone> {
    state: Mutex<State<T>>,
}

#[derive(Debug)]
struct State<T: Clone> {
    root: TopicNode<T>,
}

impl<T: Clone> TopicTree<T> {
    pub fn new() -> TopicTree<T> {
        TopicTree {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    root: TopicNode::new(usize::MAX),
                }),
            }),
        }
    }

    pub fn subscribe(&mut self, topic: String) -> broadcast::Receiver<T> {
        let levels = topic.split('/');
        let root = &mut self.shared.state.lock().unwrap().root;
        let mut next = root;

        for (idx, level) in levels.enumerate() {
            next = next
                .children
                .entry(level.to_string())
                .or_insert_with(|| TopicNode::new(idx));
        }

        next.channel.subscribe()
    }

    pub fn publish(&mut self, topic: &str, value: T) {
        // TODO: Validate topic name
        let mut visited = HashSet::<&TopicNode<T>>::new();
        let mut stack = VecDeque::<&TopicNode<T>>::new();

        let root = &self.shared.state.lock().unwrap().root;
        let levels: Vec<&str> = topic.split('/').collect();

        stack.push_front(root);

        while !stack.is_empty() {
            let node = stack.pop_front().unwrap(); // Safe to unwrap, otherwise we
                                                   // wouldn't get into the loop
            if node.level == levels.len() - 1 {
                // We reached the last level, send message to subscribers
                if let Err(e) = node.channel.send(value.clone()) {
                    error!("Error publishing value {}", e);
                }

                // Check if there is a children multi-level wildcard sub in the next level,
                // if so send to them too
                if let Some(next) = node.children.get("#") {
                    if let Err(e) = next.channel.send(value) {
                        error!("Error publishing value {}", e);
                    }
                }

                break;
            }

            // DFS alike
            if let Some(next) = node.children.get(levels[node.level.overflowing_add(1).0]) {
                if !visited.contains(next) {
                    visited.insert(next);
                    stack.push_front(next);
                }
            }

            if let Some(next) = node.children.get("+") {
                if !visited.contains(next) {
                    visited.insert(next);
                    stack.push_front(next);
                }
            }

            if let Some(next) = node.children.get("#") {
                if let Err(e) = next.channel.send(value.clone()) {
                    error!("Error publishing value {}", e);
                }
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
        let mut tree = TopicTree::<String>::new();
        let mut subscriber = tree.subscribe("a/b/c".to_string());
        let mut subscriber2 = tree.subscribe("/a/b/c".to_string());

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
        let mut tree = TopicTree::<String>::new();

        let mut subscriber = tree.subscribe("sport/tennis/player1/#".into());
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

        let mut subscriber = tree.subscribe("sport/#".to_string());
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
        let mut tree = TopicTree::<String>::new();
        let mut subscriber = tree.subscribe("sport/tennis/+".into());
        let mut subscriber2 = tree.subscribe("sport/tennis/+/ranking".into());
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
