use mercurio_core::reason::ReasonCode;
use thiserror::Error;

/// Errors that can occur in the MQTT client.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum ClientError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// The connection couldn't be opened, or the broker closed it before
    /// answering CONNECT.
    #[error("Connection failed: {0}")]
    ConnectionFailed(String),

    /// The broker answered CONNECT with this failure, such as bad
    /// credentials. Reconnecting retries after Server Unavailable, Server
    /// Busy, Quota Exceeded and Connection Rate Exceeded, and stops after
    /// any other.
    #[error("Connection refused: {0:?}")]
    ConnectionRefused(ReasonCode),

    /// The broker refused the connection and named another server to use
    /// instead, `reason` saying whether the move is for good.
    #[error("Connection refused ({reason:?}), use {server_reference} instead")]
    Redirected {
        reason: ReasonCode,
        server_reference: String,
    },

    /// A packet the client was to send exceeds the Maximum Packet Size
    /// the broker announced, or the largest packet MQTT can frame; nothing
    /// was sent.
    #[error("Packet larger than the broker accepts")]
    PacketTooLarge,

    /// This string or binary value is longer than the 65,535 bytes MQTT
    /// can carry; nothing was sent.
    #[error("{0} is longer than 65,535 bytes")]
    TooLong(&'static str),

    /// The request is invalid on its face, such as a topic name with
    /// wildcards or an empty list of filters; nothing was sent.
    #[error("Invalid argument: {0}")]
    InvalidArgument(String),

    /// The broker announced it lacks what the request needs: a QoS above
    /// its maximum, retained messages, or subscription identifiers.
    #[error("Not supported by the broker: {0}")]
    NotSupported(&'static str),

    /// The broker broke the protocol; the client disconnects with this
    /// reason
    #[error("Broker violated the protocol: {0:?}")]
    BrokerViolation(ReasonCode),

    /// The enhanced authentication exchange failed, on the client's side
    /// or the broker's.
    #[error("Authentication failed: {0}")]
    AuthenticationFailed(String),

    /// The broker answered a QoS 1 or 2 publish with this failure, such as
    /// Not Authorized; the message wasn't accepted.
    #[error("Publish refused by the broker: {0:?}")]
    PublishRefused(ReasonCode),

    /// The broker sent a packet the client didn't expect at that point.
    #[error("Protocol error: {0}")]
    Protocol(String),

    /// A packet from the broker couldn't be decoded.
    #[error("Packet error: {0}")]
    Packet(#[from] mercurio_core::error::Error),

    /// Connecting took longer than `connect_timeout`, or writing a packet
    /// longer than `write_timeout`, which ends the connection.
    #[error("Timeout")]
    Timeout,

    /// The connection ended before the request completed, or a QoS 0
    /// publish was made while disconnected; QoS 1 and 2 publishes wait for
    /// the reconnection instead.
    #[error("Client disconnected")]
    Disconnected,

    /// The client has stopped for good, so the request couldn't be sent.
    #[error("Send error")]
    SendError,

    /// The TLS configuration couldn't be used, or the handshake failed.
    #[error("TLS error: {0}")]
    Tls(String),
}

/// The client's result type.
pub type Result<T> = std::result::Result<T, ClientError>;
