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
