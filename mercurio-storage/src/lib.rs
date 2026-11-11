#![doc = include_str!("../README.md")]
#[cfg(any(feature = "sqlite", feature = "postgres"))]
mod encoding;
pub mod memory;

#[cfg(feature = "postgres")]
pub mod postgres;
#[cfg(feature = "sqlite")]
pub mod sqlite;
#[cfg(any(test, feature = "test-util"))]
pub mod testing;

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use mercurio_core::{
    message::{Message, MessageProperties},
    qos::QoS,
};
use thiserror::Error;

/// Error type for storage operations.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum StorageError {
    /// The store couldn't carry out an operation, such as when its lock
    /// is poisoned or a connection to the database can't be had.
    #[error("Storage operation failed: {0}")]
    OperationFailed(String),

    /// Stored data couldn't be decoded, such as a message's properties.
    #[error("Serialization error: {0}")]
    SerializationError(String),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    /// The database refused or failed an operation. Boxed, so the database
    /// libraries' own types stay out of this crate's API.
    #[error("Database: {}", error_chain(.0.as_ref()))]
    Database(Box<dyn std::error::Error + Send + Sync>),

    /// A failure that may pass if the operation is tried again, such as a
    /// lost connection, a timed out wait for a pooled connection, a busy
    /// database or a transaction that lost a serialization race.
    #[error("Database, temporarily: {}", error_chain(.0.as_ref()))]
    Transient(Box<dyn std::error::Error + Send + Sync>),

    /// Another broker took over the database. Its writes are refused from
    /// then on, so this broker should stop.
    #[error("another broker took over the database")]
    Fenced,
}

impl StorageError {
    #[cfg(any(feature = "sqlite", feature = "postgres"))]
    pub(crate) fn database(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        let error: Box<dyn std::error::Error + Send + Sync> = Box::new(error);
        #[cfg(feature = "sqlite")]
        match error.downcast_ref() {
            Some(rusqlite::Error::SqliteFailure(failure, _))
                if matches!(
                    failure.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                ) =>
            {
                return StorageError::Transient(error);
            }
            // A stored value that can't be read as what it stands for
            Some(
                rusqlite::Error::FromSqlConversionFailure(..)
                | rusqlite::Error::InvalidColumnType(..)
                | rusqlite::Error::IntegralValueOutOfRange(..)
                | rusqlite::Error::Utf8Error(..),
            ) => return StorageError::SerializationError(error.to_string()),
            _ => {}
        }
        StorageError::Database(error)
    }
}

/// An error with its causes, which tokio-postgres and deadpool leave out
/// of their messages, such as the server's reason for refusing a query.
pub(crate) fn error_chain(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !message.contains(&cause_text) {
            message = format!("{message}: {cause_text}");
        }
        source = cause.source();
    }
    message
}

/// Result type for storage operations.
pub type Result<T> = std::result::Result<T, StorageError>;

/// Serializable session state for persistence.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionState {
    pub client_id: String,
    pub subscriptions: Vec<StoredSubscription>,
    /// Seconds the session outlives its connection; None means forever
    pub expiry_interval: Option<u32>,
    /// Unix time in milliseconds when the session expires, once its
    /// connection has ended
    pub expires_at: Option<u64>,
    pub owner: SessionOwner,
}

/// Who a session belongs to. Only a client authenticated as the same user,
/// on a listener with the same mount point, may resume it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionOwner {
    /// The user the client authenticated as; None for anonymous clients
    pub username: Option<String>,
    /// The mount point of the listener the client connected to
    pub mount_point: Option<String>,
}

/// A subscription, the QoS granted for it and its MQTT 5 options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredSubscription {
    pub topic_filter: String,
    pub qos: QoS,
    /// Don't deliver messages published by the subscribing client itself
    pub no_local: bool,
    /// Keep the RETAIN flag of forwarded messages as published
    pub retain_as_published: bool,
    /// Sent back with every message delivered through the subscription
    pub subscription_identifier: Option<u32>,
}

impl StoredSubscription {
    /// A subscription without MQTT 5 options, as MQTT 3.x clients make.
    pub fn new(topic_filter: impl Into<String>, qos: QoS) -> Self {
        Self {
            topic_filter: topic_filter.into(),
            qos,
            no_local: false,
            retain_as_published: false,
            subscription_identifier: None,
        }
    }
}

/// Will message for persistence.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredWillMessage {
    pub topic: String,
    pub payload: Bytes,
    pub qos: QoS,
    pub retain: bool,
    /// Seconds to wait after the connection ends before publishing
    pub delay_secs: u32,
    pub properties: MessageProperties,
}

/// A ban the broker keeps across restarts. The broker gives `kind` and
/// `value` their meaning; stores only keep them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredBan {
    /// What the ban matches clients by: `clientid`, `username` or `address`
    pub kind: String,
    /// The client id or user name, or the address or CIDR block
    pub value: String,
    /// `value` is matched as a prefix
    pub prefix: bool,
    pub reason: Option<String>,
    /// Unix seconds
    pub created_at: u64,
    /// Unix seconds; None for a ban that never expires
    pub expires_at: Option<u64>,
}

/// Where the broker keeps its bans. A ban is named by its kind, value and
/// prefix flag.
#[async_trait]
pub trait BanStore: Send + Sync {
    /// Every stored ban, expired ones included.
    async fn load_bans(&self) -> Result<Vec<StoredBan>>;

    /// Stores `ban`, replacing the one with the same kind, value and prefix.
    async fn store_ban(&self, ban: &StoredBan) -> Result<()>;

    /// Removes the ban with this kind, value and prefix, if there is one.
    async fn delete_ban(&self, kind: &str, value: &str, prefix: bool) -> Result<()>;
}

/// Inflight message for QoS 1/2 tracking.
#[derive(Debug, Clone, PartialEq)]
pub struct InflightMessage {
    pub packet_id: u16,
    pub topic: String,
    /// None once `released`, when only PUBREL remains to be sent
    pub payload: Option<Bytes>,
    pub qos: QoS,
    pub dup: bool,
    /// The RETAIN flag the message was sent with, which a resend keeps
    pub retain: bool,
    /// Unix time in milliseconds when the message expires, if it does
    pub expires_at: Option<u64>,
    /// Position in the order messages were sent, for resending in order
    pub sequence: u64,
    /// The client acknowledged a QoS 2 message with PUBREC, so PUBREL is
    /// what remains to be sent
    pub released: bool,
    pub properties: MessageProperties,
    /// Identifiers of the subscriptions the message matched, sent with it
    pub subscription_identifiers: Vec<u32>,
}

/// Trait for retained message storage.
///
/// Retained messages are stored per topic and delivered to new subscribers.
#[async_trait]
pub trait RetainedMessageStore: Send + Sync {
    /// Stores the retained message of a topic, or with None clears it.
    /// Returns whether the topic held a retained message before, expired or
    /// not, so a caller can keep `count_retained` up to date itself.
    async fn store_retained(&self, topic: &str, message: Option<Message>) -> Result<bool>;

    /// At most `limit` of the retained messages matching `topic_filter`, in
    /// the byte order of their topics, from the first topic after `after`
    /// on. Expired messages are left out. Fewer than `limit` means there
    /// are no more.
    async fn get_retained_page(
        &self,
        topic_filter: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Message>>;

    /// Every retained message matching `topic_filter`, a page at a time.
    async fn get_retained(&self, topic_filter: &str) -> Result<Vec<Message>> {
        const PAGE: usize = 1000;
        let mut found: Vec<Message> = Vec::new();
        loop {
            let after = found.last().map(|m| m.topic.to_string());
            let page = self
                .get_retained_page(topic_filter, after.as_deref(), PAGE)
                .await?;
            let last = page.len() < PAGE;
            found.extend(page);
            if last {
                return Ok(found);
            }
        }
    }

    /// Clear all retained messages.
    async fn clear_all_retained(&self) -> Result<()>;

    /// How many retained messages are stored, expired ones included until
    /// `remove_expired_retained` removes them.
    async fn count_retained(&self) -> Result<usize>;

    /// Removes the expired retained messages, which lookups already leave
    /// out, and returns how many. The broker calls it now and then rather
    /// than have each lookup delete them.
    async fn remove_expired_retained(&self) -> Result<usize> {
        Ok(0)
    }
}

/// The topics a lookup with a topic filter reads, before each is matched
/// against the filter.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TopicRange {
    /// A filter without wildcards matches one topic
    Exact(String),
    /// Those from `start` on, and before `end` if there is one: all that
    /// start with the filter's part before its first wildcard
    From { start: String, end: Option<String> },
}

impl TopicRange {
    pub(crate) fn of(filter: &str) -> TopicRange {
        let Some(wildcard) = filter.find(['+', '#']) else {
            return TopicRange::Exact(filter.to_string());
        };
        // `a/#` matches `a` too
        let start = filter[..wildcard].trim_end_matches('/').to_string();
        let end = after_prefix(&start);
        TopicRange::From { start, end }
    }
}

/// The first string after every string starting with `prefix`, in byte
/// order, which UTF-8 shares with code point order; None if there is none.
fn after_prefix(prefix: &str) -> Option<String> {
    let mut chars: Vec<char> = prefix.chars().collect();
    while let Some(last) = chars.pop() {
        let next = match last as u32 + 1 {
            0xD800 => Some('\u{E000}'),
            next => char::from_u32(next),
        };
        if let Some(next) = next {
            chars.push(next);
            return Some(chars.into_iter().collect());
        }
    }
    None
}

/// A change to what is stored for one client.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Write {
    /// Store the session, replacing what was stored for the client
    SaveSession(SessionState),
    /// Add a subscription to the stored session, replacing the one with the
    /// same topic filter; nothing if no session is stored
    AddSubscription(StoredSubscription),
    /// Remove the stored session's subscription to this topic filter
    RemoveSubscription(String),
    /// Record when the session expires, if it is stored
    SetExpiry {
        /// Seconds the session outlives its connection
        interval: u32,
        /// Unix time in milliseconds when it expires
        expires_at: u64,
    },
    StoreWill(StoredWillMessage),
    DeleteWill,
    /// Store a QoS 1 or 2 message sent and not yet acknowledged
    StoreInflight(InflightMessage),
    /// The client sent PUBREC for this packet id: keep the record, drop the
    /// payload
    ReleaseInflight(u16),
    /// The client acknowledged this packet id for good
    RemoveInflight(u16),
    /// The client's QoS 2 message with this packet id was taken, and PUBREL
    /// is awaited
    StoreIncomingQos2(u16),
    /// The client sent PUBREL for this packet id
    RemoveIncomingQos2(u16),
    /// Store a message queued for the client, at this position in its
    /// queue
    Enqueue(u64, Message),
    /// Remove the message at this position of the client's queue, which
    /// left the queue: sent, in the batch storing it in flight, or dropped
    Dequeue(u64),
    /// Drop the stored queue
    ClearQueue,
    /// Remove the session and everything stored with it
    DeleteSession,
}

/// Where the broker keeps what it holds for each client: its session and
/// subscriptions, will, queue, in-flight messages and the packet ids of
/// incoming QoS 2 messages awaiting PUBREL.
///
/// Every change goes through `apply`; the other methods read.
#[async_trait]
pub trait BatchStore: Send + Sync {
    /// Names the backend, as the broker's admin API reports it.
    fn backend_name(&self) -> &'static str {
        "custom"
    }

    /// Where the broker keeps its bans. Without one, the broker keeps them
    /// in memory, and they last until it stops.
    fn ban_store(self: Arc<Self>) -> Option<Arc<dyn BanStore>> {
        None
    }

    /// Whether stored queues outlive the process. Stores that don't are
    /// never asked to keep a copy of the queues the broker already holds.
    fn is_durable(&self) -> bool {
        true
    }

    /// Apply `writes` for `client_id` in order. Each write may be applied
    /// again after a failure, so applying one twice must be harmless.
    /// Stores that can should apply the batch atomically.
    async fn apply(&self, client_id: &str, writes: &[Write]) -> Result<()>;

    /// The client's stored session, with its subscriptions in no particular
    /// order.
    async fn load_session(&self, client_id: &str) -> Result<Option<SessionState>>;

    /// The client ids of every stored session.
    async fn list_sessions(&self) -> Result<Vec<String>>;

    /// The client's stored will.
    async fn load_will(&self, client_id: &str) -> Result<Option<StoredWillMessage>>;

    /// The client's messages sent and not yet acknowledged, in no particular
    /// order.
    async fn load_inflight(&self, client_id: &str) -> Result<Vec<InflightMessage>>;

    /// The packet ids of the client's QoS 2 messages awaiting PUBREL.
    async fn load_incoming_qos2(&self, client_id: &str) -> Result<Vec<u16>>;

    /// The client's queued messages, each with its position in the queue,
    /// in that order.
    async fn load_queue(&self, client_id: &str) -> Result<Vec<(u64, Message)>>;

    /// Removes the client's session and everything stored with it, as
    /// `Write::DeleteSession` does.
    async fn delete_session(&self, client_id: &str) -> Result<()> {
        self.apply(client_id, &[Write::DeleteSession]).await
    }

    /// The client's stored session with everything stored with it.
    async fn load_client(&self, client_id: &str) -> Result<Option<StoredClient>> {
        let Some(session) = self.load_session(client_id).await? else {
            return Ok(None);
        };
        Ok(Some(StoredClient {
            session,
            will: self.load_will(client_id).await?,
            queue: self.load_queue(client_id).await?,
            inflight: self.load_inflight(client_id).await?,
            incoming_qos2: self.load_incoming_qos2(client_id).await?,
        }))
    }

    /// At most `limit` stored sessions, each with everything stored with
    /// it, ordered by client id in an order of the store's choosing, from
    /// the first after the client id `after` on. Fewer than `limit` means
    /// there are no more. The broker restores its sessions with this as it
    /// starts, a page at a time.
    ///
    /// A client whose rows can't be decoded has its entry fail with
    /// `StorageError::SerializationError`; others fail the whole call.
    ///
    /// The default reads each client on its own; stores should read each
    /// kind of row for the whole page at once.
    async fn load_clients(&self, after: Option<&str>, limit: usize) -> Result<Vec<LoadedClient>> {
        let mut client_ids = self.list_sessions().await?;
        client_ids.retain(|id| after.is_none_or(|after| id.as_str() > after));
        client_ids.sort_unstable();
        client_ids.truncate(limit);
        let mut clients = Vec::with_capacity(client_ids.len());
        for client_id in client_ids {
            let stored = match self.load_client(&client_id).await {
                Ok(Some(stored)) => Ok(stored),
                // Deleted since it was listed
                Ok(None) => continue,
                Err(e @ StorageError::SerializationError(_)) => Err(e),
                Err(e) => return Err(e),
            };
            clients.push(LoadedClient { client_id, stored });
        }
        Ok(clients)
    }
}

/// A client's stored session with everything stored with it.
#[derive(Debug, Clone)]
pub struct StoredClient {
    pub session: SessionState,
    pub will: Option<StoredWillMessage>,
    /// Its queued messages, each with its position in the queue, in that
    /// order
    pub queue: Vec<(u64, Message)>,
    /// In no particular order
    pub inflight: Vec<InflightMessage>,
    /// The packet ids of its QoS 2 messages awaiting PUBREL
    pub incoming_qos2: Vec<u16>,
}

/// A client as `BatchStore::load_clients` returns it.
#[derive(Debug)]
pub struct LoadedClient {
    pub client_id: String,
    /// What is stored for it, or `StorageError::SerializationError` when
    /// some of it can't be decoded
    pub stored: Result<StoredClient>,
}

/// Combined trait for full MQTT storage functionality.
///
/// Implementations provide all storage capabilities needed by an MQTT broker.
pub trait MqttStore: BatchStore + RetainedMessageStore {}

/// Blanket implementation for any type implementing all storage traits.
impl<T> MqttStore for T where T: BatchStore + RetainedMessageStore {}

#[cfg(test)]
mod range_tests {
    use super::TopicRange;

    fn from(start: &str, end: Option<&str>) -> TopicRange {
        TopicRange::From {
            start: start.into(),
            end: end.map(str::to_string),
        }
    }

    #[test]
    fn test_topic_ranges() {
        assert_eq!(TopicRange::of("a/b"), TopicRange::Exact("a/b".into()));
        assert_eq!(TopicRange::of("#"), from("", None));
        assert_eq!(TopicRange::of("+/c"), from("", None));
        assert_eq!(TopicRange::of("a/#"), from("a", Some("b")));
        assert_eq!(TopicRange::of("a/b/+/c"), from("a/b", Some("a/c")));
        assert_eq!(TopicRange::of("a%_\\/+"), from("a%_\\", Some("a%_]")));
        assert_eq!(
            TopicRange::of("\u{D7FF}/#"),
            from("\u{D7FF}", Some("\u{E000}"))
        );
        assert_eq!(
            TopicRange::of("a\u{10FFFF}/#"),
            from("a\u{10FFFF}", Some("b"))
        );
        assert_eq!(TopicRange::of("\u{10FFFF}/#"), from("\u{10FFFF}", None));
    }
}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;

    #[test]
    fn test_a_busy_sqlite_database_is_transient() {
        let busy = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            None,
        );
        assert!(matches!(
            StorageError::database(busy),
            StorageError::Transient(_)
        ));
        let constraint = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT),
            None,
        );
        assert!(matches!(
            StorageError::database(constraint),
            StorageError::Database(_)
        ));
    }
}
