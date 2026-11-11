//! Broker statistics published under `$SYS/broker/`, as retained messages
//! refreshed periodically, following mosquitto's topic names.

use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicU64, Ordering},
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

use crate::{broker::Broker, session_manager::SessionManager};

/// Storage batches the writers attempted again after a failure, for every
/// broker of the process: the writers' retry loop has no broker to count in.
pub(crate) static BATCHES_RETRIED: AtomicU64 = AtomicU64::new(0);

/// Storage batches the writers gave up on, their changes lost.
pub(crate) static BATCHES_DROPPED: AtomicU64 = AtomicU64::new(0);

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
pub(crate) struct SysOverlay<S> {
    store: Arc<S>,
    sys: RwLock<BTreeMap<String, Message>>,
}

impl<S: RetainedMessageStore> SysOverlay<S> {
    pub(crate) fn new(store: Arc<S>) -> Self {
        SysOverlay {
            store,
            sys: Default::default(),
        }
    }

    /// Removes the `$SYS` messages an earlier version left in the store.
    pub(crate) async fn forget_stored(&self) -> StorageResult<()> {
        for message in self.store.get_retained("$SYS/#").await? {
            self.store.store_retained(&message.topic, None).await?;
        }
        Ok(())
    }
}

pub(crate) fn is_sys(topic: &str) -> bool {
    topic == "$SYS" || topic.starts_with("$SYS/")
}

#[async_trait]
impl<S: RetainedMessageStore> RetainedMessageStore for SysOverlay<S> {
    async fn store_retained(&self, topic: &str, message: Option<Message>) -> StorageResult<()> {
        if !is_sys(topic) {
            return self.store.store_retained(topic, message).await;
        }
        let mut sys = self.sys.write().unwrap();
        match message {
            Some(message) => sys.insert(topic.to_string(), message),
            None => sys.remove(topic),
        };
        Ok(())
    }

    async fn get_retained(&self, topic_filter: &str) -> StorageResult<Vec<Message>> {
        let mut found = self.store.get_retained(topic_filter).await?;
        // Wildcards at the first level never match `$` topics
        if topic_filter.starts_with('$') {
            let sys = self.sys.read().unwrap();
            found.extend(
                sys.iter()
                    .filter(|(topic, _)| topic_matches_filter(topic, topic_filter))
                    .map(|(_, message)| Message {
                        retain: true,
                        ..message.clone()
                    }),
            );
        }
        Ok(found)
    }

    async fn clear_all_retained(&self) -> StorageResult<()> {
        self.sys.write().unwrap().clear();
        self.store.clear_all_retained().await
    }

    /// Only those stored, which `max_retained_messages` limits.
    async fn count_retained(&self) -> StorageResult<usize> {
        self.store.count_retained().await
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
    storage_failures: (u64, usize),
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
        ("storage/failed batches", storage_failures.0),
        ("storage/dirty clients", storage_failures.1 as u64),
    ];
    counters.extend(DROPPED_BY_CAUSE.into_iter().zip(dropped));
    counters.push(("store/batches/retried", get(&BATCHES_RETRIED)));
    counters.push(("store/batches/dropped", get(&BATCHES_DROPPED)));
    counters
}

/// Stored sessions set aside at restore as they couldn't be read.
pub(crate) const SET_ASIDE: &str = "storage/set aside sessions";

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
        session_manager.storage_failures(),
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
        "store/batches/retried" => (
            "mercurio_storage_batches_retried_total",
            "Batches of session changes written again after a storage error",
            counter,
        ),
        "store/batches/dropped" => (
            "mercurio_storage_batches_dropped_total",
            "Batches of session changes given up on after storage errors",
            counter,
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

/// Publishes the statistics every `interval` until the process ends.
pub(crate) fn spawn_publisher<St: MqttStore + 'static>(
    broker: Broker<St>,
    session_manager: SessionManager<St>,
    stats: Arc<Stats>,
    started: Instant,
    interval: Duration,
) {
    tokio::spawn(async move {
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
    };
    if let Err(e) = broker.publish(&topic, message).await {
        warn!("Failed to publish `{}`: {}", topic, e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
