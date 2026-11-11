//! What the admin API returns, serialized with the field names of the
//! `$CONTROL/broker/v1` protocol.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// `getBrokerInfo`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct BrokerInfo {
    pub version: String,
    /// Unix seconds
    pub started_at: u64,
    /// Seconds
    pub uptime: u64,
    /// The storage backend: `memory`, `sqlite`, `postgres`, or what a
    /// custom one calls itself
    pub storage: String,
    /// The `$SYS/broker/` counters, by their topic under it
    pub stats: BTreeMap<String, u64>,
}

/// One entry of `listListeners`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ListenerInfo {
    /// A TCP listener's address
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// A Unix socket listener's path
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// `mqtt` or `websocket`
    pub protocol: String,
    pub tls: bool,
    pub mount_point: Option<String>,
    /// Connections open on it, handshakes included
    pub connections: usize,
}

/// One entry of `listClients`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ClientSummary {
    #[serde(rename = "clientid")]
    pub client_id: String,
    pub username: Option<String>,
    pub connected: bool,
    /// The client's address and port, its last one when disconnected
    pub address: Option<String>,
    pub listener: Option<String>,
    /// 3, 4 or 5
    pub protocol_version: Option<u8>,
    /// Unix seconds, while connected
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connected_since: Option<u64>,
    /// Unix seconds, while disconnected
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disconnected_since: Option<u64>,
    pub subscriptions: usize,
    pub queued: usize,
    pub inflight: usize,
    /// Messages for the client the broker lost since it last connected,
    /// while it was away included
    pub dropped: u64,
}

/// `getClient`: the `listClients` entry with the subscriptions themselves,
/// the connection's settings and its traffic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ClientDetails {
    #[serde(rename = "clientid")]
    pub client_id: String,
    pub username: Option<String>,
    pub connected: bool,
    pub address: Option<String>,
    pub listener: Option<String>,
    pub protocol_version: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connected_since: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disconnected_since: Option<u64>,
    pub subscriptions: Vec<ClientSubscription>,
    pub queued: usize,
    pub inflight: usize,
    pub keep_alive: Option<u16>,
    pub clean_start: Option<bool>,
    /// Seconds the session outlives its connection; null for never
    pub session_expiry: Option<u64>,
    /// Seconds left, while disconnected
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_expires_in: Option<u64>,
    pub will_pending: bool,
    pub messages_in: u64,
    pub messages_out: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub dropped: u64,
}

/// A subscription in `getClient`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ClientSubscription {
    pub filter: String,
    pub qos: u8,
    pub no_local: bool,
    pub retain_as_published: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscription_identifier: Option<u32>,
}

/// One entry of `listRetained`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct RetainedSummary {
    pub topic: String,
    pub qos: u8,
    /// Payload bytes
    pub size: usize,
    /// Seconds left; null for never
    pub expires_in: Option<u64>,
    /// The payload's first 64 bytes as text; null when they aren't UTF-8
    pub preview: Option<String>,
}

/// `getRetained`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct RetainedMessage {
    pub topic: String,
    pub qos: u8,
    /// The payload, when it is UTF-8
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<String>,
    /// The payload, when it isn't UTF-8
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload_base64: Option<String>,
    pub properties: RetainedProperties,
    /// Seconds left; null for never
    pub expires_in: Option<u64>,
}

/// The MQTT 5 properties stored with a retained message.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct RetainedProperties {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload_format_indicator: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_topic: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_data_base64: Option<String>,
    /// Name and value pairs, in order
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub user_properties: Vec<(String, String)>,
}

/// One entry of `listSubscriptions`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct SubscriptionSummary {
    pub filter: String,
    pub subscribers: usize,
}

/// One entry of `getSubscribers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct SubscriberInfo {
    #[serde(rename = "clientid")]
    pub client_id: String,
    pub qos: u8,
    pub no_local: bool,
    pub retain_as_published: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscription_identifier: Option<u32>,
}

/// What a ban matches clients by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum BanKind {
    /// The client id, once the broker assigned one
    ClientId,
    /// The user the broker verified the client as; anonymous clients have
    /// none
    Username,
    /// The client's IP address, from the PROXY header when there is one
    Address,
}

impl BanKind {
    /// Its name in the protocol and in storage.
    pub fn as_str(self) -> &'static str {
        match self {
            BanKind::ClientId => "clientid",
            BanKind::Username => "username",
            BanKind::Address => "address",
        }
    }

    pub(crate) fn from_name(name: &str) -> Option<BanKind> {
        [BanKind::ClientId, BanKind::Username, BanKind::Address]
            .into_iter()
            .find(|kind| kind.as_str() == name)
    }
}

/// One entry of `listBans`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Ban {
    #[serde(rename = "type")]
    pub kind: BanKind,
    /// An address range as its first address and length: `10.0.0.0/8`,
    /// `192.0.2.7`
    pub value: String,
    pub prefix: bool,
    pub reason: Option<String>,
    /// Unix seconds, when the ban was first added
    pub created_at: u64,
    /// Unix seconds; null for a ban that never expires
    pub expires_at: Option<u64>,
}
