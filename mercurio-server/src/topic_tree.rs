use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use smallvec::SmallVec;

#[derive(Debug)]
struct TopicNode<T> {
    /// Subscribers whose filter ends at this node, keyed by subscriber id
    subscribers: HashMap<u64, T>,
    children: HashMap<String, TopicNode<T>>,
}

impl<T> TopicNode<T> {
    fn new() -> TopicNode<T> {
        TopicNode {
            subscribers: HashMap::new(),
            children: HashMap::new(),
        }
    }

    fn is_empty(&self) -> bool {
        self.subscribers.is_empty() && self.children.is_empty()
    }

    /// Removes the subscriber at the end of `levels`, pruning nodes left empty.
    fn remove(&mut self, levels: &[&str], id: u64) -> bool {
        let Some((level, rest)) = levels.split_first() else {
            return self.subscribers.remove(&id).is_some();
        };

        let Some(child) = self.children.get_mut(*level) else {
            return false;
        };
        let removed = child.remove(rest, id);
        if child.is_empty() {
            self.children.remove(*level);
        }
        removed
    }
}

/// Maps topic filters to their subscribers.
#[derive(Debug)]
pub(crate) struct TopicTree<T> {
    root: Arc<RwLock<TopicNode<T>>>,
}

impl<T> Clone for TopicTree<T> {
    fn clone(&self) -> Self {
        TopicTree {
            root: Arc::clone(&self.root),
        }
    }
}

impl<T: Clone> TopicTree<T> {
    pub fn new() -> TopicTree<T> {
        TopicTree {
            root: Arc::new(RwLock::new(TopicNode::new())),
        }
    }

    /// Adds or replaces the subscription of `id` to `filter`.
    pub fn subscribe(&self, filter: &str, id: u64, subscriber: T) {
        let mut root = self.root.write().unwrap();
        let mut node = &mut *root;

        for level in filter.split('/') {
            node = node
                .children
                .entry(level.to_string())
                .or_insert_with(TopicNode::new);
        }

        node.subscribers.insert(id, subscriber);
    }

    /// Removes the subscription of `id` to `filter`. Returns whether it existed.
    pub fn unsubscribe(&self, filter: &str, id: u64) -> bool {
        let levels: SmallVec<[&str; 8]> = filter.split('/').collect();
        self.root.write().unwrap().remove(&levels, id)
    }

    /// Returns the subscribers of every filter matching `topic`, once per
    /// matching filter.
    pub fn matching(&self, topic: &str) -> Vec<T> {
        let root = self.root.read().unwrap();
        let mut matches = Vec::new();

        // Use SmallVec to avoid heap allocation for typical topic depths (< 8 levels)
        let mut stack: SmallVec<[(&TopicNode<T>, usize); 8]> = SmallVec::new();
        let levels: SmallVec<[&str; 8]> = topic.split('/').collect();
        let target_depth = levels.len();

        stack.push((&root, 0));

        while let Some((node, depth)) = stack.pop() {
            if depth == target_depth {
                matches.extend(node.subscribers.values().cloned());

                // "a/#" also matches "a" itself
                if let Some(wildcard) = node.children.get("#") {
                    matches.extend(wildcard.subscribers.values().cloned());
                }
                continue;
            }

            if let Some(child) = node.children.get(levels[depth]) {
                stack.push((child, depth + 1));
            }

            // Wildcards at the first level never match topics starting with
            // `$`, which are reserved for server use [MQTT-4.7.2-1]
            if depth == 0 && topic.starts_with('$') {
                continue;
            }

            if let Some(child) = node.children.get("+") {
                stack.push((child, depth + 1));
            }

            // Multi-level wildcard (#) matches the remainder of the topic
            if let Some(child) = node.children.get("#") {
                matches.extend(child.subscribers.values().cloned());
            }
        }

        matches
    }
}

#[cfg(test)]
mod tests {
    use super::TopicTree;

    fn matching(tree: &TopicTree<&'static str>, topic: &str) -> Vec<&'static str> {
        let mut matches = tree.matching(topic);
        matches.sort();
        matches
    }

    #[test]
    fn test_normal_topics() {
        let tree = TopicTree::new();
        tree.subscribe("a/b/c", 1, "abc");
        tree.subscribe("/a/b/c", 2, "/abc");

        assert_eq!(matching(&tree, "a/b/c"), ["abc"]);
        assert_eq!(matching(&tree, "/a/b/c"), ["/abc"]);
        assert!(matching(&tree, "a/b").is_empty());
    }

    #[test]
    fn test_multi_level_wildcard() {
        let tree = TopicTree::new();
        tree.subscribe("sport/tennis/player1/#", 1, "player1");
        tree.subscribe("sport/#", 2, "sport");

        assert_eq!(
            matching(&tree, "sport/tennis/player1"),
            ["player1", "sport"]
        );
        assert_eq!(
            matching(&tree, "sport/tennis/player1/score/wimbledon"),
            ["player1", "sport"]
        );
        assert_eq!(matching(&tree, "sport"), ["sport"]);
        assert!(matching(&tree, "music").is_empty());
    }

    #[test]
    fn test_single_level_wildcard() {
        let tree = TopicTree::new();
        tree.subscribe("sport/tennis/+", 1, "plus");
        tree.subscribe("sport/tennis/+/ranking", 2, "ranking");

        assert_eq!(matching(&tree, "sport/tennis/player1"), ["plus"]);
        assert_eq!(matching(&tree, "sport/tennis/player1/ranking"), ["ranking"]);
        assert!(matching(&tree, "sport/tennis").is_empty());
        assert_eq!(matching(&tree, "sport/tennis/"), ["plus"]);
    }

    #[test]
    fn test_dollar_topics_skip_leading_wildcards() {
        let tree = TopicTree::new();
        tree.subscribe("#", 1, "all");
        tree.subscribe("+/monitor/clients", 2, "plus");
        tree.subscribe("$SYS/#", 3, "sys");
        tree.subscribe("$SYS/monitor/+", 4, "sys-plus");

        assert_eq!(matching(&tree, "$SYS/monitor/clients"), ["sys", "sys-plus"]);
        assert_eq!(matching(&tree, "a/monitor/clients"), ["all", "plus"]);
    }

    #[test]
    fn test_resubscribe_replaces_and_unsubscribe_prunes() {
        let tree = TopicTree::new();
        tree.subscribe("a/+/c", 1, "first");
        tree.subscribe("a/+/c", 1, "second");
        tree.subscribe("a/b", 2, "other");

        assert_eq!(matching(&tree, "a/x/c"), ["second"]);

        assert!(tree.unsubscribe("a/+/c", 1));
        assert!(!tree.unsubscribe("a/+/c", 1));
        assert!(matching(&tree, "a/x/c").is_empty());

        let root = tree.root.read().unwrap();
        assert!(!root.children["a"].children.contains_key("+"));
        drop(root);

        assert!(tree.unsubscribe("a/b", 2));
        assert!(tree.root.read().unwrap().is_empty());
    }
}
