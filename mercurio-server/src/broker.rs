use std::{collections::HashMap, sync::Arc};

use crate::{
    queue::Subscriber,
    sys::{is_sys, DropCause, Stats, SysOverlay},
    topic_tree::TopicTree,
};
use mercurio_core::message::Message;

use crate::error::{Error, Result};
use mercurio_storage::RetainedMessageStore;
use tracing::warn;

/// A random number for spreading shares; not for secrets. Each
/// `RandomState` hashes with its own keys, seeded once per thread from the
/// operating system and stepped for every new one.
fn random_u64() -> u64 {
    use std::hash::{BuildHasher, RandomState};
    RandomState::new().hash_one(0u8)
}

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
    share_strategy: ShareStrategy,
    share_redelivery: ShareRedelivery,
}

impl<S: RetainedMessageStore> Clone for Broker<S> {
    fn clone(&self) -> Self {
        Broker {
            subscriptions: self.subscriptions.clone(),
            storage: Arc::clone(&self.storage),
            stats: Arc::clone(&self.stats),
            share_strategy: self.share_strategy,
            share_redelivery: self.share_redelivery,
        }
    }
}

/// When the messages a member of a share group was given and hasn't
/// processed go to another member: those still queued for it, and those
/// at QoS 1 it was sent and hasn't acknowledged. QoS 2 messages it was
/// sent stay with it [MQTT-4.8.2-5]. A QoS 1 message the member processed
/// without acknowledging reaches the application twice.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum ShareRedelivery {
    /// When the member's session ends, as its expiry passes, a clean start
    /// replaces it or it is deleted; until then its messages wait for it
    #[default]
    OnSessionEnd,
    /// Also as soon as the member's connection is lost, to the members
    /// connected then; the rest still waits for the member
    OnDisconnect,
}

/// Gives the messages a departing member of a share group held to
/// another member of the group.
pub(crate) trait Reoffer: Send + Sync {
    /// Offers `message`, which reached the session `except` through the
    /// shared subscription `group`, to another member: one connected, or
    /// with `anyone` a disconnected one when no connected member takes it.
    /// Whether a member took it; one no member takes with `anyone` is lost.
    fn reoffer(&self, group: &str, message: Message, except: u64, anyone: bool) -> bool;
}

impl<S: RetainedMessageStore + 'static> Reoffer for Broker<S> {
    fn reoffer(&self, group: &str, message: Message, except: u64, anyone: bool) -> bool {
        let mut taken = false;
        self.subscriptions.choose_in(group, |members, turn| {
            let chosen = self.share_member(members, turn, None, &message, Some(except), anyone);
            taken = chosen.is_some();
            chosen
        });
        if !taken && anyone {
            self.stats.dropped(DropCause::NoShareMember, 1, None);
        }
        taken
    }
}

/// How a shared subscription's group picks the member a message goes to.
/// Whatever the strategy, a connected member whose queue takes the message
/// comes first; the strategy decides which member is tried first, and the
/// others follow in turn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum ShareStrategy {
    /// Each message to the member after the one that took the last
    #[default]
    RoundRobin,
    /// Each message to a member picked at random
    Random,
    /// Every message to the member that took the last one, for as long as
    /// it is connected and its queue takes them
    Sticky,
    /// The messages of one publishing client to the same member, picked by
    /// a hash of its client id, so they stay in order; messages from the
    /// broker itself, such as wills, go round robin
    Hash,
}

/// The client session a message comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Publisher {
    /// Its subscriber id, for No Local
    pub session: u64,
    /// A hash of its client id, for `ShareStrategy::Hash`
    pub key: u64,
}

impl<S: RetainedMessageStore> Broker<S> {
    pub(crate) fn new(storage: Arc<S>) -> Broker<S> {
        Broker {
            subscriptions: TopicTree::new(),
            storage: Arc::new(SysOverlay::new(storage)),
            stats: Default::default(),
            share_strategy: ShareStrategy::default(),
            share_redelivery: ShareRedelivery::default(),
        }
    }

    /// Picks the members of shared subscriptions with `strategy`.
    pub(crate) fn with_share_strategy(mut self, strategy: ShareStrategy) -> Self {
        self.share_strategy = strategy;
        self
    }

    /// Hands a departing member's messages to the rest of its group as
    /// `redelivery` says.
    pub(crate) fn with_share_redelivery(mut self, redelivery: ShareRedelivery) -> Self {
        self.share_redelivery = redelivery;
        self
    }

    pub(crate) fn share_redelivery(&self) -> ShareRedelivery {
        self.share_redelivery
    }

    /// The member of a share group, of `members` and with `turn` the index
    /// whose turn it is, that takes `message`: the first connected member
    /// whose queue takes it, from where the strategy starts, and only with
    /// `anyone` a disconnected one, whose queue waits for its client to
    /// return, when no connected member can. The session `except` is
    /// never chosen.
    fn share_member(
        &self,
        members: &[(u64, Subscriber)],
        turn: usize,
        publisher: Option<Publisher>,
        message: &Message,
        except: Option<u64>,
        anyone: bool,
    ) -> Option<usize> {
        let len = members.len();
        // `turn` follows the member chosen last
        let start = match (self.share_strategy, publisher) {
            (ShareStrategy::Random, _) => (random_u64() % len as u64) as usize,
            (ShareStrategy::Sticky, _) => (turn + len - 1) % len,
            (ShareStrategy::Hash, Some(publisher)) => (publisher.key % len as u64) as usize,
            _ => turn,
        };
        let origin = publisher.map(|p| p.session);
        let order = (0..len).map(|i| (start + i) % len);
        let offer = |want_connected: bool| {
            order.clone().find(|&i| {
                let member = &members[i].1;
                Some(member.session_id) != except
                    && member.queue.is_connected() == want_connected
                    && Subscriber::offer(std::slice::from_ref(member), message.clone(), origin)
            })
        };
        offer(true).or_else(|| if anyone { offer(false) } else { None })
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

    /// The retained messages matching a topic filter.
    #[cfg(test)]
    pub(crate) async fn retained(&self, filter: &str) -> Result<Vec<Message>> {
        self.storage.get_retained(filter).await.map_err(Error::from)
    }

    /// At most `limit` of the retained messages matching a topic filter, in
    /// topic order, after the topic `after`. Looked up apart from
    /// subscribing, as most subscriptions are sent none: shared ones, those
    /// restored, and those asking not to be.
    pub(crate) async fn retained_page(
        &self,
        filter: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Message>> {
        self.storage
            .get_retained_page(filter, after, limit)
            .await
            .map_err(Error::from)
    }

    /// Whether `max` topics hold retained messages already, `topic` not one
    /// of them, so retaining another would exceed it. The count is kept in
    /// memory, so the store is only asked about `topic` once at the limit.
    pub(crate) async fn retained_full(&self, topic: &str, max: usize) -> Result<bool> {
        if is_sys(topic) {
            return Ok(false);
        }
        let storage_error = Error::from;
        if self.storage.count_retained().await.map_err(storage_error)? < max {
            return Ok(false);
        }
        let existing = self
            .storage
            .get_retained_page(topic, None, 1)
            .await
            .map_err(storage_error)?;
        Ok(existing.is_empty())
    }

    /// Removes the expired retained messages from storage, which lookups
    /// already leave out.
    pub(crate) async fn remove_expired_retained(&self) {
        if let Err(e) = self.storage.remove_expired_retained().await {
            warn!("Can't remove the expired retained messages: {}", e);
        }
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

    /// Publishes a message on behalf of `publisher`, if a client sent it.
    /// Returns how many subscriptions matched it.
    pub(crate) async fn publish_from(
        &self,
        topic: &str,
        message: Message,
        publisher: Option<Publisher>,
    ) -> Result<usize> {
        let origin = publisher.map(|p| p.session);
        // Handle retained message storage
        if message.retain {
            if message.payload.as_ref().is_none_or(|p| p.is_empty()) {
                // Empty payload clears the retained message
                self.storage
                    .store_retained(topic, None)
                    .await
                    .map_err(Error::from)?;
            } else {
                // Store the retained message (with retain flag set to false for storage)
                let mut retained_msg = message.clone();
                retained_msg.retain = false;
                self.storage
                    .store_retained(topic, Some(retained_msg))
                    .await
                    .map_err(Error::from)?;
            }
        }

        let matches = self.subscriptions.matching_with(topic, |members, turn| {
            let chosen = self.share_member(members, turn, publisher, &message, None, true);
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
            share_group: None,
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
            share: Some(Arc::from("$share/g/jobs")),
        }
    }

    #[tokio::test]
    async fn test_reoffered_messages_go_to_another_member() {
        let broker = Broker::new(Arc::new(MemoryStore::new()));
        let leaving = share_member(1, QueueLimits::default(), false);
        let online = share_member(2, QueueLimits::default(), true);
        let offline = share_member(3, QueueLimits::default(), false);
        for member in [&leaving, &online, &offline] {
            broker.subscribe("$share/g/jobs", member.session_id, member.clone());
        }
        let message = create_test_message("jobs", "m", false);

        // A connected member takes it, never the one leaving
        assert!(broker.reoffer("$share/g/jobs", message.clone(), 1, false));
        assert_eq!(online.queue.len(), 1);
        // Without one connected, only when anyone may take it
        online.queue.set_connected(false);
        assert!(!broker.reoffer("$share/g/jobs", message.clone(), 2, false));
        assert!(broker.reoffer("$share/g/jobs", message.clone(), 1, true));
        assert_eq!(leaving.queue.len(), 0);
        assert_eq!(online.queue.len() + offline.queue.len(), 2);
        assert!(!broker.reoffer("$share/other/jobs", message, 1, true));
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

        let counters = crate::sys::counters(&stats, 0, 0, 0, 0, Default::default());
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

    /// Publishes `count` messages to a group of `members` connected members
    /// with `strategy`, `publisher` being the publishing client's key;
    /// how many each member got.
    async fn shares(
        strategy: ShareStrategy,
        members: &[Subscriber],
        count: usize,
        publisher: Option<Publisher>,
    ) -> Vec<usize> {
        let broker = Broker::new(Arc::new(MemoryStore::new())).with_share_strategy(strategy);
        for member in members {
            broker.subscribe("$share/g/jobs", member.session_id, member.clone());
        }
        for _ in 0..count {
            broker
                .publish_from("jobs", create_test_message("jobs", "x", false), publisher)
                .await
                .unwrap();
        }
        members.iter().map(|m| m.queue.len()).collect()
    }

    fn connected_members(count: u64) -> Vec<Subscriber> {
        (1..=count)
            .map(|id| share_member(id, QueueLimits::default(), true))
            .collect()
    }

    #[tokio::test]
    async fn test_sticky_shares_keep_to_one_member_while_it_takes_messages() {
        let small = QueueLimits {
            max_qos0: 3,
            ..Default::default()
        };
        let members: Vec<Subscriber> = (1..=3).map(|id| share_member(id, small, true)).collect();
        let broker =
            Broker::new(Arc::new(MemoryStore::new())).with_share_strategy(ShareStrategy::Sticky);
        for member in &members {
            broker.subscribe("$share/g/jobs", member.session_id, member.clone());
        }
        let publish = |n| {
            let broker = broker.clone();
            async move {
                for _ in 0..n {
                    broker
                        .publish("jobs", create_test_message("jobs", "x", false))
                        .await
                        .unwrap();
                }
            }
        };
        let counts = || {
            let mut counts: Vec<usize> = members.iter().map(|m| m.queue.len()).collect();
            counts.sort();
            counts
        };
        publish(3).await;
        assert_eq!(counts(), [0, 0, 3]);
        // Once its member is full, the next one is stuck to instead
        publish(2).await;
        assert_eq!(counts(), [0, 2, 3]);
        publish(1).await;
        assert_eq!(counts(), [0, 3, 3]);
    }

    #[tokio::test]
    async fn test_hashed_shares_send_one_publisher_to_one_member() {
        let members = connected_members(4);
        let publisher = |key| Some(Publisher { session: 99, key });
        let counts = shares(ShareStrategy::Hash, &members, 8, publisher(7)).await;
        assert_eq!(counts.iter().filter(|&&n| n == 8).count(), 1);
        // The same publisher lands on the same member again
        let again = shares(ShareStrategy::Hash, &members, 8, publisher(7)).await;
        assert_eq!(
            counts.iter().position(|&n| n == 8),
            again.iter().position(|&n| n == 16)
        );
        // Publishers spread over the members
        let mut reached = std::collections::HashSet::new();
        for key in 0..64 {
            let before: Vec<usize> = members.iter().map(|m| m.queue.len()).collect();
            let after = shares(ShareStrategy::Hash, &members, 1, publisher(key)).await;
            reached.insert(after.iter().zip(&before).position(|(a, b)| a != b));
        }
        assert_eq!(reached.len(), 4);
    }

    #[tokio::test]
    async fn test_random_shares_reach_every_member_and_prefer_connected_ones() {
        let mut members = connected_members(3);
        members.push(share_member(4, QueueLimits::default(), false));
        let counts = shares(ShareStrategy::Random, &members, 300, None).await;
        assert!(counts[..3].iter().all(|&n| n > 30), "{counts:?}");
        assert_eq!(counts[3], 0);
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

    /// A store counting how often it is asked for its count.
    #[derive(Default)]
    struct CountingStore {
        inner: MemoryStore,
        counted: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl RetainedMessageStore for CountingStore {
        async fn store_retained(
            &self,
            topic: &str,
            message: Option<Message>,
        ) -> mercurio_storage::Result<bool> {
            self.inner.store_retained(topic, message).await
        }

        async fn get_retained_page(
            &self,
            topic_filter: &str,
            after: Option<&str>,
            limit: usize,
        ) -> mercurio_storage::Result<Vec<Message>> {
            self.inner
                .get_retained_page(topic_filter, after, limit)
                .await
        }

        async fn clear_all_retained(&self) -> mercurio_storage::Result<()> {
            self.inner.clear_all_retained().await
        }

        async fn count_retained(&self) -> mercurio_storage::Result<usize> {
            self.counted
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.inner.count_retained().await
        }

        async fn remove_expired_retained(&self) -> mercurio_storage::Result<usize> {
            self.inner.remove_expired_retained().await
        }
    }

    /// The broker counts the stored retained messages itself once it has
    /// read their count, rather than ask the store on every retained
    /// publish under a limit.
    #[tokio::test]
    async fn test_the_retained_count_is_kept_in_memory() {
        let storage = Arc::new(CountingStore::default());
        storage
            .inner
            .store_retained("old", Some(create_test_message("old", "x", false)))
            .await
            .unwrap();
        let broker = Broker::new(Arc::clone(&storage));
        for topic in ["a", "b", "a", "c"] {
            assert!(!broker.retained_full(topic, 10).await.unwrap());
            broker
                .publish(topic, create_test_message(topic, "x", true))
                .await
                .unwrap();
        }
        assert_eq!(broker.retained_count().await, 4);
        broker
            .publish("b", create_test_message("b", "", true))
            .await
            .unwrap();
        broker
            .publish("never", create_test_message("never", "", true))
            .await
            .unwrap();
        assert_eq!(broker.retained_count().await, 3);
        assert!(broker.retained_full("d", 3).await.unwrap());
        assert!(!broker.retained_full("a", 3).await.unwrap());

        let mut expired = create_test_message("e", "x", true);
        expired.expires_at = Some(1);
        broker.publish("e", expired).await.unwrap();
        assert_eq!(broker.retained_count().await, 4);
        broker.remove_expired_retained().await;
        assert_eq!(broker.retained_count().await, 3);
        assert_eq!(
            storage.counted.load(std::sync::atomic::Ordering::Relaxed),
            1
        );
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
            share_group: None,
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
