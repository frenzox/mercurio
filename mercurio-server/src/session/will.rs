//! A client's will: what its CONNECT asked for, and the copy left waiting out
//! its delay once the connection is gone.

use bytes::Bytes;
use mercurio_core::{message::MessageProperties, qos::QoS};
use mercurio_packets::connect::ConnectPacket;
use mercurio_storage::StoredWillMessage;
use tokio::{task::AbortHandle, time::Duration};

use super::Session;

/// Will message to be published on abnormal client disconnect.
#[derive(Debug, Clone)]
pub struct WillMessage {
    pub topic: String,
    pub payload: Bytes,
    pub qos: QoS,
    pub retain: bool,
    /// How long to wait after the connection closes before publishing
    pub delay: Duration,
    pub properties: MessageProperties,
}

impl WillMessage {
    /// Extract will message from a ConnectPacket if it has one.
    pub(super) fn from_connect_packet(packet: &ConnectPacket) -> Option<Self> {
        let will = packet.payload.will.as_ref()?;
        let will_properties = will.properties.as_ref();
        let delay = will_properties
            .and_then(|p| p.will_delay_interval.as_ref())
            .map_or(0, |d| d.value);
        let properties = will_properties
            .map(|p| MessageProperties {
                payload_format_indicator: p.payload_format_indicator.clone(),
                message_expiry_interval: p.message_expiry_interval.clone(),
                content_type: p.content_type.clone(),
                response_topic: p.response_topic.clone(),
                correlation_data: p.correlation_data.clone(),
                user_property: p.user_property.clone(),
            })
            .unwrap_or_default();

        Some(WillMessage {
            topic: will.topic.clone(),
            payload: will.payload.clone(),
            qos: will.qos,
            retain: will.retain,
            delay: Duration::from_secs(delay.into()),
            properties,
        })
    }
}

impl From<WillMessage> for StoredWillMessage {
    fn from(will: WillMessage) -> Self {
        StoredWillMessage {
            topic: will.topic,
            payload: will.payload,
            qos: will.qos,
            retain: will.retain,
            delay_secs: will.delay.as_secs().try_into().unwrap_or(u32::MAX),
            properties: will.properties,
        }
    }
}

impl From<StoredWillMessage> for WillMessage {
    fn from(stored: StoredWillMessage) -> Self {
        WillMessage {
            topic: stored.topic,
            payload: stored.payload,
            qos: stored.qos,
            retain: stored.retain,
            delay: Duration::from_secs(stored.delay_secs.into()),
            properties: stored.properties,
        }
    }
}

impl Session {
    /// Clear the will message. Called on normal DISCONNECT.
    pub(crate) async fn clear_will(&self) {
        let mut session = self.shared.state.lock().await;
        session.will_message = None;
    }

    /// Take the will message for publishing. Called on abnormal disconnect.
    /// Returns None if no will was set or it was already cleared.
    pub(crate) async fn take_will(&self) -> Option<WillMessage> {
        let mut session = self.shared.state.lock().await;
        session.will_message.take()
    }

    /// Drops the will an earlier connection left waiting for its delay, as
    /// its client is back.
    pub(crate) async fn drop_pending_will(&self) {
        if let Some((will, _, _)) = self.shared.state.lock().await.pending_will.take() {
            will.abort();
        }
    }

    /// Records the task holding `will`, of the connection of `generation`,
    /// until its delay passes, aborting it if a newer connection has
    /// already taken over.
    pub(crate) async fn set_pending_will(
        &self,
        generation: u64,
        task: AbortHandle,
        will: WillMessage,
    ) {
        let mut state = self.shared.state.lock().await;
        if self.generation() == generation {
            state.pending_will = Some((task, will, generation));
        } else {
            task.abort();
        }
    }

    /// Called by the pending will once its delay passed: whether it is still
    /// the session's to publish. From then on no takeover aborts it, so the
    /// will is never cut off half published.
    pub(crate) async fn claim_pending_will(&self, generation: u64) -> bool {
        let mut state = self.shared.state.lock().await;
        if state.pending_will.as_ref().map(|(_, _, g)| *g) != Some(generation) {
            return false;
        }
        state.pending_will = None;
        true
    }

    /// Get a clone of the will message without removing it.
    pub(crate) async fn get_will(&self) -> Option<WillMessage> {
        let session = self.shared.state.lock().await;
        session.will_message.clone()
    }
}
