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

    /// Adds every regular subscriber of this node to `matches` and lets
    /// `choose` pick the member of each share group, given the members and
    /// the index whose turn it is. The turn then passes to the member after
    /// the one chosen, so members `choose` skips don't shift the rotation.
    fn collect(
        &self,
        matches: &mut Matches<T>,
        choose: &mut impl FnMut(&[(u64, T)], usize) -> Option<usize>,
    ) where
        T: Clone,
    {
        matches.regular.extend(self.subscribers.values().cloned());
        for group in self.shared.values() {
            let len = group.members.len();
            let turn = group.next.load(Ordering::Relaxed) % len;
            let next = choose(&group.members, turn).unwrap_or(turn) + 1;
            group.next.store(next % len, Ordering::Relaxed);
            matches.shared_groups += 1;
        }
    }
}

/// The subscribers matching a topic, once per matching filter.
#[derive(Debug)]
pub(crate) struct Matches<T> {
    pub regular: Vec<T>,
    /// How many share groups matched
    pub shared_groups: usize,
}

impl<T> Matches<T> {
    pub fn len(&self) -> usize {
        self.regular.len() + self.shared_groups
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

    /// Returns the regular subscribers of every filter matching `topic`,
    /// once per matching filter. Each matching share group calls `choose`
    /// with its members and the index whose turn it is; it returns the
    /// member chosen, if any. Called under the tree's read lock.
    pub fn matching_with(
        &self,
        topic: &str,
        mut choose: impl FnMut(&[(u64, T)], usize) -> Option<usize>,
    ) -> Matches<T> {
        let mut matches = Matches {
            regular: Vec::new(),
            shared_groups: 0,
        };
        self.walk(topic, |node| node.collect(&mut matches, &mut choose));
        matches
    }

    /// `matching_with` giving each share group's message to the member whose
    /// turn it is.
    #[cfg(test)]
    pub fn matching(&self, topic: &str) -> Matches<T> {
        self.matching_with(topic, |_, turn| Some(turn))
    }

    /// Returns every subscriber accepted by `keep` among those of the
    /// filters matching `topic`, including all members of share groups.
    /// Unlike `matching`, it leaves the share groups' turns alone.
    pub fn matching_where(&self, topic: &str, keep: impl Fn(&T) -> bool) -> Vec<T> {
        let mut found = Vec::new();
        self.walk(topic, |node| {
            let members = node
                .shared
                .values()
                .flat_map(|g| &g.members)
                .map(|(_, m)| m);
            found.extend(
                node.subscribers
                    .values()
                    .chain(members)
                    .filter(|s| keep(s))
                    .cloned(),
            );
        });
        found
    }

    /// Every filter that has subscribers, with how many. A shared
    /// subscription's group is listed as `$share/<group>/<filter>`. Only
    /// counts are taken, so the tree stays read-locked for as short as
    /// possible.
    pub fn filters(&self) -> Vec<(String, usize)> {
        let root = self.root.read().unwrap();
        let mut found = Vec::new();
        let mut stack: Vec<(String, &TopicNode<T>)> = root
            .children
            .iter()
            .map(|(level, node)| (level.clone(), node))
            .collect();
        while let Some((filter, node)) = stack.pop() {
            if !node.subscribers.is_empty() {
                found.push((filter.clone(), node.subscribers.len()));
            }
            for (group, share) in &node.shared {
                found.push((format!("$share/{group}/{filter}"), share.members.len()));
            }
            for (level, child) in &node.children {
                stack.push((format!("{filter}/{level}"), child));
            }
        }
        found
    }

    /// The subscribers of exactly `filter`, which may be a shared
    /// subscription.
    pub fn subscribers_of(&self, filter: &str) -> Vec<T> {
        let (share_name, filter) = split_shared(filter);
        let root = self.root.read().unwrap();
        let mut node = &*root;
        for level in filter.split('/') {
            match node.children.get(level) {
                Some(child) => node = child,
                None => return Vec::new(),
            }
        }
        match share_name {
            None => node.subscribers.values().cloned().collect(),
            Some(name) => node
                .shared
                .get(name)
                .map(|group| group.members.iter().map(|(_, m)| m.clone()).collect())
                .unwrap_or_default(),
        }
    }

    /// Calls `visit` on the node of every filter matching `topic`.
    fn walk(&self, topic: &str, mut visit: impl FnMut(&TopicNode<T>)) {
        let root = self.root.read().unwrap();

        // Use SmallVec to avoid heap allocation for typical topic depths (< 8 levels)
        let mut stack: SmallVec<[(&TopicNode<T>, usize); 8]> = SmallVec::new();
        let levels: SmallVec<[&str; 8]> = topic.split('/').collect();
        let target_depth = levels.len();

        stack.push((&root, 0));

        while let Some((node, depth)) = stack.pop() {
            if depth == target_depth {
                visit(node);

                // "a/#" also matches "a" itself
                if let Some(wildcard) = node.children.get("#") {
                    visit(wildcard);
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
                visit(child);
            }
        }
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

    /// The regular subscribers and the member whose turn it is in each
    /// share group.
    fn matching(tree: &TopicTree<&'static str>, topic: &str) -> Vec<&'static str> {
        let mut chosen = Vec::new();
        let found = tree.matching_with(topic, |members, turn| {
            chosen.push(members[turn].1);
            Some(turn)
        });
        let mut matches = [found.regular, chosen].concat();
        matches.sort();
        matches
    }

    fn shared(tree: &TopicTree<&'static str>, topic: &str) -> Vec<&'static str> {
        let mut chosen = Vec::new();
        tree.matching_with(topic, |members, turn| {
            chosen.push(members[turn].1);
            Some(turn)
        });
        chosen
    }

    #[test]
    fn test_filters_lists_every_filter_with_its_subscribers() {
        let tree = TopicTree::new();
        tree.subscribe("a/#", 1, "one");
        tree.subscribe("a/#", 2, "two");
        tree.subscribe("/b", 1, "one");
        tree.subscribe("$share/g/a/#", 3, "three");

        tree.subscribe("a//b", 1, "one");
        tree.subscribe("#", 1, "one");
        tree.subscribe("+/x", 1, "one");

        let mut filters = tree.filters();
        filters.sort();
        assert_eq!(
            filters,
            [
                ("#".to_string(), 1),
                ("$share/g/a/#".to_string(), 1),
                ("+/x".to_string(), 1),
                ("/b".to_string(), 1),
                ("a/#".to_string(), 2),
                ("a//b".to_string(), 1),
            ]
        );

        let mut subscribers = tree.subscribers_of("a/#");
        subscribers.sort();
        assert_eq!(subscribers, ["one", "two"]);
        assert_eq!(tree.subscribers_of("$share/g/a/#"), ["three"]);
        assert!(tree.subscribers_of("a").is_empty());
        assert!(tree.subscribers_of("$share/h/a/#").is_empty());
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
    fn test_matching_where_leaves_share_turns_alone() {
        let tree = TopicTree::new();
        tree.subscribe("$share/g/a/b", 1, "one");
        tree.subscribe("$share/g/a/b", 2, "two");
        tree.subscribe("a/+", 3, "plain");

        for _ in 0..3 {
            assert_eq!(tree.matching_where("a/b", |m| *m == "two"), ["two"]);
        }
        assert_eq!(shared(&tree, "a/b"), ["one"]);
        assert_eq!(shared(&tree, "a/b"), ["two"]);
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
