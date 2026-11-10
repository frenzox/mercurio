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
}

impl Stats {
    pub(crate) fn add(counter: &AtomicU64) {
        Self::add_n(counter, 1);
    }

    pub(crate) fn add_n(counter: &AtomicU64, n: u64) {
        counter.fetch_add(n, Ordering::Relaxed);
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
) -> Vec<(&'static str, u64)> {
    let get = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
    let connected = get(&stats.connected);
    let sessions = sessions as u64;
    vec![
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
    ]
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
