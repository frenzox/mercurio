//! In-memory storage backend.
//!
//! This module provides a thread-safe in-memory implementation of all storage traits.
//! Suitable for development, testing, and single-node deployments where persistence
//! across restarts is not required.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    ops::Bound,
    sync::{Arc, RwLock},
};

use async_trait::async_trait;
use mercurio_core::{message::Message, topic::topic_matches_filter};

use crate::{
    BanStore, BatchStore, InflightMessage, Result, RetainedMessageStore, SessionKey, SessionState,
    StorageError, StoredBan, StoredWillMessage, TopicRange, Write,
};

/// A ban's kind, value and prefix flag.
type BanKey = (String, String, bool);

/// What the store holds for the clients.
#[derive(Debug, Default)]
struct Clients {
    sessions: HashMap<SessionKey, SessionState>,
    wills: HashMap<SessionKey, StoredWillMessage>,
    inflight: HashMap<SessionKey, HashMap<u16, InflightMessage>>,
    incoming_qos2: HashMap<SessionKey, HashSet<u16>>,
}

impl Clients {
    fn apply(&mut self, key: &SessionKey, write: &Write) {
        match write {
            Write::SaveSession(session) => {
                self.sessions.insert(key.clone(), session.clone());
            }
            Write::AddSubscription(subscription) => {
                if let Some(session) = self.sessions.get_mut(key) {
                    session
                        .subscriptions
                        .retain(|s| s.topic_filter != subscription.topic_filter);
                    session.subscriptions.push(subscription.clone());
                }
            }
            Write::RemoveSubscription(topic_filter) => {
                if let Some(session) = self.sessions.get_mut(key) {
                    session
                        .subscriptions
                        .retain(|s| s.topic_filter != *topic_filter);
                }
            }
            Write::SetExpiry {
                interval,
                expires_at,
            } => {
                if let Some(session) = self.sessions.get_mut(key) {
                    session.expiry_interval = Some(*interval);
                    session.expires_at = Some(*expires_at);
                }
            }
            Write::StoreWill(will) => {
                self.wills.insert(key.clone(), will.clone());
            }
            Write::DeleteWill => {
                self.wills.remove(key);
            }
            Write::StoreInflight(message) => {
                self.inflight
                    .entry(key.clone())
                    .or_default()
                    .insert(message.packet_id, message.clone());
            }
            Write::ReleaseInflight(packet_id) => {
                if let Some(message) = self
                    .inflight
                    .get_mut(key)
                    .and_then(|messages| messages.get_mut(packet_id))
                {
                    message.released = true;
                    message.payload = None;
                }
            }
            Write::RemoveInflight(packet_id) => {
                if let Some(messages) = self.inflight.get_mut(key) {
                    messages.remove(packet_id);
                    if messages.is_empty() {
                        self.inflight.remove(key);
                    }
                }
            }
            Write::StoreIncomingQos2(packet_id) => {
                self.incoming_qos2
                    .entry(key.clone())
                    .or_default()
                    .insert(*packet_id);
            }
            Write::RemoveIncomingQos2(packet_id) => {
                if let Some(ids) = self.incoming_qos2.get_mut(key) {
                    ids.remove(packet_id);
                    if ids.is_empty() {
                        self.incoming_qos2.remove(key);
                    }
                }
            }
            // Everything in memory is lost on restart anyway, so queues are
            // not copied
            Write::Enqueue(..) | Write::Dequeue(_) | Write::ClearQueue => {}
            Write::DeleteSession => {
                self.sessions.remove(key);
                self.wills.remove(key);
                self.inflight.remove(key);
                self.incoming_qos2.remove(key);
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
    async fn apply(&self, key: &SessionKey, writes: &[Write]) -> Result<()> {
        let mut clients = self.clients.write().map_err(poisoned)?;
        for write in writes {
            clients.apply(key, write);
        }
        Ok(())
    }

    async fn load_session(&self, key: &SessionKey) -> Result<Option<SessionState>> {
        self.read(|clients| clients.sessions.get(key).cloned())
    }

    async fn list_sessions(&self) -> Result<Vec<SessionKey>> {
        self.read(|clients| clients.sessions.keys().cloned().collect())
    }

    async fn load_will(&self, key: &SessionKey) -> Result<Option<StoredWillMessage>> {
        self.read(|clients| clients.wills.get(key).cloned())
    }

    async fn load_inflight(&self, key: &SessionKey) -> Result<Vec<InflightMessage>> {
        self.read(|clients| {
            clients
                .inflight
                .get(key)
                .map(|messages| messages.values().cloned().collect())
                .unwrap_or_default()
        })
    }

    async fn load_incoming_qos2(&self, key: &SessionKey) -> Result<Vec<u16>> {
        self.read(|clients| {
            clients
                .incoming_qos2
                .get(key)
                .map(|ids| ids.iter().copied().collect())
                .unwrap_or_default()
        })
    }

    async fn load_queue(&self, _key: &SessionKey) -> Result<Vec<(u64, Message)>> {
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
    async fn store_retained(&self, topic: &str, message: Option<Message>) -> Result<bool> {
        let mut retained = self.retained.write().map_err(poisoned)?;
        Ok(match message {
            Some(msg) => retained.insert(topic.to_string(), msg).is_some(),
            None => retained.remove(topic).is_some(),
        })
    }

    async fn get_retained_page(
        &self,
        topic_filter: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Message>> {
        let retained = self.retained.read().map_err(poisoned)?;
        let (start, end) = match TopicRange::of(topic_filter) {
            TopicRange::Exact(topic) => {
                let end = Bound::Included(topic.clone());
                (topic, end)
            }
            TopicRange::From { start, end } => {
                (start, end.map_or(Bound::Unbounded, Bound::Excluded))
            }
        };
        let start = match after {
            Some(after) if after >= start.as_str() => Bound::Excluded(after.to_string()),
            _ => Bound::Included(start),
        };
        // A range that starts past its end is empty; BTreeMap would panic
        if let (Bound::Excluded(after), Bound::Included(end) | Bound::Excluded(end)) =
            (&start, &end)
        {
            if after >= end {
                return Ok(Vec::new());
            }
        }
        Ok(retained
            .range((start, end))
            .filter(|(topic, message)| {
                !message.is_expired() && topic_matches_filter(topic, topic_filter)
            })
            .take(limit)
            .map(|(_, message)| Message {
                retain: true,
                ..message.clone()
            })
            .collect())
    }

    async fn remove_expired_retained(&self) -> Result<usize> {
        let mut retained = self.retained.write().map_err(poisoned)?;
        let before = retained.len();
        retained.retain(|_, message| !message.is_expired());
        Ok(before - retained.len())
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
