//! Storage abstraction layer for Mercurio MQTT broker.
//!
//! This crate provides traits for persistent storage of MQTT broker state,
//! allowing different storage backends (in-memory, SQLite, Redis, etc.).

#[cfg(feature = "sqlite")]
mod encoding;
pub mod memory;

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

/// Combined trait for full MQTT storage functionality.
///
/// Implementations provide all storage capabilities needed by an MQTT broker.
pub trait MqttStore:
    SessionStore + RetainedMessageStore + WillStore + InflightStore + QueueStore
{
}

/// Blanket implementation for any type implementing all storage traits.
impl<T> MqttStore for T where
    T: SessionStore + RetainedMessageStore + WillStore + InflightStore + QueueStore
{
}
