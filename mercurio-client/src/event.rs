use bytes::Bytes;
use mercurio_core::{qos::QoS, reason::ReasonCode};

/// Events received from the MQTT broker.
#[derive(Debug, Clone)]
pub enum Event {
    /// A message was received on a subscribed topic.
    Message {
        topic: String,
        payload: Bytes,
        qos: QoS,
        retain: bool,
    },

    /// The client was disconnected from the broker.
    Disconnected { reason: DisconnectReason },
}

/// Reason for disconnection.
#[derive(Debug, Clone)]
pub enum DisconnectReason {
    /// Client initiated disconnect.
    ClientInitiated,

    /// The broker sent DISCONNECT, saying why.
    ServerInitiated {
        reason: ReasonCode,
        reason_string: Option<String>,
        /// Another server to use, when the broker is moving clients away
        server_reference: Option<String>,
    },

    /// The broker closed the connection without saying why.
    ConnectionClosed,

    /// Connection lost (network error).
    ConnectionLost,

    /// Keep-alive timeout.
    KeepAliveTimeout,

    /// Protocol error.
    ProtocolError(String),
}

/// Result of a subscription request.
#[derive(Debug, Clone)]
pub struct SubscribeResult {
    pub topic: String,
    pub qos: QoS,
    pub success: bool,
}
