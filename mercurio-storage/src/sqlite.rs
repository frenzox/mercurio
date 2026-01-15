//! SQLite storage backend.
//!
//! This module provides a persistent SQLite implementation of all storage traits.
//! Suitable for single-node deployments requiring persistence across restarts.

use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use bytes::Bytes;
use mercurio_core::{message::Message, qos::QoS};
use rusqlite::{params, Connection, OptionalExtension};

use crate::{
    InflightMessage, InflightStore, Result, RetainedMessageStore, SessionState, SessionStore,
    StorageError, StoredWillMessage, WillStore,
};

/// SQLite storage backend implementing all storage traits.
///
/// Uses a single SQLite connection protected by a mutex. For high-concurrency
/// scenarios, consider using a connection pool.
#[derive(Clone)]
pub struct SqliteStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteStore {
    /// Create a new SQLite store with the given database path.
    ///
    /// Creates the database file and tables if they don't exist.
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        let conn = Connection::open(path)?;
        let store = SqliteStore {
            conn: Arc::new(Mutex::new(conn)),
        };
        store.init_schema()?;
        Ok(store)
    }

    /// Create an in-memory SQLite store (useful for testing).
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let store = SqliteStore {
            conn: Arc::new(Mutex::new(conn)),
        };
        store.init_schema()?;
        Ok(store)
    }

    /// Initialize the database schema.
    fn init_schema(&self) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| StorageError::OperationFailed(e.to_string()))?;

        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS sessions (
                client_id TEXT PRIMARY KEY,
                subscriptions TEXT NOT NULL,
                clean_start INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS retained_messages (
                topic TEXT PRIMARY KEY,
                payload BLOB,
                qos INTEGER NOT NULL,
                dup INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS will_messages (
                client_id TEXT PRIMARY KEY,
                topic TEXT NOT NULL,
                payload BLOB NOT NULL,
                qos INTEGER NOT NULL,
                retain INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS inflight_messages (
                client_id TEXT NOT NULL,
                packet_id INTEGER NOT NULL,
                topic TEXT NOT NULL,
                payload BLOB,
                qos INTEGER NOT NULL,
                dup INTEGER NOT NULL,
                PRIMARY KEY (client_id, packet_id)
            );

            CREATE INDEX IF NOT EXISTS idx_inflight_client
                ON inflight_messages(client_id);
            ",
        )?;

        Ok(())
    }

    /// Execute a blocking operation on the tokio runtime.
    async fn blocking<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let conn = conn
                .lock()
                .map_err(|e| StorageError::OperationFailed(e.to_string()))?;
            f(&conn)
        })
        .await
        .map_err(|e| StorageError::OperationFailed(e.to_string()))?
    }
}

fn qos_to_int(qos: QoS) -> i32 {
    match qos {
        QoS::AtMostOnce => 0,
        QoS::AtLeastOnce => 1,
        QoS::ExactlyOnce => 2,
        QoS::Invalid => -1,
    }
}

fn int_to_qos(val: i32) -> QoS {
    match val {
        0 => QoS::AtMostOnce,
        1 => QoS::AtLeastOnce,
        2 => QoS::ExactlyOnce,
        _ => QoS::Invalid,
    }
}

#[async_trait]
impl SessionStore for SqliteStore {
    async fn save_session(&self, client_id: &str, session: &SessionState) -> Result<()> {
        let client_id = client_id.to_string();
        let subscriptions = session.subscriptions.join(",");
        let clean_start = session.clean_start;

        self.blocking(move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO sessions (client_id, subscriptions, clean_start)
                 VALUES (?1, ?2, ?3)",
                params![client_id, subscriptions, clean_start],
            )?;
            Ok(())
        })
        .await
    }

    async fn load_session(&self, client_id: &str) -> Result<Option<SessionState>> {
        let client_id = client_id.to_string();

        self.blocking(move |conn| {
            let mut stmt = conn
                .prepare("SELECT subscriptions, clean_start FROM sessions WHERE client_id = ?1")?;

            let result = stmt
                .query_row(params![client_id.clone()], |row| {
                    let subs: String = row.get(0)?;
                    let clean_start: bool = row.get(1)?;
                    Ok((subs, clean_start))
                })
                .optional()?;

            Ok(result.map(|(subs, clean_start)| SessionState {
                client_id,
                subscriptions: if subs.is_empty() {
                    vec![]
                } else {
                    subs.split(',').map(|s| s.to_string()).collect()
                },
                clean_start,
            }))
        })
        .await
    }

    async fn delete_session(&self, client_id: &str) -> Result<()> {
        let client_id = client_id.to_string();

        self.blocking(move |conn| {
            conn.execute(
                "DELETE FROM sessions WHERE client_id = ?1",
                params![client_id],
            )?;
            Ok(())
        })
        .await
    }

    async fn list_sessions(&self) -> Result<Vec<String>> {
        self.blocking(move |conn| {
            let mut stmt = conn.prepare("SELECT client_id FROM sessions")?;
            let rows = stmt.query_map([], |row| row.get(0))?;

            let mut sessions = Vec::new();
            for row in rows {
                sessions.push(row?);
            }
            Ok(sessions)
        })
        .await
    }
}

#[async_trait]
impl RetainedMessageStore for SqliteStore {
    async fn store_retained(&self, topic: &str, message: Option<Message>) -> Result<()> {
        let topic = topic.to_string();

        self.blocking(move |conn| {
            match message {
                Some(msg) => {
                    let payload = msg.payload.as_ref().map(|b| b.to_vec());
                    conn.execute(
                        "INSERT OR REPLACE INTO retained_messages (topic, payload, qos, dup)
                         VALUES (?1, ?2, ?3, ?4)",
                        params![topic, payload, qos_to_int(msg.qos), msg.dup],
                    )?;
                }
                None => {
                    conn.execute(
                        "DELETE FROM retained_messages WHERE topic = ?1",
                        params![topic],
                    )?;
                }
            }
            Ok(())
        })
        .await
    }

    async fn get_retained(&self, topic_filter: &str) -> Result<Vec<Message>> {
        let filter = topic_filter.to_string();

        self.blocking(move |conn| {
            let mut stmt =
                conn.prepare("SELECT topic, payload, qos, dup FROM retained_messages")?;
            let rows = stmt.query_map([], |row| {
                let topic: String = row.get(0)?;
                let payload: Option<Vec<u8>> = row.get(1)?;
                let qos: i32 = row.get(2)?;
                let dup: bool = row.get(3)?;
                Ok((topic, payload, qos, dup))
            })?;

            let filter_parts: Vec<&str> = filter.split('/').collect();
            let mut messages = Vec::new();

            for row in rows {
                let (topic, payload, qos, dup) = row?;
                if topic_matches_filter(&topic, &filter_parts) {
                    messages.push(Message {
                        packet_id: None,
                        topic: Arc::from(topic.as_str()),
                        dup,
                        qos: int_to_qos(qos),
                        retain: true,
                        payload: payload.map(Bytes::from),
                    });
                }
            }

            Ok(messages)
        })
        .await
    }

    async fn clear_all_retained(&self) -> Result<()> {
        self.blocking(move |conn| {
            conn.execute("DELETE FROM retained_messages", [])?;
            Ok(())
        })
        .await
    }
}

#[async_trait]
impl WillStore for SqliteStore {
    async fn store_will(&self, client_id: &str, will: &StoredWillMessage) -> Result<()> {
        let client_id = client_id.to_string();
        let topic = will.topic.clone();
        let payload = will.payload.to_vec();
        let qos = qos_to_int(will.qos);
        let retain = will.retain;

        self.blocking(move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO will_messages (client_id, topic, payload, qos, retain)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![client_id, topic, payload, qos, retain],
            )?;
            Ok(())
        })
        .await
    }

    async fn get_will(&self, client_id: &str) -> Result<Option<StoredWillMessage>> {
        let client_id = client_id.to_string();

        self.blocking(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT topic, payload, qos, retain FROM will_messages WHERE client_id = ?1",
            )?;

            let result = stmt
                .query_row(params![client_id], |row| {
                    let topic: String = row.get(0)?;
                    let payload: Vec<u8> = row.get(1)?;
                    let qos: i32 = row.get(2)?;
                    let retain: bool = row.get(3)?;
                    Ok((topic, payload, qos, retain))
                })
                .optional()?;

            Ok(
                result.map(|(topic, payload, qos, retain)| StoredWillMessage {
                    topic,
                    payload: Bytes::from(payload),
                    qos: int_to_qos(qos),
                    retain,
                }),
            )
        })
        .await
    }

    async fn delete_will(&self, client_id: &str) -> Result<()> {
        let client_id = client_id.to_string();

        self.blocking(move |conn| {
            conn.execute(
                "DELETE FROM will_messages WHERE client_id = ?1",
                params![client_id],
            )?;
            Ok(())
        })
        .await
    }
}

#[async_trait]
impl InflightStore for SqliteStore {
    async fn store_inflight(
        &self,
        client_id: &str,
        packet_id: u16,
        message: &InflightMessage,
    ) -> Result<()> {
        let client_id = client_id.to_string();
        let topic = message.topic.clone();
        let payload = message.payload.as_ref().map(|b| b.to_vec());
        let qos = qos_to_int(message.qos);
        let dup = message.dup;

        self.blocking(move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO inflight_messages
                 (client_id, packet_id, topic, payload, qos, dup)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![client_id, packet_id, topic, payload, qos, dup],
            )?;
            Ok(())
        })
        .await
    }

    async fn get_inflight(
        &self,
        client_id: &str,
        packet_id: u16,
    ) -> Result<Option<InflightMessage>> {
        let client_id = client_id.to_string();

        self.blocking(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT topic, payload, qos, dup FROM inflight_messages
                 WHERE client_id = ?1 AND packet_id = ?2",
            )?;

            let result = stmt
                .query_row(params![client_id, packet_id], |row| {
                    let topic: String = row.get(0)?;
                    let payload: Option<Vec<u8>> = row.get(1)?;
                    let qos: i32 = row.get(2)?;
                    let dup: bool = row.get(3)?;
                    Ok((topic, payload, qos, dup))
                })
                .optional()?;

            Ok(result.map(|(topic, payload, qos, dup)| InflightMessage {
                packet_id,
                topic,
                payload: payload.map(Bytes::from),
                qos: int_to_qos(qos),
                dup,
            }))
        })
        .await
    }

    async fn remove_inflight(&self, client_id: &str, packet_id: u16) -> Result<()> {
        let client_id = client_id.to_string();

        self.blocking(move |conn| {
            conn.execute(
                "DELETE FROM inflight_messages WHERE client_id = ?1 AND packet_id = ?2",
                params![client_id, packet_id],
            )?;
            Ok(())
        })
        .await
    }

    async fn get_all_inflight(&self, client_id: &str) -> Result<Vec<InflightMessage>> {
        let client_id = client_id.to_string();

        self.blocking(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT packet_id, topic, payload, qos, dup FROM inflight_messages
                 WHERE client_id = ?1",
            )?;

            let rows = stmt.query_map(params![client_id], |row| {
                let packet_id: u16 = row.get(0)?;
                let topic: String = row.get(1)?;
                let payload: Option<Vec<u8>> = row.get(2)?;
                let qos: i32 = row.get(3)?;
                let dup: bool = row.get(4)?;
                Ok(InflightMessage {
                    packet_id,
                    topic,
                    payload: payload.map(Bytes::from),
                    qos: int_to_qos(qos),
                    dup,
                })
            })?;

            let mut messages = Vec::new();
            for row in rows {
                messages.push(row?);
            }
            Ok(messages)
        })
        .await
    }
}

/// Check if a topic matches a filter (with wildcard support).
fn topic_matches_filter(topic: &str, filter_parts: &[&str]) -> bool {
    let topic_parts: Vec<&str> = topic.split('/').collect();
    let mut topic_idx = 0;
    let mut filter_idx = 0;

    while filter_idx < filter_parts.len() {
        let filter_part = filter_parts[filter_idx];

        if filter_part == "#" {
            return true;
        }

        if topic_idx >= topic_parts.len() {
            return false;
        }

        if filter_part != "+" && filter_part != topic_parts[topic_idx] {
            return false;
        }
        topic_idx += 1;
        filter_idx += 1;
    }

    topic_idx == topic_parts.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_message(topic: &str, payload: &str) -> Message {
        Message {
            packet_id: None,
            topic: Arc::from(topic),
            dup: false,
            qos: QoS::AtMostOnce,
            retain: false,
            payload: Some(Bytes::from(payload.to_string())),
        }
    }

    #[tokio::test]
    async fn test_session_store() {
        let store = SqliteStore::in_memory().unwrap();

        let session = SessionState {
            client_id: "client1".to_string(),
            subscriptions: vec!["topic/a".to_string(), "topic/b".to_string()],
            clean_start: false,
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
        let store = SqliteStore::in_memory().unwrap();

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
        let store = SqliteStore::in_memory().unwrap();

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
        let store = SqliteStore::in_memory().unwrap();

        let msg = InflightMessage {
            packet_id: 1,
            topic: "test/topic".to_string(),
            payload: Some(Bytes::from("data")),
            qos: QoS::AtLeastOnce,
            dup: false,
        };

        store.store_inflight("client1", 1, &msg).await.unwrap();

        let loaded = store.get_inflight("client1", 1).await.unwrap();
        assert!(loaded.is_some());

        let all = store.get_all_inflight("client1").await.unwrap();
        assert_eq!(all.len(), 1);

        store.remove_inflight("client1", 1).await.unwrap();
        let loaded = store.get_inflight("client1", 1).await.unwrap();
        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn test_clear_all_retained() {
        let store = SqliteStore::in_memory().unwrap();

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

    #[tokio::test]
    async fn test_persistence_across_instances() {
        // Test that data persists when we create a new store instance
        // pointing to the same file
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join("mercurio_test.db");

        // Clean up any existing test database
        let _ = std::fs::remove_file(&db_path);

        // Create first store and add data
        {
            let store = SqliteStore::new(&db_path).unwrap();
            let session = SessionState {
                client_id: "persistent_client".to_string(),
                subscriptions: vec!["topic/test".to_string()],
                clean_start: false,
            };
            store
                .save_session("persistent_client", &session)
                .await
                .unwrap();
        }

        // Create second store instance and verify data persists
        {
            let store = SqliteStore::new(&db_path).unwrap();
            let loaded = store.load_session("persistent_client").await.unwrap();
            assert!(loaded.is_some());
            assert_eq!(loaded.unwrap().client_id, "persistent_client");
        }

        // Clean up
        let _ = std::fs::remove_file(&db_path);
    }
}
