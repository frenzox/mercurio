//! Storage abstraction layer for Mercurio MQTT broker.
//!
//! This crate provides traits for persistent storage of MQTT broker state,
//! allowing different storage backends (in-memory, SQLite, Redis, etc.).

pub mod memory;

#[cfg(feature = "sqlite")]
pub mod sqlite;

use async_trait::async_trait;
use bytes::Bytes;
use mercurio_core::{message::Message, qos::QoS};
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
    pub subscriptions: Vec<String>,
    pub clean_start: bool,
}

/// Will message for persistence.
#[derive(Debug, Clone)]
pub struct StoredWillMessage {
    pub topic: String,
    pub payload: Bytes,
    pub qos: QoS,
    pub retain: bool,
}

/// Inflight message for QoS 1/2 tracking.
#[derive(Debug, Clone)]
pub struct InflightMessage {
    pub packet_id: u16,
    pub topic: String,
    pub payload: Option<Bytes>,
    pub qos: QoS,
    pub dup: bool,
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

    /// Get all inflight messages for a client (for retry on reconnect).
    async fn get_all_inflight(&self, client_id: &str) -> Result<Vec<InflightMessage>>;
}

/// Combined trait for full MQTT storage functionality.
///
/// Implementations provide all storage capabilities needed by an MQTT broker.
pub trait MqttStore: SessionStore + RetainedMessageStore + WillStore + InflightStore {}

/// Blanket implementation for any type implementing all storage traits.
impl<T> MqttStore for T where T: SessionStore + RetainedMessageStore + WillStore + InflightStore {}
