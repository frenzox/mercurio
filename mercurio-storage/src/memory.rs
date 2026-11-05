//! In-memory storage backend.
//!
//! This module provides a thread-safe in-memory implementation of all storage traits.
//! Suitable for development, testing, and single-node deployments where persistence
//! across restarts is not required.

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use async_trait::async_trait;
use mercurio_core::{message::Message, topic::topic_matches_filter};

use crate::{
    InflightMessage, InflightStore, Result, RetainedMessageStore, SessionState, SessionStore,
    StorageError, StoredWillMessage, WillStore,
};

/// In-memory storage backend implementing all storage traits.
///
/// Uses `RwLock` for thread-safe concurrent access. All data is lost on restart.
#[derive(Debug, Clone, Default)]
pub struct MemoryStore {
    sessions: Arc<RwLock<HashMap<String, SessionState>>>,
    retained: Arc<RwLock<HashMap<String, Message>>>,
    wills: Arc<RwLock<HashMap<String, StoredWillMessage>>>,
    inflight: Arc<RwLock<HashMap<String, HashMap<u16, InflightMessage>>>>,
}

impl MemoryStore {
    /// Create a new in-memory store.
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl SessionStore for MemoryStore {
    async fn save_session(&self, client_id: &str, session: &SessionState) -> Result<()> {
        let mut sessions = self
            .sessions
            .write()
            .map_err(|e| StorageError::OperationFailed(e.to_string()))?;
        sessions.insert(client_id.to_string(), session.clone());
        Ok(())
    }

    async fn load_session(&self, client_id: &str) -> Result<Option<SessionState>> {
        let sessions = self
            .sessions
            .read()
            .map_err(|e| StorageError::OperationFailed(e.to_string()))?;
        Ok(sessions.get(client_id).cloned())
    }

    async fn delete_session(&self, client_id: &str) -> Result<()> {
        let mut sessions = self
            .sessions
            .write()
            .map_err(|e| StorageError::OperationFailed(e.to_string()))?;
        sessions.remove(client_id);
        Ok(())
    }

    async fn list_sessions(&self) -> Result<Vec<String>> {
        let sessions = self
            .sessions
            .read()
            .map_err(|e| StorageError::OperationFailed(e.to_string()))?;
        Ok(sessions.keys().cloned().collect())
    }
}

#[async_trait]
impl RetainedMessageStore for MemoryStore {
    async fn store_retained(&self, topic: &str, message: Option<Message>) -> Result<()> {
        let mut retained = self
            .retained
            .write()
            .map_err(|e| StorageError::OperationFailed(e.to_string()))?;

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
        let retained = self
            .retained
            .read()
            .map_err(|e| StorageError::OperationFailed(e.to_string()))?;

        let messages: Vec<Message> = retained
            .iter()
            // ponytail: expired messages are skipped, not removed, so they stay in
            // memory until their topic gets a new retained message
            .filter(|(topic, msg)| topic_matches_filter(topic, topic_filter) && !msg.is_expired())
            .map(|(_, msg)| {
                let mut msg = msg.clone();
                msg.retain = true; // Mark as retained when delivering
                msg
            })
            .collect();

        Ok(messages)
    }

    async fn clear_all_retained(&self) -> Result<()> {
        let mut retained = self
            .retained
            .write()
            .map_err(|e| StorageError::OperationFailed(e.to_string()))?;
        retained.clear();
        Ok(())
    }
}

#[async_trait]
impl WillStore for MemoryStore {
    async fn store_will(&self, client_id: &str, will: &StoredWillMessage) -> Result<()> {
        let mut wills = self
            .wills
            .write()
            .map_err(|e| StorageError::OperationFailed(e.to_string()))?;
        wills.insert(client_id.to_string(), will.clone());
        Ok(())
    }

    async fn get_will(&self, client_id: &str) -> Result<Option<StoredWillMessage>> {
        let wills = self
            .wills
            .read()
            .map_err(|e| StorageError::OperationFailed(e.to_string()))?;
        Ok(wills.get(client_id).cloned())
    }

    async fn delete_will(&self, client_id: &str) -> Result<()> {
        let mut wills = self
            .wills
            .write()
            .map_err(|e| StorageError::OperationFailed(e.to_string()))?;
        wills.remove(client_id);
        Ok(())
    }
}

#[async_trait]
impl InflightStore for MemoryStore {
    async fn store_inflight(
        &self,
        client_id: &str,
        packet_id: u16,
        message: &InflightMessage,
    ) -> Result<()> {
        let mut inflight = self
            .inflight
            .write()
            .map_err(|e| StorageError::OperationFailed(e.to_string()))?;

        inflight
            .entry(client_id.to_string())
            .or_default()
            .insert(packet_id, message.clone());
        Ok(())
    }

    async fn get_inflight(
        &self,
        client_id: &str,
        packet_id: u16,
    ) -> Result<Option<InflightMessage>> {
        let inflight = self
            .inflight
            .read()
            .map_err(|e| StorageError::OperationFailed(e.to_string()))?;

        Ok(inflight
            .get(client_id)
            .and_then(|msgs| msgs.get(&packet_id).cloned()))
    }

    async fn remove_inflight(&self, client_id: &str, packet_id: u16) -> Result<()> {
        let mut inflight = self
            .inflight
            .write()
            .map_err(|e| StorageError::OperationFailed(e.to_string()))?;

        if let Some(msgs) = inflight.get_mut(client_id) {
            msgs.remove(&packet_id);
        }
        Ok(())
    }

    async fn release_inflight(&self, client_id: &str, packet_id: u16) -> Result<()> {
        let mut inflight = self
            .inflight
            .write()
            .map_err(|e| StorageError::OperationFailed(e.to_string()))?;

        if let Some(message) = inflight
            .get_mut(client_id)
            .and_then(|msgs| msgs.get_mut(&packet_id))
        {
            message.released = true;
            message.payload = None;
        }
        Ok(())
    }

    async fn get_all_inflight(&self, client_id: &str) -> Result<Vec<InflightMessage>> {
        let inflight = self
            .inflight
            .read()
            .map_err(|e| StorageError::OperationFailed(e.to_string()))?;

        Ok(inflight
            .get(client_id)
            .map(|msgs| msgs.values().cloned().collect())
            .unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StoredSubscription;
    use bytes::Bytes;
    use mercurio_core::qos::QoS;
    use std::sync::Arc;

    fn create_test_message(topic: &str, payload: &str) -> Message {
        Message {
            packet_id: None,
            topic: Arc::from(topic),
            dup: false,
            qos: QoS::AtMostOnce,
            retain: false,
            payload: Some(Bytes::from(payload.to_string())),
            properties: Default::default(),
            expires_at: None,
            subscription_identifier: None,
        }
    }

    #[tokio::test]
    async fn test_session_store() {
        let store = MemoryStore::new();

        let session = SessionState {
            client_id: "client1".to_string(),
            subscriptions: vec![
                StoredSubscription::new("topic/a", QoS::AtMostOnce),
                StoredSubscription::new("topic/b", QoS::AtLeastOnce),
            ],
            clean_start: false,
            expiry_interval: None,
            expires_at: None,
        };

        // Save and load
        store.save_session("client1", &session).await.unwrap();
        let loaded = store.load_session("client1").await.unwrap();
        assert!(loaded.is_some());
        assert_eq!(loaded.unwrap().subscriptions.len(), 2);

        // List sessions
        let sessions = store.list_sessions().await.unwrap();
        assert_eq!(sessions.len(), 1);

        // Delete
        store.delete_session("client1").await.unwrap();
        let loaded = store.load_session("client1").await.unwrap();
        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn test_retained_message_store() {
        let store = MemoryStore::new();

        let msg = create_test_message("sensors/temp", "22.5");
        store
            .store_retained("sensors/temp", Some(msg))
            .await
            .unwrap();

        // Exact match
        let retained = store.get_retained("sensors/temp").await.unwrap();
        assert_eq!(retained.len(), 1);

        // Wildcard match
        let retained = store.get_retained("sensors/+").await.unwrap();
        assert_eq!(retained.len(), 1);

        // Clear specific
        store.store_retained("sensors/temp", None).await.unwrap();
        let retained = store.get_retained("sensors/temp").await.unwrap();
        assert!(retained.is_empty());
    }

    #[tokio::test]
    async fn test_will_store() {
        let store = MemoryStore::new();

        let will = StoredWillMessage {
            topic: "client/status".to_string(),
            payload: Bytes::from("offline"),
            qos: QoS::AtLeastOnce,
            retain: true,
        };

        store.store_will("client1", &will).await.unwrap();
        let loaded = store.get_will("client1").await.unwrap();
        assert!(loaded.is_some());
        assert_eq!(loaded.unwrap().topic, "client/status");

        store.delete_will("client1").await.unwrap();
        let loaded = store.get_will("client1").await.unwrap();
        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn test_inflight_store() {
        let store = MemoryStore::new();

        let msg = InflightMessage {
            packet_id: 1,
            topic: "test/topic".to_string(),
            payload: Some(Bytes::from("data")),
            qos: QoS::ExactlyOnce,
            dup: false,
            sequence: 7,
            released: false,
            properties: Default::default(),
            subscription_identifier: None,
        };

        store.store_inflight("client1", 1, &msg).await.unwrap();

        let loaded = store.get_inflight("client1", 1).await.unwrap().unwrap();
        assert_eq!(loaded.sequence, 7);
        assert!(!loaded.released);

        store.release_inflight("client1", 1).await.unwrap();
        let all = store.get_all_inflight("client1").await.unwrap();
        assert_eq!(all.len(), 1);
        assert!(all[0].released);
        assert!(all[0].payload.is_none());
        assert_eq!(all[0].sequence, 7);

        store.remove_inflight("client1", 1).await.unwrap();
        let loaded = store.get_inflight("client1", 1).await.unwrap();
        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn test_clear_all_retained() {
        let store = MemoryStore::new();

        store
            .store_retained("a", Some(create_test_message("a", "1")))
            .await
            .unwrap();
        store
            .store_retained("b", Some(create_test_message("b", "2")))
            .await
            .unwrap();

        let retained = store.get_retained("#").await.unwrap();
        assert_eq!(retained.len(), 2);

        store.clear_all_retained().await.unwrap();

        let retained = store.get_retained("#").await.unwrap();
        assert!(retained.is_empty());
    }
}
