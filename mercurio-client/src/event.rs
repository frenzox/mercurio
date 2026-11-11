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
    #[non_exhaustive]
    Message {
        topic: String,
        payload: Bytes,
        qos: QoS,
        retain: bool,
        properties: MessageProperties,
        /// Identifiers of the matching subscriptions that have one
        subscription_ids: Vec<u32>,
        /// With [`manual_acks`](crate::ConnectOptions::manual_acks) on, what
        /// to pass to [`MqttClient::ack`](crate::MqttClient::ack) once the
        /// message is taken care of; `None` for QoS 0 and otherwise.
        ack: Option<AckToken>,
    },

    /// The connection ended. With reconnection on, the client tries again
    /// and reports `Reconnected`, or `Stopped` if the broker refuses it
    /// for good; otherwise no more events follow.
    Disconnected { reason: DisconnectReason },

    /// The client reconnected after losing the connection. Without
    /// `session_present` the broker forgot the subscriptions.
    #[non_exhaustive]
    Reconnected { session_present: bool },

    /// With [`resubscribe`](crate::ConnectOptions::resubscribe) on, the
    /// broker refused to take back this subscription after reconnecting
    /// without a session; the client forgets it.
    #[non_exhaustive]
    ResubscribeFailed { topic: String, reason: ReasonCode },

    /// Reconnecting failed for good, as when the broker refused the
    /// credentials; the last event before [`MqttClient::recv`] returns
    /// `None`.
    ///
    /// [`MqttClient::recv`]: crate::MqttClient::recv
    #[non_exhaustive]
    Stopped { error: Arc<ClientError> },
}

/// The answer to [`MqttClient::request`](crate::MqttClient::request).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Response {
    /// The response topic it came on
    pub topic: String,
    pub payload: Bytes,
    pub properties: MessageProperties,
}

/// Identifies a received message to acknowledge, with manual
/// acknowledgement on. Only the connection it came on can acknowledge it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AckToken {
    /// Counts deliveries for the whole life of the client
    pub(crate) delivery: u64,
    pub(crate) packet_id: u16,
}

/// Reason for disconnection.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum DisconnectReason {
    /// Client initiated disconnect.
    ClientInitiated,

    /// The broker sent DISCONNECT, saying why.
    #[non_exhaustive]
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

/// What the broker settled for a connection in its CONNACK, with each
/// value the broker left out taking its MQTT default.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ConnAckInfo {
    /// The broker resumed a session it kept for this client id
    pub session_present: bool,
    /// Keep-alive in effect, in seconds: the broker's Server Keep Alive
    /// when it sent one, the requested one otherwise
    pub keep_alive: u16,
    /// Highest QoS the broker accepts in PUBLISH
    pub maximum_qos: QoS,
    /// Whether the broker accepts retained messages
    pub retain_available: bool,
    /// QoS 1 and 2 messages the broker takes from this client at once
    pub receive_maximum: u16,
    /// Largest packet the broker accepts, if it set a limit
    pub maximum_packet_size: Option<u32>,
    /// Highest topic alias the broker accepts from this client; 0 for none
    pub topic_alias_maximum: u16,
    /// Seconds the broker keeps the session after the connection closes:
    /// its own value when it overrode the requested one
    pub session_expiry: u32,
    /// The answer to `request_response_information`, often a topic prefix
    /// for response topics
    pub response_information: Option<String>,
    /// Whether subscriptions may use wildcards
    pub wildcard_subscriptions: bool,
    /// Whether shared subscriptions are available
    pub shared_subscriptions: bool,
    /// Whether subscriptions may carry identifiers
    pub subscription_identifiers: bool,
}

/// Result of a subscription request.
#[derive(Debug, Clone)]
#[non_exhaustive]
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
#[non_exhaustive]
pub struct UnsubscribeResult {
    pub topic: String,
    pub success: bool,
    /// Why the broker refused, or `NoSubscriptionExisted`
    pub reason: ReasonCode,
}
