use std::sync::Arc;

use bytes::Bytes;
use mercurio_core::{qos::QoS, reason::ReasonCode};

use crate::error::ClientError;
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

    /// The connection ended. With reconnection on, the client tries again
    /// and reports `Reconnected`, or `Stopped` if the broker refuses it
    /// for good; otherwise no more events follow.
    Disconnected { reason: DisconnectReason },

    /// The client reconnected after losing the connection. Without
    /// `session_present` the broker forgot the subscriptions.
    Reconnected { session_present: bool },

    /// Reconnecting failed for good, as when the broker refused the
    /// credentials; the last event before [`MqttClient::recv`] returns
    /// `None`.
    ///
    /// [`MqttClient::recv`]: crate::MqttClient::recv
    Stopped { error: Arc<ClientError> },
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
        /// A human-readable explanation, when the broker sent one
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
    /// The QoS the broker granted, which may be lower than requested
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
