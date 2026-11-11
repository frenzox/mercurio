//! Broker statistics published under `$SYS/broker/`, as retained messages
//! refreshed periodically, following mosquitto's topic names.

use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, RwLock,
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use bytes::Bytes;
use mercurio_core::{message::Message, qos::QoS, topic::topic_matches_filter};
use mercurio_storage::{MqttStore, Result as StorageResult, RetainedMessageStore};
use tokio::time;
use tracing::warn;

use crate::{
    broker::Broker, session_manager::SessionManager, shutdown::Tasks, writer::StorageHealth,
};

/// Why the broker lost a message it had accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DropCause {
    /// A QoS 0 message found the session's queue at its QoS 0 limit
    QueueFullQos0,
    /// A QoS 1 or 2 message found the session's queue full
    QueueFull,
    /// Its Message Expiry Interval passed while it waited in a queue
    Expired,
    /// Larger than the client's Maximum Packet Size
    TooLarge,
    /// The subscriber may not read its topic
    NotAuthorized,
    /// No member of a shared subscription's group could take it
    NoShareMember,
}

/// The `$SYS/broker/` names of the messages dropped by cause, in
/// `DropCause` order.
const DROPPED_BY_CAUSE: [&str; 6] = [
    "publish/messages/dropped/queue_full_qos0",
    "publish/messages/dropped/queue_full",
    "publish/messages/dropped/expired",
    "publish/messages/dropped/too_large",
    "publish/messages/dropped/not_authorized",
    "publish/messages/dropped/no_share_member",
];

/// Counters the connection handlers update.
pub(crate) struct Stats {
    pub started: Instant,
    pub connected: AtomicU64,
    /// Most clients connected at once
    pub max_connected: AtomicU64,
    pub connections: AtomicU64,
    pub messages_received: AtomicU64,
    pub messages_sent: AtomicU64,
    pub bytes_received: AtomicU64,
    pub bytes_sent: AtomicU64,
    /// By `DropCause`, in its order
    dropped: [AtomicU64; DROPPED_BY_CAUSE.len()],
}

impl Default for Stats {
    fn default() -> Self {
        Stats {
            started: Instant::now(),
            connected: Default::default(),
            max_connected: Default::default(),
            connections: Default::default(),
            messages_received: Default::default(),
            messages_sent: Default::default(),
            bytes_received: Default::default(),
            bytes_sent: Default::default(),
            dropped: Default::default(),
        }
    }
}

impl Stats {
    pub(crate) fn add(counter: &AtomicU64) {
        Self::add_n(counter, 1);
    }

    pub(crate) fn add_n(counter: &AtomicU64, n: u64) {
        counter.fetch_add(n, Ordering::Relaxed);
    }

    /// Counts `n` messages lost for `cause`, and against `client` when one
    /// client lost them.
    pub(crate) fn dropped(&self, cause: DropCause, n: u64, client: Option<&ClientCounters>) {
        Self::add_n(&self.dropped[cause as usize], n);
        if let Some(client) = client {
            Self::add_n(&client.dropped, n);
        }
    }
}

/// Traffic of one connection since it connected, for the admin API.
#[derive(Default)]
pub(crate) struct ClientCounters {
    pub messages_in: AtomicU64,
    pub messages_out: AtomicU64,
    pub bytes_in: AtomicU64,
    pub bytes_out: AtomicU64,
    /// Messages for the client the broker lost, those lost while it was
    /// away after this connection included
    pub dropped: AtomicU64,
}

/// Retained messages, those on `$SYS` topics kept in memory in front of
/// `store`. The broker republishes its statistics every interval, so
/// storing each update would be wasted writes, and they would count toward
/// `max_retained_messages`. Clients may not publish to `$SYS`, so only the
/// broker and its bridges put anything there.
///
/// Also keeps the count of stored messages, read from the store once and
/// kept up to date as messages are stored and removed, as the store would
/// count them all on each retained publish under `max_retained_messages`.
pub(crate) struct SysOverlay<S> {
    store: Arc<S>,
    sys: RwLock<BTreeMap<String, Message>>,
    stored: tokio::sync::OnceCell<AtomicUsize>,
}

impl<S: RetainedMessageStore> SysOverlay<S> {
    pub(crate) fn new(store: Arc<S>) -> Self {
        SysOverlay {
            store,
            sys: Default::default(),
            stored: Default::default(),
        }
    }

    /// Removes the `$SYS` messages an earlier version left in the store.
    pub(crate) async fn forget_stored(&self) -> StorageResult<()> {
        for message in self.store.get_retained("$SYS/#").await? {
            let existed = self.store.store_retained(&message.topic, None).await?;
            self.count(0, usize::from(existed));
        }
        Ok(())
    }

    /// The count of stored messages, read from the store the first time.
    async fn stored(&self) -> StorageResult<&AtomicUsize> {
        self.stored
            .get_or_try_init(|| async { Ok(AtomicUsize::new(self.store.count_retained().await?)) })
            .await
    }

    /// Updates the count, once it has been read, by `added` and `removed`.
    fn count(&self, added: usize, removed: usize) {
        if let Some(stored) = self.stored.get() {
            let mut count = stored.load(Ordering::Relaxed);
            while let Err(actual) = stored.compare_exchange_weak(
                count,
                (count + added).saturating_sub(removed),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                count = actual;
            }
        }
    }
}

pub(crate) fn is_sys(topic: &str) -> bool {
    topic == "$SYS" || topic.starts_with("$SYS/")
}

#[async_trait]
impl<S: RetainedMessageStore> RetainedMessageStore for SysOverlay<S> {
    async fn store_retained(&self, topic: &str, message: Option<Message>) -> StorageResult<bool> {
        if !is_sys(topic) {
            let added = message.is_some();
            let existed = self.store.store_retained(topic, message).await?;
            self.count(
                usize::from(added && !existed),
                usize::from(!added && existed),
            );
            return Ok(existed);
        }
        let mut sys = self.sys.write().unwrap();
        Ok(match message {
            Some(message) => sys.insert(topic.to_string(), message),
            None => sys.remove(topic),
        }
        .is_some())
    }

    /// The store's page merged with the `$SYS` messages, both in topic
    /// order.
    async fn get_retained_page(
        &self,
        topic_filter: &str,
        after: Option<&str>,
        limit: usize,
    ) -> StorageResult<Vec<Message>> {
        let mut found = self
            .store
            .get_retained_page(topic_filter, after, limit)
            .await?;
        // Wildcards at the first level never match `$` topics
        if topic_filter.starts_with('$') {
            let sys = self.sys.read().unwrap();
            found.extend(
                sys.iter()
                    .filter(|(topic, message)| {
                        after.is_none_or(|after| topic.as_str() > after)
                            && !message.is_expired()
                            && topic_matches_filter(topic, topic_filter)
                    })
                    .take(limit)
                    .map(|(_, message)| Message {
                        retain: true,
                        ..message.clone()
                    }),
            );
            found.sort_by(|a, b| a.topic.cmp(&b.topic));
            found.truncate(limit);
        }
        Ok(found)
    }

    async fn clear_all_retained(&self) -> StorageResult<()> {
        self.sys.write().unwrap().clear();
        self.store.clear_all_retained().await?;
        if let Some(stored) = self.stored.get() {
            stored.store(0, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Only those stored, which `max_retained_messages` limits.
    async fn count_retained(&self) -> StorageResult<usize> {
        Ok(self.stored().await?.load(Ordering::Relaxed))
    }

    async fn remove_expired_retained(&self) -> StorageResult<usize> {
        let removed = self.store.remove_expired_retained().await?;
        self.count(0, removed);
        Ok(removed)
    }
}

/// Rates per minute averaged over 1, 5 and 15 minutes, as mosquitto's
/// `$SYS/broker/load/` topics are: exponentially weighted, updated every
/// `interval`.
struct Load {
    name: &'static str,
    last: u64,
    averages: [f64; 3],
}

impl Load {
    const PERIODS: [(&'static str, f64); 3] = [("1min", 60.0), ("5min", 300.0), ("15min", 900.0)];

    fn new(name: &'static str) -> Self {
        Self {
            name,
            last: 0,
            averages: [0.0; 3],
        }
    }

    /// Takes in the counter's current value, `interval` after the last one.
    fn update(&mut self, value: u64, interval: Duration) {
        let seconds = interval.as_secs_f64();
        let per_minute = (value - self.last) as f64 * 60.0 / seconds;
        self.last = value;
        for (average, (_, period)) in self.averages.iter_mut().zip(Self::PERIODS) {
            let weight = (-seconds / period).exp();
            *average = *average * weight + per_minute * (1.0 - weight);
        }
    }

    fn values(&self) -> impl Iterator<Item = (String, String)> + '_ {
        self.averages
            .iter()
            .zip(Self::PERIODS)
            .map(|(average, (period, _))| {
                (
                    format!("load/{}/{}", self.name, period),
                    format!("{average:.2}"),
                )
            })
    }
}

/// Counts a connection as connected for as long as it is held.
pub(crate) struct ConnectedGuard(Arc<Stats>);

impl ConnectedGuard {
    pub(crate) fn new(stats: Arc<Stats>) -> Self {
        let connected = stats.connected.fetch_add(1, Ordering::Relaxed) + 1;
        stats.max_connected.fetch_max(connected, Ordering::Relaxed);
        Stats::add(&stats.connections);
        ConnectedGuard(stats)
    }
}

impl Drop for ConnectedGuard {
    fn drop(&mut self) {
        self.0.connected.fetch_sub(1, Ordering::Relaxed);
    }
}

/// The counters published under `$SYS/broker/`, by their name there.
pub(crate) fn counters(
    stats: &Stats,
    sessions: usize,
    expired: u64,
    retained: usize,
    subscriptions: usize,
    storage: StorageHealth,
) -> Vec<(&'static str, u64)> {
    let get = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
    let connected = get(&stats.connected);
    let sessions = sessions as u64;
    let dropped: Vec<u64> = stats.dropped.iter().map(get).collect();
    let mut counters = vec![
        ("clients/connected", connected),
        ("clients/total", sessions),
        ("clients/disconnected", sessions.saturating_sub(connected)),
        ("clients/maximum", get(&stats.max_connected)),
        ("clients/expired", expired),
        ("messages/received", get(&stats.messages_received)),
        ("messages/sent", get(&stats.messages_sent)),
        ("bytes/received", get(&stats.bytes_received)),
        ("bytes/sent", get(&stats.bytes_sent)),
        ("retained messages/count", retained as u64),
        ("subscriptions/count", subscriptions as u64),
        ("publish/messages/dropped", dropped.iter().sum()),
        ("storage/batches/retried", storage.retried),
        ("storage/batches/dropped", storage.dropped),
        ("storage/clients/dirty", storage.dirty as u64),
    ];
    counters.extend(DROPPED_BY_CAUSE.into_iter().zip(dropped));
    counters
}

/// Stored sessions set aside at restore as they couldn't be read.
pub(crate) const SET_ASIDE: &str = "storage/sessions/set aside";

/// The counters published under `$SYS/broker/`, read now.
pub(crate) async fn current<St: MqttStore + 'static>(
    broker: &Broker<St>,
    session_manager: &SessionManager<St>,
    stats: &Stats,
) -> Vec<(&'static str, u64)> {
    let mut counters = counters(
        stats,
        session_manager.session_count().await,
        session_manager.expired_count(),
        broker.retained_count().await,
        broker.topics().subscription_count(),
        session_manager.storage_health(),
    );
    counters.push((SET_ASIDE, session_manager.set_aside_count().await as u64));
    counters
}

/// The Prometheus metric a `$SYS/broker/` counter is exposed as: its name,
/// help and type. None for the total of dropped messages, which the
/// metric's `cause` label splits.
fn metric(name: &str) -> Option<(&'static str, &'static str, &'static str)> {
    let (counter, gauge) = ("counter", "gauge");
    Some(match name {
        "clients/connected" => ("mercurio_clients_connected", "Clients connected now", gauge),
        "clients/total" => ("mercurio_sessions", "Sessions, connected or not", gauge),
        "clients/disconnected" => (
            "mercurio_sessions_disconnected",
            "Sessions kept for clients that are not connected",
            gauge,
        ),
        "clients/maximum" => (
            "mercurio_clients_connected_max",
            "Most clients connected at once",
            gauge,
        ),
        "clients/expired" => (
            "mercurio_sessions_expired_total",
            "Sessions ended by their expiry",
            counter,
        ),
        "messages/received" => (
            "mercurio_messages_received_total",
            "PUBLISH packets received",
            counter,
        ),
        "messages/sent" => (
            "mercurio_messages_sent_total",
            "PUBLISH packets sent",
            counter,
        ),
        "bytes/received" => ("mercurio_bytes_received_total", "Bytes received", counter),
        "bytes/sent" => ("mercurio_bytes_sent_total", "Bytes sent", counter),
        "retained messages/count" => (
            "mercurio_retained_messages",
            "Retained messages stored, $SYS ones not counted",
            gauge,
        ),
        "subscriptions/count" => ("mercurio_subscriptions", "Subscriptions", gauge),
        SET_ASIDE => (
            "mercurio_sessions_set_aside",
            "Stored sessions set aside as they could not be read, their clients refused",
            gauge,
        ),
        "storage/batches/retried" => (
            "mercurio_storage_batches_retried_total",
            "Batches of session changes written again after a storage error",
            counter,
        ),
        "storage/batches/dropped" => (
            "mercurio_storage_batches_dropped_total",
            "Batches of session changes given up on after storage errors",
            counter,
        ),
        "storage/clients/dirty" => (
            "mercurio_storage_dirty_clients",
            "Clients whose stored state differs from the broker's until it is stored whole again",
            gauge,
        ),
        "connections" => (
            "mercurio_connections_total",
            "Connections accepted",
            counter,
        ),
        "uptime" => (
            "mercurio_uptime_seconds",
            "Seconds since the broker started",
            gauge,
        ),
        _ if name.starts_with("publish/messages/dropped/") => (
            "mercurio_messages_dropped_total",
            "Messages lost before reaching a subscriber, by cause",
            counter,
        ),
        _ => return None,
    })
}

/// `counters`, as returned by `current`, in the Prometheus text exposition
/// format, with the connections accepted and the uptime.
pub(crate) fn prometheus(counters: &[(&'static str, u64)], stats: &Stats) -> String {
    let extra = [
        ("connections", stats.connections.load(Ordering::Relaxed)),
        ("uptime", stats.started.elapsed().as_secs()),
    ];
    let mut text = String::new();
    let mut described = "";
    for (name, value) in counters.iter().copied().chain(extra) {
        let Some((metric, help, kind)) = metric(name) else {
            continue;
        };
        if metric != described {
            text.push_str(&format!("# HELP {metric} {help}\n# TYPE {metric} {kind}\n"));
            described = metric;
        }
        match name.strip_prefix("publish/messages/dropped/") {
            Some(cause) => text.push_str(&format!("{metric}{{cause=\"{cause}\"}} {value}\n")),
            None => text.push_str(&format!("{metric} {value}\n")),
        }
    }
    text
}

/// Publishes the statistics every `interval`, in `tasks`, until the server
/// stops.
pub(crate) fn spawn_publisher<St: MqttStore + 'static>(
    tasks: &Tasks,
    broker: Broker<St>,
    session_manager: SessionManager<St>,
    stats: Arc<Stats>,
    started: Instant,
    interval: Duration,
) {
    tasks.spawn(async move {
        publish(
            &broker,
            "version",
            format!("mercurio {}", env!("CARGO_PKG_VERSION")),
        )
        .await;

        let mut loads = [
            Load::new("messages/received"),
            Load::new("messages/sent"),
            Load::new("bytes/received"),
            Load::new("bytes/sent"),
            Load::new("connections"),
        ];
        let mut ticks = time::interval(interval);
        loop {
            ticks.tick().await;
            let current = current(&broker, &session_manager, &stats).await;
            let mut values = vec![(
                "uptime".to_string(),
                format!("{} seconds", started.elapsed().as_secs()),
            )];
            values.extend(
                current
                    .iter()
                    .map(|(name, value)| (name.to_string(), value.to_string())),
            );
            let get = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
            let rates = [
                get(&stats.messages_received),
                get(&stats.messages_sent),
                get(&stats.bytes_received),
                get(&stats.bytes_sent),
                get(&stats.connections),
            ];
            for (load, value) in loads.iter_mut().zip(rates) {
                load.update(value, interval);
                values.extend(load.values());
            }
            for (name, value) in values {
                publish(&broker, &name, value).await;
            }
        }
    });
}

async fn publish<St: MqttStore>(broker: &Broker<St>, name: &str, value: String) {
    let topic = format!("$SYS/broker/{name}");
    let message = Message {
        packet_id: None,
        topic: Arc::from(topic.as_str()),
        dup: false,
        qos: QoS::AtMostOnce,
        retain: true,
        payload: Some(Bytes::from(value)),
        properties: Default::default(),
        expires_at: None,
        subscription_identifiers: Vec::new(),
        origin: None,
        share_group: None,
    };
    if let Err(e) = broker.publish(&topic, message).await {
        warn!("Failed to publish `{}`: {}", topic, e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn retained(topic: &str) -> Message {
        Message {
            packet_id: None,
            topic: Arc::from(topic),
            dup: false,
            qos: QoS::AtMostOnce,
            retain: false,
            payload: Some(Bytes::from_static(b"1")),
            properties: Default::default(),
            expires_at: None,
            subscription_identifiers: Vec::new(),
            origin: None,
            share_group: None,
        }
    }

    /// Pages of `$SYS` lookups hold the stored messages and those in memory
    /// in one topic order.
    #[tokio::test]
    async fn test_pages_merge_the_messages_in_memory() {
        let store = Arc::new(mercurio_storage::memory::MemoryStore::new());
        for topic in ["$SYS/b", "$SYS/d"] {
            store
                .store_retained(topic, Some(retained(topic)))
                .await
                .unwrap();
        }
        let overlay = SysOverlay::new(store);
        for topic in ["$SYS/a", "$SYS/c", "$SYS/e"] {
            overlay
                .store_retained(topic, Some(retained(topic)))
                .await
                .unwrap();
        }
        let mut topics = Vec::new();
        loop {
            let after = topics.last().cloned();
            let page = overlay
                .get_retained_page("$SYS/#", after.as_deref(), 2)
                .await
                .unwrap();
            assert!(page.len() <= 2);
            let last = page.len() < 2;
            topics.extend(page.iter().map(|m| m.topic.to_string()));
            if last {
                break;
            }
        }
        assert_eq!(topics, ["$SYS/a", "$SYS/b", "$SYS/c", "$SYS/d", "$SYS/e"]);
        // Only those stored count
        assert_eq!(overlay.count_retained().await.unwrap(), 2);
    }

    #[test]
    fn test_load_averages_approach_the_rate() {
        let mut load = Load::new("messages/received");
        let interval = Duration::from_secs(10);
        // 100 messages every 10 seconds: 600 a minute
        for tick in 1..=360 {
            load.update(tick * 100, interval);
        }
        let values: Vec<(String, String)> = load.values().collect();
        assert_eq!(
            values[0],
            ("load/messages/received/1min".into(), "600.00".into())
        );
        // After an hour, the 15 minute average covered 1 - e^-4 of the way
        let fifteen: f64 = values[2].1.parse().unwrap();
        assert!((588.0..590.0).contains(&fifteen), "{fifteen}");
    }
}
