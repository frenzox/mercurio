use bytes::Bytes;
use mercurio_core::qos::QoS;

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

    /// Server initiated disconnect.
    ServerInitiated,

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
