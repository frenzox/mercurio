//! What the admin API returns, serialized with the field names of the
//! `$CONTROL/broker/v1` protocol.

use std::collections::BTreeMap;

use serde::Serialize;

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
