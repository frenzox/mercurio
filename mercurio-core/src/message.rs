//! MQTT message types for internal routing.

#[cfg(not(feature = "std"))]
use alloc::sync::Arc;
#[cfg(feature = "std")]
use std::sync::Arc;

use bytes::Bytes;

use crate::qos::QoS;

/// MQTT message for internal routing.
///
/// This struct is designed to be cheap to clone:
/// - `topic` uses `Arc<str>` for O(1) reference-counted cloning
/// - `payload` uses `Bytes` which is also reference-counted
#[derive(Clone, Debug)]
pub struct Message {
    pub packet_id: Option<u16>,
    /// Topic name - uses Arc<str> for cheap cloning when broadcasting to multiple subscribers
    pub topic: Arc<str>,
    pub dup: bool,
    pub qos: QoS,
    pub retain: bool,
    /// Payload data - Bytes is already reference-counted for cheap cloning
    pub payload: Option<Bytes>,
}
