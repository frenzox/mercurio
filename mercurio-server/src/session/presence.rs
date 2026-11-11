//! Where a session's client is connected from, since when, and until when the
//! session outlives it: what the admin API shows of a session.

use std::{net::SocketAddr, sync::Arc};

use mercurio_core::{message::unix_millis, protocol::ProtocolVersion};
use mercurio_storage::StoredSubscription;
use tokio::time::Duration;

use super::Session;
use crate::sys::ClientCounters;

/// The session's current or last connection, for the admin API.
#[derive(Default)]
pub(super) struct Presence {
    pub(super) listener: Option<Arc<str>>,
    /// Unix seconds; None until a client connects in this run of the broker
    pub(super) connected_at: Option<u64>,
    /// Unix seconds
    pub(super) disconnected_at: Option<u64>,
    pub(super) keepalive: u16,
    /// How long the session outlives the connection; forever if None
    pub(super) session_expiry: Option<Duration>,
    /// Unix milliseconds, once disconnected
    pub(super) expires_at: Option<u64>,
    pub(super) counters: Arc<ClientCounters>,
}

/// A session's state at one moment, for the admin API.
pub(crate) struct SessionSnapshot {
    pub username: Option<String>,
    pub peer: Option<SocketAddr>,
    pub listener: Option<Arc<str>>,
    /// None for a session restored from storage whose client hasn't
    /// connected since, like the other connection details
    pub protocol_version: Option<ProtocolVersion>,
    pub keepalive: Option<u16>,
    pub clean_start: Option<bool>,
    pub connected_at: Option<u64>,
    pub disconnected_at: Option<u64>,
    pub session_expiry: Option<Duration>,
    pub expires_at: Option<u64>,
    pub subscriptions: Vec<StoredSubscription>,
    pub queued: usize,
    pub inflight: usize,
    pub will_pending: bool,
    pub counters: Arc<ClientCounters>,
}

impl Session {
    /// Records a new connection to the session and returns the counters
    /// of its traffic.
    pub(crate) async fn attach(
        &self,
        listener: Arc<str>,
        keepalive: u16,
        expiry: Option<Duration>,
    ) -> Arc<ClientCounters> {
        let counters = Arc::new(ClientCounters::default());
        self.shared.queue.set_counters(Arc::clone(&counters));
        self.shared.queue.set_connected(true);
        self.shared.state.lock().await.presence = Presence {
            listener: Some(listener),
            connected_at: Some(unix_millis() / 1000),
            disconnected_at: None,
            keepalive,
            session_expiry: expiry,
            expires_at: None,
            counters: Arc::clone(&counters),
        };
        counters
    }

    /// Records that the connection of `generation` ended, the session
    /// outliving it by `expiry`, unless a newer connection took over.
    pub(crate) async fn detach(&self, generation: u64, expiry: Option<Duration>) {
        let mut state = self.shared.state.lock().await;
        if self.generation() != generation {
            return;
        }
        self.shared.queue.set_connected(false);
        let now = unix_millis();
        state.presence.disconnected_at = Some(now / 1000);
        state.presence.session_expiry = expiry;
        state.presence.expires_at = expiry.map(|e| now + e.as_millis() as u64);
    }

    /// Records that a connection is starting the session to outlive it by
    /// `expiry`, so storing the session whole before the connection is
    /// attached keeps that expiry.
    pub(crate) async fn set_expiry(&self, expiry: Option<Duration>) {
        let presence = &mut self.shared.state.lock().await.presence;
        presence.session_expiry = expiry;
        presence.expires_at = None;
    }

    /// Records a session restored from storage as disconnected from now on.
    pub(crate) async fn mark_restored(&self, expiry: Option<Duration>, expires_at: Option<u64>) {
        let presence = &mut self.shared.state.lock().await.presence;
        presence.disconnected_at = Some(unix_millis() / 1000);
        presence.session_expiry = expiry;
        presence.expires_at = expires_at;
    }

    pub(crate) async fn snapshot(&self) -> SessionSnapshot {
        let state = self.shared.state.lock().await;
        let presence = &state.presence;
        let seen = presence.connected_at.is_some();
        SessionSnapshot {
            username: state.owner.username.clone(),
            peer: state.peer,
            listener: presence.listener.clone(),
            protocol_version: seen.then_some(state.client.protocol_version),
            keepalive: seen.then_some(presence.keepalive),
            clean_start: seen.then_some(state.client.clean_start),
            connected_at: presence
                .connected_at
                .filter(|_| presence.disconnected_at.is_none()),
            disconnected_at: presence.disconnected_at,
            session_expiry: presence.session_expiry,
            expires_at: presence.expires_at,
            subscriptions: state.subscriptions.values().cloned().collect(),
            queued: self.shared.queue.len(),
            inflight: state.inflight.len(),
            will_pending: state.will_message.is_some() || state.pending_will.is_some(),
            counters: Arc::clone(&presence.counters),
        }
    }
}
