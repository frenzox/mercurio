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
    pub messages_received: AtomicU64,
    pub messages_sent: AtomicU64,
}

impl Stats {
    pub(crate) fn add(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// Counts a connection as connected for as long as it is held.
pub(crate) struct ConnectedGuard(Arc<Stats>);

impl ConnectedGuard {
    pub(crate) fn new(stats: Arc<Stats>) -> Self {
        Stats::add(&stats.connected);
        ConnectedGuard(stats)
    }
}

impl Drop for ConnectedGuard {
    fn drop(&mut self) {
        self.0.connected.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Publishes the statistics every `interval` until the process ends.
pub(crate) fn spawn_publisher<St: MqttStore + 'static>(
    broker: Broker<St>,
    session_manager: SessionManager<St>,
    stats: Arc<Stats>,
    interval: Duration,
) {
    let started = Instant::now();
    tokio::spawn(async move {
        publish(
            &broker,
            "version",
            format!("mercurio {}", env!("CARGO_PKG_VERSION")),
        )
        .await;

        let mut ticks = time::interval(interval);
        loop {
            ticks.tick().await;
            let values = [
                ("uptime", format!("{} seconds", started.elapsed().as_secs())),
                (
                    "clients/connected",
                    stats.connected.load(Ordering::Relaxed).to_string(),
                ),
                (
                    "clients/total",
                    session_manager.session_count().await.to_string(),
                ),
                (
                    "messages/received",
                    stats.messages_received.load(Ordering::Relaxed).to_string(),
                ),
                (
                    "messages/sent",
                    stats.messages_sent.load(Ordering::Relaxed).to_string(),
                ),
                (
                    "subscriptions/count",
                    broker.topics().subscription_count().to_string(),
                ),
            ];
            for (name, value) in values {
                publish(&broker, name, value).await;
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
    };
    if let Err(e) = broker.publish(&topic, message).await {
        warn!("Failed to publish `{}`: {}", topic, e);
    }
}
