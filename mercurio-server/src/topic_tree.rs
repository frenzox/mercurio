use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, RwLock,
    },
};

use smallvec::SmallVec;

use mercurio_core::topic::parse_shared_subscription;

#[derive(Debug)]
struct TopicNode<T> {
    /// Subscribers whose filter ends at this node, keyed by subscriber id
    subscribers: HashMap<u64, T>,
    /// Shared subscriptions ending at this node, by share name
    shared: HashMap<String, ShareGroup<T>>,
    children: HashMap<String, TopicNode<T>>,
}

/// Subscribers sharing a subscription; each message goes to one of them.
#[derive(Debug)]
struct ShareGroup<T> {
    members: Vec<(u64, T)>,
    /// Round-robin position, advanced under the tree's read lock
    next: AtomicUsize,
}

impl<T> TopicNode<T> {
    fn new() -> TopicNode<T> {
        TopicNode {
            subscribers: HashMap::new(),
            shared: HashMap::new(),
            children: HashMap::new(),
        }
    }

    fn is_empty(&self) -> bool {
        self.subscribers.is_empty() && self.shared.is_empty() && self.children.is_empty()
    }

    /// Removes the subscriber at the end of `levels`, pruning nodes left empty.
    fn remove(&mut self, levels: &[&str], id: u64, share_name: Option<&str>) -> bool {
        let Some((level, rest)) = levels.split_first() else {
            let Some(share_name) = share_name else {
                return self.subscribers.remove(&id).is_some();
            };
            let Some(group) = self.shared.get_mut(share_name) else {
                return false;
            };
            let before = group.members.len();
            group.members.retain(|(member, _)| *member != id);
            let removed = group.members.len() != before;
            if group.members.is_empty() {
                self.shared.remove(share_name);
            }
            return removed;
        };

        let Some(child) = self.children.get_mut(*level) else {
            return false;
        };
        let removed = child.remove(rest, id, share_name);
        if child.is_empty() {
            self.children.remove(*level);
        }
        removed
    }

    /// Adds every subscriber of this node to `matches`: all regular ones and
    /// one member of each share group.
    fn collect(&self, matches: &mut Matches<T>)
    where
        T: Clone,
    {
        matches.regular.extend(self.subscribers.values().cloned());
        for group in self.shared.values() {
            let turn = group.next.fetch_add(1, Ordering::Relaxed);
            let (_, member) = &group.members[turn % group.members.len()];
            matches.shared.push(member.clone());
        }
    }
}

/// The subscribers matching a topic, once per matching filter.
#[derive(Debug)]
pub(crate) struct Matches<T> {
    pub regular: Vec<T>,
    /// The member chosen in each matching share group
    pub shared: Vec<T>,
}

impl<T> Matches<T> {
    pub fn len(&self) -> usize {
        self.regular.len() + self.shared.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Maps topic filters to their subscribers.
#[derive(Debug)]
pub(crate) struct TopicTree<T> {
    root: Arc<RwLock<TopicNode<T>>>,
    /// Subscriptions in the tree, kept for statistics
    count: Arc<AtomicUsize>,
}

impl<T> Clone for TopicTree<T> {
    fn clone(&self) -> Self {
        TopicTree {
            root: Arc::clone(&self.root),
            count: Arc::clone(&self.count),
        }
    }
}

impl<T: Clone> TopicTree<T> {
    pub fn new() -> TopicTree<T> {
        TopicTree {
            root: Arc::new(RwLock::new(TopicNode::new())),
            count: Arc::default(),
        }
    }

    /// How many subscriptions the tree holds.
    pub fn subscription_count(&self) -> usize {
        self.count.load(Ordering::Relaxed)
    }

    /// Adds or replaces the subscription of `id` to `filter`, which may be a
    /// shared subscription (`$share/{share name}/{topic filter}`).
    pub fn subscribe(&self, filter: &str, id: u64, subscriber: T) {
        let (share_name, filter) = split_shared(filter);
        let mut root = self.root.write().unwrap();
        let mut node = &mut *root;

        for level in filter.split('/') {
            node = node
                .children
                .entry(level.to_string())
                .or_insert_with(TopicNode::new);
        }

        let Some(share_name) = share_name else {
            if node.subscribers.insert(id, subscriber).is_none() {
                self.count.fetch_add(1, Ordering::Relaxed);
            }
            return;
        };
        let group = node
            .shared
            .entry(share_name.to_string())
            .or_insert_with(|| ShareGroup {
                members: Vec::new(),
                next: AtomicUsize::new(0),
            });
        match group.members.iter_mut().find(|(member, _)| *member == id) {
            Some((_, existing)) => *existing = subscriber,
            None => {
                group.members.push((id, subscriber));
                self.count.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Removes the subscription of `id` to `filter`. Returns whether it existed.
    pub fn unsubscribe(&self, filter: &str, id: u64) -> bool {
        let (share_name, filter) = split_shared(filter);
        let levels: SmallVec<[&str; 8]> = filter.split('/').collect();
        let removed = self.root.write().unwrap().remove(&levels, id, share_name);
        if removed {
            self.count.fetch_sub(1, Ordering::Relaxed);
        }
        removed
    }

    /// Returns the subscribers of every filter matching `topic`, once per
    /// matching filter.
    pub fn matching(&self, topic: &str) -> Matches<T> {
        let root = self.root.read().unwrap();
        let mut matches = Matches {
            regular: Vec::new(),
            shared: Vec::new(),
        };

        // Use SmallVec to avoid heap allocation for typical topic depths (< 8 levels)
        let mut stack: SmallVec<[(&TopicNode<T>, usize); 8]> = SmallVec::new();
        let levels: SmallVec<[&str; 8]> = topic.split('/').collect();
        let target_depth = levels.len();

        stack.push((&root, 0));

        while let Some((node, depth)) = stack.pop() {
            if depth == target_depth {
                node.collect(&mut matches);

                // "a/#" also matches "a" itself
                if let Some(wildcard) = node.children.get("#") {
                    wildcard.collect(&mut matches);
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
                child.collect(&mut matches);
            }
        }

        matches
    }
}

/// The share name and topic filter of a shared subscription, or just the
/// filter for a regular one.
fn split_shared(filter: &str) -> (Option<&str>, &str) {
    match parse_shared_subscription(filter) {
        Some((share_name, filter)) => (Some(share_name), filter),
        None => (None, filter),
    }
}

#[cfg(test)]
mod tests {
    use super::TopicTree;

    fn matching(tree: &TopicTree<&'static str>, topic: &str) -> Vec<&'static str> {
        let found = tree.matching(topic);
        let mut matches = [found.regular, found.shared].concat();
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
    fn test_shared_subscriptions_take_turns() {
        let tree = TopicTree::new();
        tree.subscribe("$share/g/jobs/+", 1, "one");
        tree.subscribe("$share/g/jobs/+", 2, "two");
        tree.subscribe("jobs/#", 3, "plain");

        let mut shared = Vec::new();
        for _ in 0..4 {
            let matches = matching(&tree, "jobs/a");
            assert!(matches.contains(&"plain"));
            assert_eq!(matches.len(), 2);
            shared.extend(matches.into_iter().filter(|m| *m != "plain"));
        }
        shared.sort();
        assert_eq!(shared, ["one", "one", "two", "two"]);

        assert!(tree.unsubscribe("$share/g/jobs/+", 1));
        assert_eq!(matching(&tree, "jobs/a"), ["plain", "two"]);
        assert!(tree.unsubscribe("$share/g/jobs/+", 2));
        assert!(tree.unsubscribe("jobs/#", 3));
        assert!(tree.root.read().unwrap().is_empty());
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
