//! Storage abstraction layer for Mercurio MQTT broker.
//!
//! This crate provides traits for persistent storage of MQTT broker state,
//! allowing different storage backends (in-memory, SQLite, PostgreSQL).

#[cfg(any(feature = "sqlite", feature = "postgres"))]
mod encoding;
pub mod memory;

#[cfg(feature = "postgres")]
pub mod postgres;
#[cfg(feature = "sqlite")]
pub mod sqlite;

use async_trait::async_trait;
use bytes::Bytes;
use mercurio_core::{
    message::{Message, MessageProperties},
    qos::QoS,
};
use thiserror::Error;

/// Error type for storage operations.
#[derive(Error, Debug)]
pub enum StorageError {
    #[error("Session not found: {0}")]
    SessionNotFound(String),

    #[error("Storage operation failed: {0}")]
    OperationFailed(String),

    #[error("Serialization error: {0}")]
    SerializationError(String),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[cfg(feature = "sqlite")]
    #[error("Database error: {0}")]
    DatabaseError(#[from] rusqlite::Error),

    #[cfg(feature = "postgres")]
    #[error("PostgreSQL: {}", error_chain(.0))]
    Postgres(#[from] tokio_postgres::Error),
}

/// An error with its causes, which tokio-postgres and deadpool leave out
/// of their messages, such as the server's reason for refusing a query.
#[cfg(feature = "postgres")]
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
    pub clean_start: bool,
    /// Seconds the session outlives its connection; None means forever
    pub expiry_interval: Option<u32>,
    /// Unix time in milliseconds when the session expires, once its
    /// connection has ended
    pub expires_at: Option<u64>,
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

/// Inflight message for QoS 1/2 tracking.
#[derive(Debug, Clone)]
pub struct InflightMessage {
    pub packet_id: u16,
    pub topic: String,
    pub payload: Option<Bytes>,
    pub qos: QoS,
    pub dup: bool,
    /// Position in the order messages were sent, for resending in order
    pub sequence: u64,
    /// The client acknowledged a QoS 2 message with PUBREC, so PUBREL is
    /// what remains to be sent
    pub released: bool,
    pub properties: MessageProperties,
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

    async fn enqueue(&self, client_id: &str, sequence: u64, message: &Message) -> Result<()>;

    /// The client's queued messages, in sequence order.
    async fn load_queue(&self, client_id: &str) -> Result<Vec<(u64, Message)>>;

    async fn clear_queue(&self, client_id: &str) -> Result<()>;
}

/// A change to what is stored for one client.
#[derive(Debug, Clone)]
pub enum Write {
    SaveSession(SessionState),
    /// Replace the session's subscriptions, creating the session if needed
    SaveSubscriptions(Vec<StoredSubscription>),
    /// Record when the session expires, if it is stored
    SetExpiry {
        interval: u32,
        expires_at: u64,
    },
    StoreWill(StoredWillMessage),
    DeleteWill,
    StoreInflight(InflightMessage),
    ReleaseInflight(u16),
    RemoveInflight(u16),
    Enqueue(u64, Message),
    ClearQueue,
    /// Remove the session and everything stored with it
    DeleteSession,
}

/// Applies a client's changes in batches, in order.
#[async_trait]
pub trait BatchStore: SessionStore + WillStore + InflightStore + QueueStore {
    /// Apply `writes` for `client_id` in order. Each write may be applied
    /// again after a failure, so applying one twice must be harmless.
    /// Stores that can should apply the batch atomically.
    async fn apply(&self, client_id: &str, writes: &[Write]) -> Result<()> {
        for write in writes {
            match write {
                Write::SaveSession(state) => self.save_session(client_id, state).await?,
                Write::SaveSubscriptions(subscriptions) => {
                    let mut state = self.load_session(client_id).await?.unwrap_or(SessionState {
                        client_id: client_id.to_string(),
                        subscriptions: Vec::new(),
                        clean_start: false,
                        expiry_interval: None,
                        expires_at: None,
                    });
                    state.subscriptions = subscriptions.clone();
                    self.save_session(client_id, &state).await?;
                }
                Write::SetExpiry {
                    interval,
                    expires_at,
                } => {
                    if let Some(mut state) = self.load_session(client_id).await? {
                        state.expiry_interval = Some(*interval);
                        state.expires_at = Some(*expires_at);
                        self.save_session(client_id, &state).await?;
                    }
                }
                Write::StoreWill(will) => self.store_will(client_id, will).await?,
                Write::DeleteWill => self.delete_will(client_id).await?,
                Write::StoreInflight(message) => {
                    self.store_inflight(client_id, message.packet_id, message)
                        .await?
                }
                Write::ReleaseInflight(packet_id) => {
                    self.release_inflight(client_id, *packet_id).await?
                }
                Write::RemoveInflight(packet_id) => {
                    self.remove_inflight(client_id, *packet_id).await?
                }
                Write::Enqueue(sequence, message) => {
                    self.enqueue(client_id, *sequence, message).await?
                }
                Write::ClearQueue => self.clear_queue(client_id).await?,
                Write::DeleteSession => {
                    self.delete_session(client_id).await?;
                    self.delete_will(client_id).await?;
                    self.clear_queue(client_id).await?;
                    for message in self.get_all_inflight(client_id).await? {
                        self.remove_inflight(client_id, message.packet_id).await?;
                    }
                }
            }
        }
        Ok(())
    }
}

/// Combined trait for full MQTT storage functionality.
///
/// Implementations provide all storage capabilities needed by an MQTT broker.
pub trait MqttStore: BatchStore + RetainedMessageStore {}

/// Blanket implementation for any type implementing all storage traits.
impl<T> MqttStore for T where T: BatchStore + RetainedMessageStore {}
