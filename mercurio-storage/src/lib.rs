#![doc = include_str!("../README.md")]
#[cfg(any(feature = "sqlite", feature = "postgres"))]
mod encoding;
pub mod memory;

#[cfg(feature = "postgres")]
pub mod postgres;
#[cfg(feature = "sqlite")]
pub mod sqlite;

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
        if let Some(rusqlite::Error::SqliteFailure(failure, _)) = error.downcast_ref() {
            if matches!(
                failure.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ) {
                return StorageError::Transient(error);
            }
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
#[derive(Debug, Clone)]
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
#[derive(Debug, Clone)]
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
#[derive(Debug, Clone)]
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

/// Trait for session persistence.
///
/// Implementations store and retrieve session state for clients,
/// enabling session resumption across broker restarts.
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Save session state for a client.
    async fn save_session(&self, client_id: &str, session: &SessionState) -> Result<()>;

    /// Load session state for a client.
    async fn load_session(&self, client_id: &str) -> Result<Option<SessionState>>;

    /// Delete session state for a client.
    async fn delete_session(&self, client_id: &str) -> Result<()>;

    /// List all stored session client IDs.
    async fn list_sessions(&self) -> Result<Vec<String>>;
}

/// Trait for retained message storage.
///
/// Retained messages are stored per topic and delivered to new subscribers.
#[async_trait]
pub trait RetainedMessageStore: Send + Sync {
    /// Store a retained message for a topic.
    /// Pass None to clear the retained message.
    async fn store_retained(&self, topic: &str, message: Option<Message>) -> Result<()>;

    /// Get retained messages matching a topic filter (supports wildcards).
    async fn get_retained(&self, topic_filter: &str) -> Result<Vec<Message>>;

    /// Clear all retained messages.
    async fn clear_all_retained(&self) -> Result<()>;

    /// How many retained messages are stored, expired ones possibly
    /// included until they are next looked up.
    async fn count_retained(&self) -> Result<usize>;
}

/// Trait for will message storage.
///
/// Will messages are published when a client disconnects abnormally.
#[async_trait]
pub trait WillStore: Send + Sync {
    /// Store a will message for a client.
    async fn store_will(&self, client_id: &str, will: &StoredWillMessage) -> Result<()>;

    /// Get the will message for a client.
    async fn get_will(&self, client_id: &str) -> Result<Option<StoredWillMessage>>;

    /// Delete the will message for a client.
    async fn delete_will(&self, client_id: &str) -> Result<()>;
}

/// Trait for inflight message storage (QoS 1/2 tracking).
///
/// Tracks messages that have been sent but not yet acknowledged,
/// enabling message retry and exactly-once delivery.
#[async_trait]
pub trait InflightStore: Send + Sync {
    /// Store an inflight message for a client.
    async fn store_inflight(
        &self,
        client_id: &str,
        packet_id: u16,
        message: &InflightMessage,
    ) -> Result<()>;

    /// Get an inflight message by packet ID.
    async fn get_inflight(
        &self,
        client_id: &str,
        packet_id: u16,
    ) -> Result<Option<InflightMessage>>;

    /// Remove an inflight message (after acknowledgment).
    async fn remove_inflight(&self, client_id: &str, packet_id: u16) -> Result<()>;

    /// Mark a QoS 2 message as released: the client sent PUBREC, so only
    /// PUBREL remains and the payload is no longer needed.
    async fn release_inflight(&self, client_id: &str, packet_id: u16) -> Result<()>;

    /// Get all inflight messages for a client (for retry on reconnect).
    async fn get_all_inflight(&self, client_id: &str) -> Result<Vec<InflightMessage>>;

    /// Record that the client's QoS 2 message with this packet id was
    /// taken and acknowledged with PUBREC, so a copy the client sends again
    /// before its PUBREL isn't forwarded twice.
    async fn store_incoming_qos2(&self, client_id: &str, packet_id: u16) -> Result<()>;

    /// The client sent PUBREL for this packet id.
    async fn remove_incoming_qos2(&self, client_id: &str, packet_id: u16) -> Result<()>;

    /// The packet ids of the client's QoS 2 messages awaiting PUBREL.
    async fn get_incoming_qos2(&self, client_id: &str) -> Result<Vec<u16>>;
}

/// Messages waiting for disconnected clients, kept so they survive a
/// restart. Each message is identified by its sequence in the client's queue.
#[async_trait]
pub trait QueueStore: Send + Sync {
    /// Whether stored queues outlive the process. Stores that don't are
    /// never asked to keep a copy of the queues the broker already holds.
    fn is_durable(&self) -> bool {
        true
    }

    /// Stores `message` at position `sequence` of the client's queue,
    /// replacing what is stored there.
    async fn enqueue(&self, client_id: &str, sequence: u64, message: &Message) -> Result<()>;

    /// The client's queued messages, in sequence order.
    async fn load_queue(&self, client_id: &str) -> Result<Vec<(u64, Message)>>;

    /// Removes the client's stored queue.
    async fn clear_queue(&self, client_id: &str) -> Result<()>;
}

/// A change to what is stored for one client.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Write {
    /// Store the session, replacing what was stored for the client
    SaveSession(SessionState),
    /// Replace the session's subscriptions, creating the session if needed
    SaveSubscriptions(Vec<StoredSubscription>),
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
    /// Store a message queued for a disconnected client, at this position
    /// in its queue
    Enqueue(u64, Message),
    /// Drop the stored queue, once the client took it
    ClearQueue,
    /// Remove the session and everything stored with it
    DeleteSession,
}

/// Applies a client's changes in batches, in order.
#[async_trait]
pub trait BatchStore: SessionStore + WillStore + InflightStore + QueueStore {
    /// Names the backend, as the broker's admin API reports it.
    fn backend_name(&self) -> &'static str {
        "custom"
    }

    /// Where the broker keeps its bans. Without one, the broker keeps them
    /// in memory, and they last until it stops.
    fn ban_store(self: Arc<Self>) -> Option<Arc<dyn BanStore>> {
        None
    }

    /// Apply `writes` for `client_id` in order. Each write may be applied
    /// again after a failure, so applying one twice must be harmless.
    /// Stores that can should apply the batch atomically.
    async fn apply(&self, client_id: &str, writes: &[Write]) -> Result<()> {
        apply_each(self, client_id, writes).await
    }
}

/// Applies `writes` one at a time through the store's other methods: what
/// `BatchStore::apply` does unless a store overrides it. A store that does
/// can hand it the changes it has no batch form for, such as kinds of
/// change added in later versions.
pub async fn apply_each<S: BatchStore + ?Sized>(
    store: &S,
    client_id: &str,
    writes: &[Write],
) -> Result<()> {
    for write in writes {
        match write {
            Write::SaveSession(state) => store.save_session(client_id, state).await?,
            Write::SaveSubscriptions(subscriptions) => {
                let mut state = store
                    .load_session(client_id)
                    .await?
                    .unwrap_or(SessionState {
                        client_id: client_id.to_string(),
                        subscriptions: Vec::new(),
                        expiry_interval: None,
                        expires_at: None,
                        owner: SessionOwner::default(),
                    });
                state.subscriptions = subscriptions.clone();
                store.save_session(client_id, &state).await?;
            }
            Write::SetExpiry {
                interval,
                expires_at,
            } => {
                if let Some(mut state) = store.load_session(client_id).await? {
                    state.expiry_interval = Some(*interval);
                    state.expires_at = Some(*expires_at);
                    store.save_session(client_id, &state).await?;
                }
            }
            Write::StoreWill(will) => store.store_will(client_id, will).await?,
            Write::DeleteWill => store.delete_will(client_id).await?,
            Write::StoreInflight(message) => {
                store
                    .store_inflight(client_id, message.packet_id, message)
                    .await?
            }
            Write::ReleaseInflight(packet_id) => {
                store.release_inflight(client_id, *packet_id).await?
            }
            Write::RemoveInflight(packet_id) => {
                store.remove_inflight(client_id, *packet_id).await?
            }
            Write::StoreIncomingQos2(packet_id) => {
                store.store_incoming_qos2(client_id, *packet_id).await?
            }
            Write::RemoveIncomingQos2(packet_id) => {
                store.remove_incoming_qos2(client_id, *packet_id).await?
            }
            Write::Enqueue(sequence, message) => {
                store.enqueue(client_id, *sequence, message).await?
            }
            Write::ClearQueue => store.clear_queue(client_id).await?,
            Write::DeleteSession => {
                store.delete_session(client_id).await?;
                store.delete_will(client_id).await?;
                store.clear_queue(client_id).await?;
                for message in store.get_all_inflight(client_id).await? {
                    store.remove_inflight(client_id, message.packet_id).await?;
                }
                for packet_id in store.get_incoming_qos2(client_id).await? {
                    store.remove_incoming_qos2(client_id, packet_id).await?;
                }
            }
        }
    }
    Ok(())
}

/// Combined trait for full MQTT storage functionality.
///
/// Implementations provide all storage capabilities needed by an MQTT broker.
pub trait MqttStore: BatchStore + RetainedMessageStore {}

/// Blanket implementation for any type implementing all storage traits.
impl<T> MqttStore for T where T: BatchStore + RetainedMessageStore {}

#[cfg(test)]
pub(crate) mod testing {
    use super::{BanStore, BatchStore, StoredBan, Write};

    /// What every store does with the packet ids of incoming QoS 2
    /// messages: batches apply in order, applying one twice is harmless, and
    /// deleting the session removes them.
    pub(crate) async fn incoming_qos2_round_trip(store: &dyn BatchStore) {
        let sorted = |mut ids: Vec<u16>| {
            ids.sort_unstable();
            ids
        };
        let batch = [
            Write::StoreIncomingQos2(1),
            Write::StoreIncomingQos2(u16::MAX),
            Write::StoreIncomingQos2(7),
            Write::RemoveIncomingQos2(7),
            Write::RemoveIncomingQos2(1),
            Write::StoreIncomingQos2(1),
        ];
        for _ in 0..2 {
            store.apply("c", &batch).await.unwrap();
            assert_eq!(
                sorted(store.get_incoming_qos2("c").await.unwrap()),
                [1, u16::MAX]
            );
        }
        store
            .apply("other", &[Write::StoreIncomingQos2(1)])
            .await
            .unwrap();
        // Removing what isn't stored is no error
        store
            .apply("c", &[Write::RemoveIncomingQos2(2)])
            .await
            .unwrap();

        store.apply("c", &[Write::DeleteSession]).await.unwrap();
        assert!(store.get_incoming_qos2("c").await.unwrap().is_empty());
        assert_eq!(store.get_incoming_qos2("other").await.unwrap(), [1]);
    }

    pub(crate) fn ban(kind: &str, value: &str, prefix: bool) -> StoredBan {
        StoredBan {
            kind: kind.into(),
            value: value.into(),
            prefix,
            reason: None,
            created_at: 1_800_000_000,
            expires_at: None,
        }
    }

    /// What every `BanStore` does: a ban is named by its kind, value and
    /// prefix flag, and storing one again replaces it.
    pub(crate) async fn bans_round_trip(store: &dyn BanStore) {
        let sorted = |mut bans: Vec<StoredBan>| {
            bans.sort_by(|a, b| (&a.kind, &a.value, a.prefix).cmp(&(&b.kind, &b.value, b.prefix)));
            bans
        };
        assert!(store.load_bans().await.unwrap().is_empty());

        let mut exact = ban("clientid", "c", false);
        exact.reason = Some("spam".into());
        exact.expires_at = Some(1_800_003_600);
        let prefixed = ban("clientid", "c", true);
        let address = ban("address", "10.0.0.0/8", false);
        for stored in [&exact, &prefixed, &address] {
            store.store_ban(stored).await.unwrap();
        }
        assert_eq!(
            sorted(store.load_bans().await.unwrap()),
            vec![address.clone(), exact.clone(), prefixed.clone()]
        );

        exact.reason = None;
        exact.expires_at = None;
        exact.created_at = 1_800_000_100;
        store.store_ban(&exact).await.unwrap();
        store.delete_ban("clientid", "c", true).await.unwrap();
        // Deleting what isn't stored is no error
        store.delete_ban("username", "c", false).await.unwrap();
        assert_eq!(
            sorted(store.load_bans().await.unwrap()),
            vec![address, exact]
        );
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
