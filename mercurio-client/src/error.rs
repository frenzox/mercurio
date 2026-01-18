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
}

pub type Result<T> = std::result::Result<T, ClientError>;
