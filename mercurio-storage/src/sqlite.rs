//! SQLite storage backend.
//!
//! This module provides a persistent SQLite implementation of all storage traits.
//! Suitable for single-node deployments requiring persistence across restarts.

use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use mercurio_core::{
    codec::{Decoder, Encoder},
    message::{unix_millis, Message, MessageProperties},
    qos::QoS,
    topic::topic_matches_filter,
};
use rusqlite::{params, Connection, OptionalExtension};

use crate::{
    InflightMessage, InflightStore, Result, RetainedMessageStore, SessionState, SessionStore,
    StorageError, StoredSubscription, StoredWillMessage, WillStore,
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
                dup INTEGER NOT NULL,
                properties BLOB NOT NULL,
                expires_at INTEGER
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
                sequence INTEGER NOT NULL,
                released INTEGER NOT NULL,
                properties BLOB NOT NULL,
                subscription_identifier INTEGER,
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

fn encode_properties(properties: &MessageProperties) -> Vec<u8> {
    let mut buffer = BytesMut::new();
    properties.encode(&mut buffer);
    buffer.to_vec()
}

// ponytail: unreadable properties are dropped rather than failing the load;
// they were written by this module, so only corruption gets here
fn decode_properties(encoded: Vec<u8>) -> MessageProperties {
    MessageProperties::decode(&mut Bytes::from(encoded)).unwrap_or_default()
}

/// Stores each subscription as three digits (QoS, No Local, Retain As
/// Published), the subscription identifier if any, `:` and the topic
/// filter. Subscriptions are separated by NUL: the only character a topic
/// filter cannot contain.
fn encode_subscriptions(subscriptions: &[StoredSubscription]) -> String {
    subscriptions
        .iter()
        .map(|s| {
            let identifier = s
                .subscription_identifier
                .map(|id| id.to_string())
                .unwrap_or_default();
            format!(
                "{}{}{}{}:{}",
                s.qos as u8,
                s.no_local as u8,
                s.retain_as_published as u8,
                identifier,
                s.topic_filter
            )
        })
        .collect::<Vec<_>>()
        .join("\0")
}

fn decode_subscriptions(encoded: &str) -> Vec<StoredSubscription> {
    encoded
        .split('\0')
        .filter_map(|s| {
            let (flags, rest) = s.split_at_checked(3)?;
            let (identifier, topic_filter) = rest.split_once(':')?;
            let flags: Vec<u8> = flags.bytes().map(|b| b.wrapping_sub(b'0')).collect();
            Some(StoredSubscription {
                topic_filter: topic_filter.to_string(),
                qos: flags[0].into(),
                no_local: flags[1] == 1,
                retain_as_published: flags[2] == 1,
                subscription_identifier: identifier.parse().ok(),
            })
        })
        .collect()
}

#[async_trait]
impl SessionStore for SqliteStore {
    async fn save_session(&self, client_id: &str, session: &SessionState) -> Result<()> {
        let client_id = client_id.to_string();
        let subscriptions = encode_subscriptions(&session.subscriptions);
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
                subscriptions: decode_subscriptions(&subs),
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
                    let properties = encode_properties(&msg.properties);
                    let expires_at = msg.expires_at.map(|t| t as i64);
                    conn.execute(
                        "INSERT OR REPLACE INTO retained_messages
                         (topic, payload, qos, dup, properties, expires_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        params![
                            topic,
                            payload,
                            qos_to_int(msg.qos),
                            msg.dup,
                            properties,
                            expires_at
                        ],
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
            let mut stmt = conn.prepare(
                "SELECT topic, payload, qos, dup, properties, expires_at FROM retained_messages",
            )?;
            let rows = stmt.query_map([], |row| {
                let topic: String = row.get(0)?;
                let payload: Option<Vec<u8>> = row.get(1)?;
                let qos: i32 = row.get(2)?;
                let dup: bool = row.get(3)?;
                let properties: Vec<u8> = row.get(4)?;
                let expires_at: Option<i64> = row.get(5)?;
                Ok((topic, payload, qos, dup, properties, expires_at))
            })?;

            let mut messages = Vec::new();

            for row in rows {
                let (topic, payload, qos, dup, properties, expires_at) = row?;
                let expires_at = expires_at.map(|t| t as u64);
                let expired = expires_at.is_some_and(|t| unix_millis() >= t);
                if topic_matches_filter(&topic, &filter) && !expired {
                    messages.push(Message {
                        packet_id: None,
                        topic: Arc::from(topic.as_str()),
                        dup,
                        qos: int_to_qos(qos),
                        retain: true,
                        payload: payload.map(Bytes::from),
                        properties: decode_properties(properties),
                        expires_at,
                        subscription_identifier: None,
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
        // SQLite integers are signed; the bit pattern round-trips
        let sequence = message.sequence as i64;
        let released = message.released;
        let properties = encode_properties(&message.properties);
        let subscription_identifier = message.subscription_identifier;

        self.blocking(move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO inflight_messages
                 (client_id, packet_id, topic, payload, qos, dup, sequence, released, properties,
                  subscription_identifier)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    client_id,
                    packet_id,
                    topic,
                    payload,
                    qos,
                    dup,
                    sequence,
                    released,
                    properties,
                    subscription_identifier
                ],
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
                "SELECT topic, payload, qos, dup, sequence, released, properties,
                 subscription_identifier
                 FROM inflight_messages WHERE client_id = ?1 AND packet_id = ?2",
            )?;

            let result = stmt
                .query_row(params![client_id, packet_id], |row| {
                    let topic: String = row.get(0)?;
                    let payload: Option<Vec<u8>> = row.get(1)?;
                    let qos: i32 = row.get(2)?;
                    let dup: bool = row.get(3)?;
                    let sequence: i64 = row.get(4)?;
                    let released: bool = row.get(5)?;
                    let properties: Vec<u8> = row.get(6)?;
                    let subscription_identifier: Option<u32> = row.get(7)?;
                    Ok(InflightMessage {
                        packet_id,
                        topic,
                        payload: payload.map(Bytes::from),
                        qos: int_to_qos(qos),
                        dup,
                        sequence: sequence as u64,
                        released,
                        properties: decode_properties(properties),
                        subscription_identifier,
                    })
                })
                .optional()?;

            Ok(result)
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

    async fn release_inflight(&self, client_id: &str, packet_id: u16) -> Result<()> {
        let client_id = client_id.to_string();

        self.blocking(move |conn| {
            conn.execute(
                "UPDATE inflight_messages SET released = 1, payload = NULL
                 WHERE client_id = ?1 AND packet_id = ?2",
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
                "SELECT packet_id, topic, payload, qos, dup, sequence, released, properties,
                 subscription_identifier
                 FROM inflight_messages WHERE client_id = ?1",
            )?;

            let rows = stmt.query_map(params![client_id], |row| {
                let packet_id: u16 = row.get(0)?;
                let topic: String = row.get(1)?;
                let payload: Option<Vec<u8>> = row.get(2)?;
                let qos: i32 = row.get(3)?;
                let dup: bool = row.get(4)?;
                let sequence: i64 = row.get(5)?;
                let released: bool = row.get(6)?;
                let properties: Vec<u8> = row.get(7)?;
                let subscription_identifier: Option<u32> = row.get(8)?;
                Ok(InflightMessage {
                    packet_id,
                    topic,
                    payload: payload.map(Bytes::from),
                    qos: int_to_qos(qos),
                    dup,
                    sequence: sequence as u64,
                    released,
                    properties: decode_properties(properties),
                    subscription_identifier,
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
            properties: Default::default(),
            expires_at: None,
            subscription_identifier: None,
        }
    }

    #[tokio::test]
    async fn test_retained_keeps_properties() {
        use mercurio_core::properties::{ResponseTopic, UserProperty};

        let store = SqliteStore::in_memory().unwrap();
        let mut message = create_test_message("a/b", "payload");
        message.properties = MessageProperties {
            response_topic: Some(ResponseTopic::new("reply".into())),
            user_property: Some(vec![UserProperty::new("k".into(), "v".into())]),
            ..Default::default()
        };

        store
            .store_retained("a/b", Some(message.clone()))
            .await
            .unwrap();

        let retained = store.get_retained("a/#").await.unwrap();
        assert_eq!(retained[0].properties, message.properties);
    }

    #[tokio::test]
    async fn test_session_store() {
        let store = SqliteStore::in_memory().unwrap();

        let session = SessionState {
            client_id: "client1".to_string(),
            subscriptions: vec![
                // Commas are valid in topic filters
                StoredSubscription::new("topic/a,b", QoS::AtLeastOnce),
                StoredSubscription {
                    no_local: true,
                    retain_as_published: true,
                    subscription_identifier: Some(268_435_455),
                    ..StoredSubscription::new("topic/+:x", QoS::ExactlyOnce)
                },
            ],
            clean_start: false,
        };

        // Save and load
        store.save_session("client1", &session).await.unwrap();
        let loaded = store.load_session("client1").await.unwrap();
        assert_eq!(loaded.unwrap().subscriptions, session.subscriptions);

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
            qos: QoS::ExactlyOnce,
            dup: false,
            sequence: 7,
            released: false,
            properties: Default::default(),
            subscription_identifier: Some(3),
        };

        store.store_inflight("client1", 1, &msg).await.unwrap();

        let loaded = store.get_inflight("client1", 1).await.unwrap().unwrap();
        assert_eq!(loaded.sequence, 7);
        assert_eq!(loaded.subscription_identifier, Some(3));
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
                subscriptions: vec![StoredSubscription::new("topic/test", QoS::AtMostOnce)],
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
