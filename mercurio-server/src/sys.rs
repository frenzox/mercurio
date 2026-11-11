//! Broker statistics published under `$SYS/broker/`, as retained messages
//! refreshed periodically, following mosquitto's topic names.

use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use bytes::Bytes;
use mercurio_core::{message::Message, qos::QoS};
use mercurio_storage::MqttStore;
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
#[derive(Default)]
pub(crate) struct Stats {
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
    ];
    counters.extend(DROPPED_BY_CAUSE.into_iter().zip(dropped));
    counters.push(("store/batches/retried", get(&BATCHES_RETRIED)));
    counters.push(("store/batches/dropped", get(&BATCHES_DROPPED)));
    counters
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
            let current = counters(
                &stats,
                session_manager.session_count().await,
                session_manager.expired_count(),
                broker.retained_count().await,
                broker.topics().subscription_count(),
            );
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
