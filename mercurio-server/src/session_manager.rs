use std::{collections::HashMap, sync::Arc};

use tokio::sync::Mutex;
use tracing::info;

use mercurio_core::Result;
use mercurio_packets::connect::ConnectPacket;
use mercurio_storage::{SessionState, SessionStore};

use crate::{
    connection::Connection,
    session::{Session, SessionDropGuard},
};

pub(crate) struct SessionManagerDropGuard<S: SessionStore> {
    session_manager: SessionManager<S>,
}

pub(crate) struct SessionManager<S: SessionStore> {
    shared: Arc<Shared>,
    storage: Arc<S>,
}

impl<S: SessionStore> Clone for SessionManager<S> {
    fn clone(&self) -> Self {
        SessionManager {
            shared: Arc::clone(&self.shared),
            storage: Arc::clone(&self.storage),
        }
    }
}

struct Shared {
    state: Mutex<State>,
}

struct State {
    sessions: HashMap<String, SessionDropGuard>,
}

impl<S: SessionStore> SessionManagerDropGuard<S> {
    pub(crate) fn new(storage: Arc<S>) -> SessionManagerDropGuard<S> {
        SessionManagerDropGuard {
            session_manager: SessionManager::new(storage),
        }
    }

    pub(crate) fn session_manager(&self) -> SessionManager<S> {
        self.session_manager.clone()
    }
}

/// Result of starting a session, includes subscriptions to restore if resuming.
pub(crate) struct SessionStartResult {
    pub session: Session,
    /// Topic filters to restore (re-subscribe to broker) if resuming a persisted session.
    pub subscriptions_to_restore: Vec<String>,
}

impl<S: SessionStore> SessionManager<S> {
    pub(crate) fn new(storage: Arc<S>) -> SessionManager<S> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                sessions: HashMap::new(),
            }),
        });

        SessionManager { shared, storage }
    }

    pub(crate) async fn start_session(
        &mut self,
        connection: &mut Connection,
        connect_packet: ConnectPacket,
    ) -> Result<SessionStartResult> {
        let client_id = &connect_packet.payload.client_id;
        let mut manager = self.shared.state.lock().await;
        let mut resume = false;
        let mut subscriptions_to_restore = Vec::new();

        if connect_packet.flags.clean_start {
            // Clean start: remove any existing session (in-memory and storage)
            manager.sessions.remove(client_id);
            let _ = self.storage.delete_session(client_id).await;
        } else {
            // Try to resume existing session
            // First check in-memory sessions
            if manager.sessions.contains_key(client_id) {
                resume = true;
            } else {
                // Check persistent storage
                if let Ok(Some(stored_session)) = self.storage.load_session(client_id).await {
                    info!(
                        "Restoring session for client `{}` with {} subscriptions",
                        client_id,
                        stored_session.subscriptions.len()
                    );
                    subscriptions_to_restore = stored_session.subscriptions;
                    resume = true;
                }
            }
        }

        let mut session = match manager.sessions.entry(client_id.clone()) {
            std::collections::hash_map::Entry::Occupied(e) => {
                let mut s = e.into_mut().session();
                s.set_connect_packet(connect_packet).await;
                s
            }
            std::collections::hash_map::Entry::Vacant(e) => {
                let new_session = SessionDropGuard::new(connect_packet);
                e.insert(new_session).session()
            }
        };

        session.begin(connection, resume).await?;

        // Save session state to storage (with empty subscriptions initially)
        let session_state = SessionState {
            client_id: session.get_client_id().await,
            subscriptions: subscriptions_to_restore.clone(),
            clean_start: false,
        };
        let _ = self
            .storage
            .save_session(&session_state.client_id, &session_state)
            .await;

        Ok(SessionStartResult {
            session,
            subscriptions_to_restore,
        })
    }

    /// Update persisted session with current subscriptions.
    pub(crate) async fn save_subscriptions(
        &self,
        client_id: &str,
        subscriptions: Vec<String>,
    ) -> Result<()> {
        let session_state = SessionState {
            client_id: client_id.to_string(),
            subscriptions,
            clean_start: false,
        };
        self.storage
            .save_session(client_id, &session_state)
            .await
            .map_err(|e| mercurio_core::error::Error::Storage(e.to_string()))
    }
}
