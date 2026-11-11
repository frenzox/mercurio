//! Messages plugins publish. Each plugin publishes through a client of its
//! own inside the broker, `mercurio-plugin-<name>`, so its messages are
//! retained, counted and sent on as any client's, and the other plugins'
//! `on_message` see them. The host function only queues a message: a
//! plugin publishing from `on_message` never waits on the chain it runs
//! in.
//!
//! What stops a plugin from flooding the broker:
//! - `$` topics are refused, as the Rust plugin API has no way to publish
//!   to them either, and an `on_message` moving a message to one is
//!   dropped;
//! - a message is at most [`PUBLISH_SIZE`] bytes;
//! - a plugin publishes [`PUBLISH_BURST`] messages at once at most, then
//!   [`PUBLISH_RATE`] a second, all its copies and reloads together;
//! - at most [`PUBLISH_QUEUE`] messages wait to go out.
//!
//! A plugin's `on_message` never sees the messages the plugin itself
//! published, so republishing what it sees can't feed itself. Two plugins
//! republishing each other's messages can still loop; their rate limits
//! bound how fast.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};

use mercurio_client::{ConnectOptions, MessageProperties, MqttClient};
use mercurio_core::{qos::QoS, topic::validate_publish_topic};
use mercurio_server::{plugin::ClientInfo, server::LocalConnector};
use tokio::sync::mpsc;

use super::runtime::bindings::mercurio::plugin::types::{Message, Qos};

/// The most bytes of a message a plugin publishes: its topic, payload and
/// properties together.
pub(crate) const PUBLISH_SIZE: usize = 64 << 10;

/// How many messages a plugin may publish at once before
/// [`PUBLISH_RATE`] applies.
pub(crate) const PUBLISH_BURST: u32 = 100;

/// How many messages a second a plugin may publish over time.
pub(crate) const PUBLISH_RATE: u32 = 100;

/// How many messages may wait to go out to the broker.
const PUBLISH_QUEUE: usize = 256;

/// The listener in-process clients connect through, as plugins see it.
const LOCAL: &str = "local";

/// Publishes one plugin's messages; shared by its copies and kept across
/// reloads, with the rate limit.
pub(crate) struct Publisher {
    name: String,
    client_id: String,
    /// None when the plugin runs without a broker to publish to
    queue: Option<mpsc::Sender<Message>>,
    bucket: Mutex<Bucket>,
    /// Whether the last message was refused, so a run of refusals is
    /// logged once
    refusing: AtomicBool,
}

/// A token bucket: [`PUBLISH_BURST`] tokens at most, refilled at
/// [`PUBLISH_RATE`] a second.
struct Bucket {
    tokens: f64,
    filled_at: Instant,
}

impl Bucket {
    fn new() -> Bucket {
        Bucket {
            tokens: PUBLISH_BURST as f64,
            filled_at: Instant::now(),
        }
    }

    fn take(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.filled_at);
        self.tokens =
            (self.tokens + elapsed.as_secs_f64() * PUBLISH_RATE as f64).min(PUBLISH_BURST as f64);
        self.filled_at = now;
        if self.tokens < 1.0 {
            return false;
        }
        self.tokens -= 1.0;
        true
    }
}

impl Publisher {
    /// A publisher for the plugin `name`, sending through `broker` once
    /// the first message comes. Needs a Tokio runtime when `broker` is set.
    pub(crate) fn new(name: &str, broker: Option<LocalConnector>) -> Publisher {
        let client_id = format!("mercurio-plugin-{name}");
        let queue = broker.map(|broker| {
            let (sender, receiver) = mpsc::channel(PUBLISH_QUEUE);
            tokio::spawn(send(name.to_string(), client_id.clone(), broker, receiver));
            sender
        });
        Publisher {
            name: name.to_string(),
            client_id,
            queue,
            bucket: Mutex::new(Bucket::new()),
            refusing: AtomicBool::new(false),
        }
    }

    /// Whether `client` is this publisher's own client, whose messages the
    /// plugin's `on_message` doesn't see. Only an in-process client counts,
    /// so a network client taking the same id doesn't escape the hook.
    pub(crate) fn is_own(&self, client: &ClientInfo) -> bool {
        client.client_id == self.client_id
            && client.peer.is_none()
            && client
                .connection
                .as_ref()
                .is_some_and(|connection| &*connection.listener == LOCAL)
    }

    /// Queues `message`, or says why it can't be published.
    pub(crate) fn publish(&self, message: Message) -> Result<(), String> {
        let result = self.queue(message);
        match &result {
            Ok(()) => self.refusing.store(false, Ordering::Relaxed),
            Err(error) => {
                if !self.refusing.swap(true, Ordering::Relaxed) {
                    tracing::warn!(plugin = %self.name, "Refused to publish: {error}");
                }
            }
        }
        result
    }

    fn queue(&self, message: Message) -> Result<(), String> {
        check(&message)?;
        if !self.bucket.lock().unwrap().take(Instant::now()) {
            return Err(format!(
                "publishing faster than {PUBLISH_RATE} messages a second"
            ));
        }
        let queue = self
            .queue
            .as_ref()
            .ok_or("no broker to publish to".to_string())?;
        queue.try_send(message).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => {
                format!("{PUBLISH_QUEUE} messages already wait to be published")
            }
            mpsc::error::TrySendError::Closed(_) => "the broker stopped".to_string(),
        })
    }
}

/// Whether a plugin may publish `message`: a valid topic name not starting
/// with `$`, and [`PUBLISH_SIZE`] bytes at most.
fn check(message: &Message) -> Result<(), String> {
    let topic = &message.topic;
    if topic.starts_with('$') {
        return Err(format!("`{topic}`: plugins may not publish to `$` topics"));
    }
    validate_publish_topic(topic).map_err(|error| format!("`{topic}`: {error}"))?;
    let size = topic.len()
        + message.payload.len()
        + message.content_type.as_ref().map_or(0, String::len)
        + message.response_topic.as_ref().map_or(0, String::len)
        + message.correlation_data.as_ref().map_or(0, Vec::len)
        + message
            .user_properties
            .iter()
            .map(|(key, value)| key.len() + value.len())
            .sum::<usize>();
    if size > PUBLISH_SIZE {
        return Err(format!(
            "the message has {size} bytes, more than the {PUBLISH_SIZE} allowed"
        ));
    }
    Ok(())
}

/// Sends the queued messages through the plugin's own client, connecting
/// on the first one; until every copy of the plugin is gone.
async fn send(
    name: String,
    client_id: String,
    broker: LocalConnector,
    mut queue: mpsc::Receiver<Message>,
) {
    let options = ConnectOptions::new("localhost", 0)
        .client_id(client_id)
        .reconnect(Duration::from_secs(1))
        .connector(move || std::future::ready(broker.connect()));
    let mut client: Option<MqttClient> = None;
    loop {
        // The client's events are read, and dropped, so they don't pile up
        let message = match &client {
            Some(connected) => tokio::select! {
                message = queue.recv() => message,
                event = connected.recv() => {
                    if event.is_none() {
                        client = None;
                    }
                    continue;
                }
            },
            None => queue.recv().await,
        };
        let Some(message) = message else {
            return;
        };
        if client.is_none() {
            match MqttClient::connect(options.clone()).await {
                Ok(connected) => client = Some(connected),
                Err(error) => {
                    tracing::warn!(
                        plugin = %name,
                        "Couldn't connect to publish, dropping a message to `{}`: {error}",
                        message.topic
                    );
                    continue;
                }
            }
        }
        let Some(connected) = &client else {
            continue;
        };
        let mut properties = MessageProperties::default();
        properties.payload_is_utf8 = message.payload_is_utf8;
        properties.message_expiry = message.message_expiry;
        properties.content_type = message.content_type;
        properties.response_topic = message.response_topic;
        properties.correlation_data = message.correlation_data.map(Into::into);
        properties.user_properties = message.user_properties;
        let qos = match message.qos {
            Qos::AtMostOnce => QoS::AtMostOnce,
            Qos::AtLeastOnce => QoS::AtLeastOnce,
            Qos::ExactlyOnce => QoS::ExactlyOnce,
        };
        // Queued in the client, which sees it acknowledged; nobody waits
        // for that
        let sent = connected
            .start_publish(
                &message.topic,
                message.payload,
                qos,
                message.retain,
                properties,
            )
            .await;
        if let Err(error) = sent {
            tracing::warn!(
                plugin = %name,
                "Couldn't publish to `{}`: {error}",
                message.topic
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(topic: &str, payload: usize) -> Message {
        Message {
            topic: topic.into(),
            payload: vec![0; payload],
            qos: Qos::AtMostOnce,
            retain: false,
            payload_is_utf8: false,
            message_expiry: None,
            content_type: None,
            response_topic: None,
            correlation_data: None,
            user_properties: Vec::new(),
        }
    }

    #[test]
    fn test_plugins_may_not_publish_to_reserved_or_invalid_topics() {
        for topic in ["$SYS/broker/uptime", "$CONTROL/broker/v1", "$x"] {
            let error = check(&message(topic, 1)).unwrap_err();
            assert!(error.contains("`$` topics"), "{error}");
        }
        for topic in ["", "a/+", "a/#"] {
            assert!(check(&message(topic, 1)).is_err(), "{topic}");
        }
        assert!(check(&message("a/b", 1)).is_ok());
    }

    #[test]
    fn test_the_size_limit_counts_topic_payload_and_properties() {
        assert!(check(&message("t", PUBLISH_SIZE - 1)).is_ok());
        assert!(check(&message("t", PUBLISH_SIZE)).is_err());
        let mut big = message("t", 1);
        big.user_properties = vec![("k".into(), "v".repeat(PUBLISH_SIZE))];
        assert!(check(&big).unwrap_err().contains("allowed"));
    }

    #[test]
    fn test_the_bucket_refills_at_the_rate() {
        let mut bucket = Bucket::new();
        let start = bucket.filled_at;
        for _ in 0..PUBLISH_BURST {
            assert!(bucket.take(start));
        }
        assert!(!bucket.take(start));
        let tick = Duration::from_secs(1) / PUBLISH_RATE;
        assert!(bucket.take(start + tick));
        assert!(!bucket.take(start + tick));
        // Never more than the burst, however long it waited
        let later = start + Duration::from_secs(3600);
        let taken = (0..PUBLISH_BURST * 2)
            .filter(|_| bucket.take(later))
            .count();
        assert_eq!(taken, PUBLISH_BURST as usize);
    }

    #[test]
    fn test_only_the_in_process_client_is_its_own() {
        use mercurio_core::protocol::ProtocolVersion;
        use mercurio_server::plugin::ConnectionInfo;

        let publisher = Publisher::new("p", None);
        let local = |listener: &str| {
            let mut client = ClientInfo::new("mercurio-plugin-p", None);
            client.connection = Some(std::sync::Arc::new(ConnectionInfo::new(
                listener,
                ProtocolVersion::V5,
            )));
            client
        };
        assert!(publisher.is_own(&local("local")));
        assert!(!publisher.is_own(&local("/run/mercurio.sock")));
        let mut network = local("0.0.0.0:1883");
        network.peer = Some("10.0.0.1:5555".parse().unwrap());
        assert!(!publisher.is_own(&network));
        assert!(!publisher.is_own(&ClientInfo::new("mercurio-plugin-p", None)));
        let mut other = local("local");
        other.client_id = "mercurio-plugin-q".into();
        assert!(!publisher.is_own(&other));
    }

    #[test]
    fn test_without_a_broker_publishing_fails() {
        let publisher = Publisher::new("p", None);
        let error = publisher.publish(message("a", 1)).unwrap_err();
        assert!(error.contains("no broker"), "{error}");
    }
}
