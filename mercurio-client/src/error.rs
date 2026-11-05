use mercurio_core::reason::ReasonCode;
use thiserror::Error;

/// Errors that can occur in the MQTT client.
#[derive(Error, Debug)]
pub enum ClientError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Connection failed: {0}")]
    ConnectionFailed(String),

    #[error("Connection refused: {0:?}")]
    ConnectionRefused(ReasonCode),

    #[error("Connection refused ({reason:?}), use {server_reference} instead")]
    Redirected {
        reason: ReasonCode,
        server_reference: String,
    },

    #[error("Packet larger than the broker accepts")]
    PacketTooLarge,

    #[error("Not supported by the broker: {0}")]
    NotSupported(&'static str),

    /// The broker broke the protocol; the client disconnects with this
    /// reason
    #[error("Broker violated the protocol: {0:?}")]
    BrokerViolation(ReasonCode),

    #[error("Authentication failed: {0}")]
    AuthenticationFailed(String),

    #[error("Publish refused by the broker: {0:?}")]
    PublishRefused(ReasonCode),

    #[error("Not connected")]
    NotConnected,

    #[error("Already connected")]
    AlreadyConnected,

    #[error("Protocol error: {0}")]
    Protocol(String),

    #[error("Packet error: {0}")]
    Packet(#[from] mercurio_core::error::Error),

    #[error("Timeout")]
    Timeout,

    #[error("Client disconnected")]
    Disconnected,

    #[error("Invalid topic: {0}")]
    InvalidTopic(String),

    #[error("Send error")]
    SendError,

    #[error("TLS error: {0}")]
    Tls(String),
}

pub type Result<T> = std::result::Result<T, ClientError>;
