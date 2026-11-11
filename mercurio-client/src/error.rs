use mercurio_core::reason::ReasonCode;
use thiserror::Error;

/// Errors that can occur in the MQTT client.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum ClientError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// The connection couldn't be opened, or the broker closed it before
    /// answering CONNECT. The I/O error says why, as its
    /// [`kind`](std::io::Error::kind) does to a program.
    #[error("Connection failed: {0}")]
    ConnectionFailed(#[source] std::io::Error),

    /// The broker answered CONNECT with this failure, such as bad
    /// credentials. Reconnecting retries after Server Unavailable, Server
    /// Busy, Quota Exceeded and Connection Rate Exceeded, and stops after
    /// any other.
    #[error("Connection refused: {0:?}")]
    ConnectionRefused(ReasonCode),

    /// The broker refused the connection and named another server to use
    /// instead, `reason` saying whether the move is for good. With
    /// [`follow_redirects`](crate::ConnectOptions::follow_redirects), the
    /// last redirection when the hop limit was reached or no server it
    /// named could be read.
    #[error("Connection refused ({reason:?}), use {server_reference} instead")]
    #[non_exhaustive]
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

    /// The broker refused the subscription to the response topic of a
    /// [`request`](crate::MqttClient::request).
    #[error("Subscription to {topic} refused: {reason:?}")]
    #[non_exhaustive]
    SubscribeRefused { topic: String, reason: ReasonCode },

    /// The broker sent a packet the client didn't expect at that point.
    #[error("Protocol error: {0}")]
    Protocol(String),

    /// A packet the client was to send can't be encoded, as the broker
    /// would have to refuse it; nothing was sent.
    #[error("Packet can't be encoded: {0}")]
    Encode(#[from] mercurio_core::error::EncodeError),

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

    /// The client has stopped for good, after
    /// [`disconnect`](crate::MqttClient::disconnect) or after giving up on
    /// reconnecting, so the request couldn't be sent.
    #[error("The client has stopped")]
    Closed,

    /// The TLS configuration couldn't be used, or the broker's certificate
    /// or TLS settings were refused during the handshake. `source` holds
    /// the underlying error, such as the I/O error reading a file or the
    /// [`rustls::Error`] of the handshake, when there is one.
    #[error("TLS error: {context}{}", cause(.source))]
    #[non_exhaustive]
    Tls {
        /// What the client was doing
        context: String,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },
}

impl ClientError {
    /// Whether trying again later may succeed: the connection failed or
    /// ended, or the broker refused it for want of capacity (Server
    /// Unavailable, Server Busy, Quota Exceeded, Connection Rate
    /// Exceeded). Invalid requests, refused credentials, a redirection, TLS
    /// configuration and certificate errors and a stopped client aren't
    /// transient: repeating the same request meets the same answer.
    ///
    /// Reconnection uses the same classification for CONNACK refusals: it
    /// keeps trying after a transient one and stops after any other.
    pub fn is_transient(&self) -> bool {
        match self {
            ClientError::Io(_)
            | ClientError::ConnectionFailed(_)
            | ClientError::Timeout
            | ClientError::Disconnected
            | ClientError::BrokerViolation(_)
            | ClientError::Protocol(_)
            | ClientError::Packet(_) => true,
            ClientError::ConnectionRefused(reason) => transient_refusal(*reason),
            _ => false,
        }
    }

    pub(crate) fn tls(
        context: impl Into<String>,
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    ) -> Self {
        ClientError::Tls {
            context: context.into(),
            source,
        }
    }
}

/// Whether a broker refusing CONNECT with `reason` may take the client
/// later, the refusal being for want of capacity.
pub(crate) fn transient_refusal(reason: ReasonCode) -> bool {
    matches!(
        reason,
        ReasonCode::ServerUnavailable
            | ReasonCode::ServerBusy
            | ReasonCode::QuotaExceeded
            | ReasonCode::ConnectionRateExceeded
    )
}

fn cause(source: &Option<Box<dyn std::error::Error + Send + Sync>>) -> String {
    source
        .as_ref()
        .map_or_else(String::new, |source| format!(": {source}"))
}

/// The client's result type.
pub type Result<T> = std::result::Result<T, ClientError>;

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;

    #[test]
    fn failures_of_the_connection_are_transient() {
        for error in [
            ClientError::ConnectionFailed(io::ErrorKind::ConnectionRefused.into()),
            ClientError::Io(io::ErrorKind::BrokenPipe.into()),
            ClientError::Timeout,
            ClientError::Disconnected,
            ClientError::BrokerViolation(ReasonCode::ProtocolError),
        ] {
            assert!(error.is_transient(), "{error:?}");
        }
    }

    #[test]
    fn refusals_for_want_of_capacity_are_transient() {
        for reason in [
            ReasonCode::ServerUnavailable,
            ReasonCode::ServerBusy,
            ReasonCode::QuotaExceeded,
            ReasonCode::ConnectionRateExceeded,
        ] {
            assert!(ClientError::ConnectionRefused(reason).is_transient());
        }
        for reason in [
            ReasonCode::NotAuthorized,
            ReasonCode::BadUserNameOrPassword,
            ReasonCode::Banned,
            ReasonCode::UnsupportedProtocolVersion,
        ] {
            assert!(!ClientError::ConnectionRefused(reason).is_transient());
        }
    }

    #[test]
    fn mistakes_and_refusals_for_good_are_not_transient() {
        for error in [
            ClientError::Closed,
            ClientError::InvalidArgument("x".into()),
            ClientError::TooLong("topic"),
            ClientError::PacketTooLarge,
            ClientError::NotSupported("retained messages"),
            ClientError::PublishRefused(ReasonCode::NotAuthorized),
            ClientError::AuthenticationFailed("bad proof".into()),
            ClientError::Redirected {
                reason: ReasonCode::ServerMoved,
                server_reference: "other".into(),
            },
            ClientError::tls("no certificates", None),
        ] {
            assert!(!error.is_transient(), "{error:?}");
        }
    }

    #[test]
    fn tls_errors_keep_their_source() {
        let error = ClientError::tls(
            "reading the CA file",
            Some(io::Error::from(io::ErrorKind::NotFound).into()),
        );
        let source = std::error::Error::source(&error).expect("a source");
        assert_eq!(
            source.downcast_ref::<io::Error>().map(io::Error::kind),
            Some(io::ErrorKind::NotFound)
        );
        assert!(error.to_string().contains("reading the CA file"), "{error}");
    }
}
