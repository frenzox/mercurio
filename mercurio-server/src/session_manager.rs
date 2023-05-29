use std::{collections::HashMap, sync::Arc};

use tokio::sync::Mutex;

use mercurio_core::Result;
use mercurio_packets::connect::ConnectPacket;

use crate::{
    connection::Connection,
    session::{Session, SessionDropGuard},
};

pub(crate) struct SessionManagerDropGuard {
    session_manager: SessionManager,
}

#[derive(Clone)]
pub(crate) struct SessionManager {
    shared: Arc<Shared>,
}

struct Shared {
    state: Mutex<State>,
}

struct State {
    sessions: HashMap<String, SessionDropGuard>,
}

impl SessionManagerDropGuard {
    pub(crate) fn new() -> SessionManagerDropGuard {
        SessionManagerDropGuard {
            session_manager: SessionManager::new(),
        }
    }

    pub(crate) fn session_manager(&self) -> SessionManager {
        self.session_manager.clone()
    }
}

impl SessionManager {
    pub(crate) fn new() -> SessionManager {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                sessions: HashMap::new(),
            }),
        });

        SessionManager { shared }
    }

    pub(crate) async fn start_session(
        &mut self,
        connection: &mut Connection,
        connect_packet: ConnectPacket,
    ) -> Result<Session> {
        let mut manager = self.shared.state.lock().await;
        let mut resume = true;

        if connect_packet.flags.clean_start {
            resume = false;
            manager.sessions.remove(&connect_packet.payload.client_id);
        }

        let mut session = match manager
            .sessions
            .entry(connect_packet.payload.client_id.clone())
        {
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
        Ok(session)
    }
}
