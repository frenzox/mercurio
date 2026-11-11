//! In-memory storage backend.
//!
//! This module provides a thread-safe in-memory implementation of all storage traits.
//! Suitable for development, testing, and single-node deployments where persistence
//! across restarts is not required.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{Arc, RwLock},
};

use async_trait::async_trait;
use mercurio_core::{message::Message, topic::topic_matches_filter};

use crate::{
    BanStore, BatchStore, InflightMessage, Result, RetainedMessageStore, SessionState,
    StorageError, StoredBan, StoredWillMessage, Write,
};

/// A ban's kind, value and prefix flag.
type BanKey = (String, String, bool);

/// What the store holds for the clients.
#[derive(Debug, Default)]
struct Clients {
    sessions: HashMap<String, SessionState>,
    wills: HashMap<String, StoredWillMessage>,
    inflight: HashMap<String, HashMap<u16, InflightMessage>>,
    incoming_qos2: HashMap<String, HashSet<u16>>,
}

impl Clients {
    fn apply(&mut self, client_id: &str, write: &Write) {
        match write {
            Write::SaveSession(session) => {
                self.sessions.insert(client_id.to_string(), session.clone());
            }
            Write::AddSubscription(subscription) => {
                if let Some(session) = self.sessions.get_mut(client_id) {
                    session
                        .subscriptions
                        .retain(|s| s.topic_filter != subscription.topic_filter);
                    session.subscriptions.push(subscription.clone());
                }
            }
            Write::RemoveSubscription(topic_filter) => {
                if let Some(session) = self.sessions.get_mut(client_id) {
                    session
                        .subscriptions
                        .retain(|s| s.topic_filter != *topic_filter);
                }
            }
            Write::SetExpiry {
                interval,
                expires_at,
            } => {
                if let Some(session) = self.sessions.get_mut(client_id) {
                    session.expiry_interval = Some(*interval);
                    session.expires_at = Some(*expires_at);
                }
            }
            Write::StoreWill(will) => {
                self.wills.insert(client_id.to_string(), will.clone());
            }
            Write::DeleteWill => {
                self.wills.remove(client_id);
            }
            Write::StoreInflight(message) => {
                self.inflight
                    .entry(client_id.to_string())
                    .or_default()
                    .insert(message.packet_id, message.clone());
            }
            Write::ReleaseInflight(packet_id) => {
                if let Some(message) = self
                    .inflight
                    .get_mut(client_id)
                    .and_then(|messages| messages.get_mut(packet_id))
                {
                    message.released = true;
                    message.payload = None;
                }
            }
            Write::RemoveInflight(packet_id) => {
                if let Some(messages) = self.inflight.get_mut(client_id) {
                    messages.remove(packet_id);
                    if messages.is_empty() {
                        self.inflight.remove(client_id);
                    }
                }
            }
            Write::StoreIncomingQos2(packet_id) => {
                self.incoming_qos2
                    .entry(client_id.to_string())
                    .or_default()
                    .insert(*packet_id);
            }
            Write::RemoveIncomingQos2(packet_id) => {
                if let Some(ids) = self.incoming_qos2.get_mut(client_id) {
                    ids.remove(packet_id);
                    if ids.is_empty() {
                        self.incoming_qos2.remove(client_id);
                    }
                }
            }
            // Everything in memory is lost on restart anyway, so queues are
            // not copied
            Write::Enqueue(..) | Write::Dequeue(_) | Write::ClearQueue => {}
            Write::DeleteSession => {
                self.sessions.remove(client_id);
                self.wills.remove(client_id);
                self.inflight.remove(client_id);
                self.incoming_qos2.remove(client_id);
            }
        }
    }
}

/// In-memory storage backend implementing all storage traits.
///
/// Uses `RwLock` for thread-safe concurrent access. All data is lost on restart.
#[derive(Debug, Clone, Default)]
pub struct MemoryStore {
    clients: Arc<RwLock<Clients>>,
    retained: Arc<RwLock<BTreeMap<String, Message>>>,
    bans: Arc<RwLock<HashMap<BanKey, StoredBan>>>,
}

fn poisoned(error: impl std::fmt::Display) -> StorageError {
    StorageError::OperationFailed(error.to_string())
}

impl MemoryStore {
    /// Create a new in-memory store.
    pub fn new() -> Self {
        Self::default()
    }

    fn read<T>(&self, f: impl FnOnce(&Clients) -> T) -> Result<T> {
        Ok(f(&*self.clients.read().map_err(poisoned)?))
    }
}

#[async_trait]
impl BatchStore for MemoryStore {
    fn backend_name(&self) -> &'static str {
        "memory"
    }

    fn ban_store(self: Arc<Self>) -> Option<Arc<dyn BanStore>> {
        Some(self)
    }

    fn is_durable(&self) -> bool {
        false
    }

    /// Applies the batch under one lock, so readers see all of it or none.
    async fn apply(&self, client_id: &str, writes: &[Write]) -> Result<()> {
        let mut clients = self.clients.write().map_err(poisoned)?;
        for write in writes {
            clients.apply(client_id, write);
        }
        Ok(())
    }

    async fn load_session(&self, client_id: &str) -> Result<Option<SessionState>> {
        self.read(|clients| clients.sessions.get(client_id).cloned())
    }

    async fn list_sessions(&self) -> Result<Vec<String>> {
        self.read(|clients| clients.sessions.keys().cloned().collect())
    }

    async fn load_will(&self, client_id: &str) -> Result<Option<StoredWillMessage>> {
        self.read(|clients| clients.wills.get(client_id).cloned())
    }

    async fn load_inflight(&self, client_id: &str) -> Result<Vec<InflightMessage>> {
        self.read(|clients| {
            clients
                .inflight
                .get(client_id)
                .map(|messages| messages.values().cloned().collect())
                .unwrap_or_default()
        })
    }

    async fn load_incoming_qos2(&self, client_id: &str) -> Result<Vec<u16>> {
        self.read(|clients| {
            clients
                .incoming_qos2
                .get(client_id)
                .map(|ids| ids.iter().copied().collect())
                .unwrap_or_default()
        })
    }

    async fn load_queue(&self, _client_id: &str) -> Result<Vec<(u64, Message)>> {
        Ok(Vec::new())
    }
}

#[async_trait]
impl BanStore for MemoryStore {
    async fn load_bans(&self) -> Result<Vec<StoredBan>> {
        let bans = self.bans.read().map_err(poisoned)?;
        Ok(bans.values().cloned().collect())
    }

    async fn store_ban(&self, ban: &StoredBan) -> Result<()> {
        let mut bans = self.bans.write().map_err(poisoned)?;
        bans.insert(
            (ban.kind.clone(), ban.value.clone(), ban.prefix),
            ban.clone(),
        );
        Ok(())
    }

    async fn delete_ban(&self, kind: &str, value: &str, prefix: bool) -> Result<()> {
        let mut bans = self.bans.write().map_err(poisoned)?;
        bans.remove(&(kind.to_string(), value.to_string(), prefix));
        Ok(())
    }
}

#[async_trait]
impl RetainedMessageStore for MemoryStore {
    async fn store_retained(&self, topic: &str, message: Option<Message>) -> Result<()> {
        let mut retained = self.retained.write().map_err(poisoned)?;
        match message {
            Some(msg) => {
                retained.insert(topic.to_string(), msg);
            }
            None => {
                retained.remove(topic);
            }
        }
        Ok(())
    }

    async fn get_retained(&self, topic_filter: &str) -> Result<Vec<Message>> {
        let mut retained = self.retained.write().map_err(poisoned)?;

        // Expired messages are dropped for good, not just skipped
        retained.retain(|_, msg| !msg.is_expired());

        let messages: Vec<Message> = retained
            .iter()
            .filter(|(topic, _)| topic_matches_filter(topic, topic_filter))
            .map(|(_, msg)| {
                let mut msg = msg.clone();
                msg.retain = true; // Mark as retained when delivering
                msg
            })
            .collect();

        Ok(messages)
    }

    async fn clear_all_retained(&self) -> Result<()> {
        self.retained.write().map_err(poisoned)?.clear();
        Ok(())
    }

    async fn count_retained(&self) -> Result<usize> {
        Ok(self.retained.read().map_err(poisoned)?.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_conformance() {
        crate::testing::conformance(&MemoryStore::new()).await;
    }

    #[tokio::test]
    async fn test_bans() {
        let store = Arc::new(MemoryStore::new());
        crate::testing::bans(store.as_ref()).await;
        assert!(store.ban_store().is_some());
    }
}
