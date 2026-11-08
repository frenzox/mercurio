use bytes::Bytes;
use mercurio_core::{qos::QoS, reason::ReasonCode};

use crate::options::MessageProperties;

/// Events received from the MQTT broker.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Event {
    /// A message was received on a subscribed topic.
    Message {
        topic: String,
        payload: Bytes,
        qos: QoS,
        retain: bool,
        properties: MessageProperties,
        /// Identifiers of the matching subscriptions that have one
        subscription_ids: Vec<u32>,
    },

    /// The client was disconnected from the broker.
    Disconnected { reason: DisconnectReason },

    /// The client reconnected after losing the connection. Without
    /// `session_present` the broker forgot the subscriptions.
    Reconnected { session_present: bool },
}

/// Reason for disconnection.
#[derive(Debug, Clone)]
#[non_exhaustive]
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
    /// Why the broker refused, when it did
    pub reason: ReasonCode,
}

/// Result of unsubscribing from one topic filter.
#[derive(Debug, Clone)]
pub struct UnsubscribeResult {
    pub topic: String,
    pub success: bool,
    /// Why the broker refused, or `NoSubscriptionExisted`
    pub reason: ReasonCode,
}
