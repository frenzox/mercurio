//! SQLite storage backend.
//!
//! This module provides a persistent SQLite implementation of all storage traits.
//! Suitable for single-node deployments requiring persistence across restarts.

use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use bytes::Bytes;
use mercurio_core::{
    codec::Decoder,
    message::{unix_millis, Message, MessageOrigin, MessageProperties},
    topic::topic_matches_filter,
};
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, Null, ToSqlOutput, ValueRef};
use rusqlite::{params, Connection, OptionalExtension, ToSql};

use crate::{
    encoding::{
        decode_subscriptions, encode_properties, encode_subscriptions, int_to_qos, qos_to_int,
    },
    BanStore, BatchStore, InflightMessage, InflightStore, QueueStore, Result, RetainedMessageStore,
    SessionOwner, SessionState, SessionStore, StorageError, StoredBan, StoredWillMessage,
    WillStore, Write,
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
    ///
    /// The database is put in WAL mode, so the broker keeps `-wal` and
    /// `-shm` files next to it while it runs.
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::open(Connection::open(path))
    }

    /// Create an in-memory SQLite store (useful for testing).
    pub fn in_memory() -> Result<Self> {
        Self::open(Connection::open_in_memory())
    }

    fn open(conn: rusqlite::Result<Connection>) -> Result<Self> {
        let conn = conn
            .and_then(|conn| {
                configure(&conn)?;
                create_schema(&conn)?;
                Ok(conn)
            })
            .map_err(StorageError::database)?;
        Ok(SqliteStore {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Execute a blocking operation on the tokio runtime.
    async fn blocking<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> rusqlite::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let conn = conn
                .lock()
                .map_err(|e| StorageError::OperationFailed(e.to_string()))?;
            f(&conn).map_err(StorageError::database)
        })
        .await
        .map_err(|e| StorageError::OperationFailed(e.to_string()))?
    }
}

/// WAL lets readers, such as an operator's `sqlite3` shell, work alongside
/// the broker's writes. With WAL, NORMAL keeps the database consistent
/// across a power loss and may lose only the last commits.
fn configure(conn: &Connection) -> rusqlite::Result<()> {
    conn.busy_timeout(Duration::from_secs(5))?;
    // In-memory databases report "memory" and stay that way
    conn.pragma_update_and_check(None, "journal_mode", "WAL", |_| Ok(()))?;
    conn.pragma_update(None, "synchronous", "NORMAL")
}

/// Creates the tables, and adds the columns databases from older versions
/// lack.
fn create_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
            CREATE TABLE IF NOT EXISTS sessions (
                client_id TEXT PRIMARY KEY,
                subscriptions TEXT NOT NULL,
                clean_start INTEGER NOT NULL,
                expiry_interval INTEGER,
                expires_at INTEGER,
                owner_username TEXT,
                owner_mount_point TEXT
            );

            CREATE TABLE IF NOT EXISTS retained_messages (
                topic TEXT PRIMARY KEY,
                payload BLOB,
                qos INTEGER NOT NULL,
                dup INTEGER NOT NULL,
                properties BLOB NOT NULL,
                expires_at INTEGER,
                origin_client_id TEXT,
                origin_username TEXT
            );

            CREATE TABLE IF NOT EXISTS will_messages (
                client_id TEXT PRIMARY KEY,
                topic TEXT NOT NULL,
                payload BLOB NOT NULL,
                qos INTEGER NOT NULL,
                retain INTEGER NOT NULL,
                delay_secs INTEGER NOT NULL,
                properties BLOB NOT NULL
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
                subscription_identifiers TEXT,
                retain INTEGER NOT NULL DEFAULT 0,
                expires_at INTEGER,
                PRIMARY KEY (client_id, packet_id)
            );

            CREATE TABLE IF NOT EXISTS queued_messages (
                client_id TEXT NOT NULL,
                sequence INTEGER NOT NULL,
                topic TEXT NOT NULL,
                payload BLOB,
                qos INTEGER NOT NULL,
                retain INTEGER NOT NULL,
                properties BLOB NOT NULL,
                expires_at INTEGER,
                subscription_identifiers TEXT,
                PRIMARY KEY (client_id, sequence)
            );

            CREATE TABLE IF NOT EXISTS bans (
                kind TEXT NOT NULL,
                value TEXT NOT NULL,
                prefix INTEGER NOT NULL,
                reason TEXT,
                created_at INTEGER NOT NULL,
                expires_at INTEGER,
                PRIMARY KEY (kind, value, prefix)
            );

            CREATE INDEX IF NOT EXISTS idx_inflight_client
                ON inflight_messages(client_id);
            ",
    )?;

    // Databases created before retained messages kept their publisher
    let has_origin = conn
        .prepare("SELECT origin_client_id FROM retained_messages LIMIT 0")
        .is_ok();
    if !has_origin {
        conn.execute_batch(
            "ALTER TABLE retained_messages ADD COLUMN origin_client_id TEXT;
                 ALTER TABLE retained_messages ADD COLUMN origin_username TEXT;",
        )?;
    }

    // Databases created before sessions recorded their owner
    let has_owner = conn
        .prepare("SELECT owner_username FROM sessions LIMIT 0")
        .is_ok();
    if !has_owner {
        conn.execute_batch(
            "ALTER TABLE sessions ADD COLUMN owner_username TEXT;
                 ALTER TABLE sessions ADD COLUMN owner_mount_point TEXT;",
        )?;
    }

    // Databases created before in-flight messages kept their RETAIN flag
    // and expiry
    let has_inflight_retain = conn
        .prepare("SELECT retain FROM inflight_messages LIMIT 0")
        .is_ok();
    if !has_inflight_retain {
        conn.execute_batch(
            "ALTER TABLE inflight_messages ADD COLUMN retain INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE inflight_messages ADD COLUMN expires_at INTEGER;",
        )?;
    }

    Ok(())
}

/// Fails like any other unreadable column if the stored properties can't
/// be decoded, rather than delivering the message without them.
fn decode_properties(encoded: Vec<u8>) -> rusqlite::Result<MessageProperties> {
    MessageProperties::decode(&mut Bytes::from(encoded)).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Blob, Box::new(e))
    })
}

/// The subscription identifiers of a stored message, as comma-separated
/// text, or no value when there are none.
struct Identifiers(Vec<u32>);

impl ToSql for Identifiers {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        if self.0.is_empty() {
            return Ok(ToSqlOutput::from(Null));
        }
        let text: Vec<String> = self.0.iter().map(u32::to_string).collect();
        Ok(ToSqlOutput::from(text.join(",")))
    }
}

impl FromSql for Identifiers {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        match value {
            ValueRef::Null => Ok(Identifiers(Vec::new())),
            ValueRef::Text(text) => std::str::from_utf8(text)
                .ok()
                .and_then(|text| text.split(',').map(|id| id.parse().ok()).collect())
                .map(Identifiers)
                .ok_or(FromSqlError::InvalidType),
            _ => Err(FromSqlError::InvalidType),
        }
    }
}

/// Selects the columns `inflight_from_row` reads.
const INFLIGHT_COLUMNS: &str = "SELECT packet_id, topic, payload, qos, dup, sequence, released,
    properties, subscription_identifiers, retain, expires_at FROM inflight_messages";

fn inflight_from_row(row: &rusqlite::Row) -> rusqlite::Result<InflightMessage> {
    let payload: Option<Vec<u8>> = row.get(2)?;
    Ok(InflightMessage {
        packet_id: row.get(0)?,
        topic: row.get(1)?,
        payload: payload.map(Bytes::from),
        qos: int_to_qos(row.get(3)?),
        dup: row.get(4)?,
        // SQLite integers are signed; the bit pattern round-trips
        sequence: row.get::<_, i64>(5)? as u64,
        released: row.get(6)?,
        properties: decode_properties(row.get(7)?)?,
        subscription_identifiers: row.get::<_, Identifiers>(8)?.0,
        retain: row.get(9)?,
        expires_at: row.get::<_, Option<i64>>(10)?.map(|t| t as u64),
    })
}

/// Applies one change; the caller holds the transaction.
fn apply_write(conn: &Connection, client_id: &str, write: &Write) -> rusqlite::Result<()> {
    match write {
        Write::SaveSession(session) => {
            conn.execute(
                "INSERT OR REPLACE INTO sessions
                 (client_id, subscriptions, clean_start, expiry_interval, expires_at,
                  owner_username, owner_mount_point)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    client_id,
                    encode_subscriptions(&session.subscriptions),
                    // No longer used; kept for databases that require it
                    false,
                    session.expiry_interval,
                    session.expires_at.map(|t| t as i64),
                    session.owner.username,
                    session.owner.mount_point
                ],
            )?;
        }
        Write::SaveSubscriptions(subscriptions) => {
            conn.execute(
                "INSERT INTO sessions (client_id, subscriptions, clean_start)
                 VALUES (?1, ?2, 0)
                 ON CONFLICT (client_id) DO UPDATE SET subscriptions = excluded.subscriptions",
                params![client_id, encode_subscriptions(subscriptions)],
            )?;
        }
        Write::SetExpiry {
            interval,
            expires_at,
        } => {
            conn.execute(
                "UPDATE sessions SET expiry_interval = ?2, expires_at = ?3 WHERE client_id = ?1",
                params![client_id, interval, *expires_at as i64],
            )?;
        }
        Write::StoreWill(will) => {
            conn.execute(
                "INSERT OR REPLACE INTO will_messages
                 (client_id, topic, payload, qos, retain, delay_secs, properties)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    client_id,
                    will.topic,
                    &will.payload[..],
                    qos_to_int(will.qos),
                    will.retain,
                    will.delay_secs,
                    encode_properties(&will.properties)
                ],
            )?;
        }
        Write::DeleteWill => {
            conn.execute(
                "DELETE FROM will_messages WHERE client_id = ?1",
                params![client_id],
            )?;
        }
        Write::StoreInflight(message) => {
            conn.execute(
                "INSERT OR REPLACE INTO inflight_messages
                 (client_id, packet_id, topic, payload, qos, dup, sequence, released, properties,
                  subscription_identifiers, retain, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    client_id,
                    message.packet_id,
                    message.topic,
                    message.payload.as_deref(),
                    qos_to_int(message.qos),
                    message.dup,
                    // SQLite integers are signed; the bit pattern round-trips
                    message.sequence as i64,
                    message.released,
                    encode_properties(&message.properties),
                    Identifiers(message.subscription_identifiers.clone()),
                    message.retain,
                    message.expires_at.map(|t| t as i64)
                ],
            )?;
        }
        Write::ReleaseInflight(packet_id) => {
            conn.execute(
                "UPDATE inflight_messages SET released = 1, payload = NULL
                 WHERE client_id = ?1 AND packet_id = ?2",
                params![client_id, packet_id],
            )?;
        }
        Write::RemoveInflight(packet_id) => {
            conn.execute(
                "DELETE FROM inflight_messages WHERE client_id = ?1 AND packet_id = ?2",
                params![client_id, packet_id],
            )?;
        }
        Write::Enqueue(sequence, message) => {
            conn.execute(
                "INSERT OR REPLACE INTO queued_messages
                 (client_id, sequence, topic, payload, qos, retain, properties, expires_at,
                  subscription_identifiers)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    client_id,
                    *sequence as i64,
                    &*message.topic,
                    message.payload.as_deref(),
                    qos_to_int(message.qos),
                    message.retain,
                    encode_properties(&message.properties),
                    message.expires_at.map(|t| t as i64),
                    Identifiers(message.subscription_identifiers.clone())
                ],
            )?;
        }
        Write::ClearQueue => {
            conn.execute(
                "DELETE FROM queued_messages WHERE client_id = ?1",
                params![client_id],
            )?;
        }
        Write::DeleteSession => {
            for table in [
                "sessions",
                "will_messages",
                "queued_messages",
                "inflight_messages",
            ] {
                conn.execute(
                    &format!("DELETE FROM {table} WHERE client_id = ?1"),
                    params![client_id],
                )?;
            }
        }
    }
    Ok(())
}

/// Applies a batch in one transaction: one write to disk instead of one
/// per change.
#[async_trait]
impl BatchStore for SqliteStore {
    fn backend_name(&self) -> &'static str {
        "sqlite"
    }

    fn ban_store(self: Arc<Self>) -> Option<Arc<dyn BanStore>> {
        Some(self)
    }

    async fn apply(&self, client_id: &str, writes: &[Write]) -> Result<()> {
        let client_id = client_id.to_string();
        let writes = writes.to_vec();
        self.blocking(move |conn| {
            // Rolls back on drop, also when COMMIT itself fails
            let tx = conn.unchecked_transaction()?;
            for write in &writes {
                apply_write(&tx, &client_id, write)?;
            }
            tx.commit()
        })
        .await
    }
}

#[async_trait]
impl BanStore for SqliteStore {
    async fn load_bans(&self) -> Result<Vec<StoredBan>> {
        self.blocking(|conn| {
            let mut stmt = conn
                .prepare("SELECT kind, value, prefix, reason, created_at, expires_at FROM bans")?;
            let rows = stmt.query_map([], |row| {
                Ok(StoredBan {
                    kind: row.get(0)?,
                    value: row.get(1)?,
                    prefix: row.get(2)?,
                    reason: row.get(3)?,
                    created_at: row.get::<_, i64>(4)? as u64,
                    expires_at: row.get::<_, Option<i64>>(5)?.map(|t| t as u64),
                })
            })?;
            let bans = rows.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(bans)
        })
        .await
    }

    async fn store_ban(&self, ban: &StoredBan) -> Result<()> {
        let ban = ban.clone();
        self.blocking(move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO bans (kind, value, prefix, reason, created_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    ban.kind,
                    ban.value,
                    ban.prefix,
                    ban.reason,
                    ban.created_at as i64,
                    ban.expires_at.map(|t| t as i64)
                ],
            )?;
            Ok(())
        })
        .await
    }

    async fn delete_ban(&self, kind: &str, value: &str, prefix: bool) -> Result<()> {
        let (kind, value) = (kind.to_string(), value.to_string());
        self.blocking(move |conn| {
            conn.execute(
                "DELETE FROM bans WHERE kind = ?1 AND value = ?2 AND prefix = ?3",
                params![kind, value, prefix],
            )?;
            Ok(())
        })
        .await
    }
}

#[async_trait]
impl SessionStore for SqliteStore {
    async fn save_session(&self, client_id: &str, session: &SessionState) -> Result<()> {
        self.apply(client_id, &[Write::SaveSession(session.clone())])
            .await
    }

    async fn load_session(&self, client_id: &str) -> Result<Option<SessionState>> {
        let client_id = client_id.to_string();

        self.blocking(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT subscriptions, clean_start, expiry_interval, expires_at,
                        owner_username, owner_mount_point
                 FROM sessions WHERE client_id = ?1",
            )?;

            let result = stmt
                .query_row(params![client_id.clone()], |row| {
                    let subs: String = row.get(0)?;
                    Ok(SessionState {
                        client_id: client_id.clone(),
                        subscriptions: decode_subscriptions(&subs),
                        expiry_interval: row.get(2)?,
                        expires_at: row.get::<_, Option<i64>>(3)?.map(|t| t as u64),
                        owner: SessionOwner {
                            username: row.get(4)?,
                            mount_point: row.get(5)?,
                        },
                    })
                })
                .optional()?;

            Ok(result)
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
                    let origin = msg.origin.as_deref();
                    conn.execute(
                        "INSERT OR REPLACE INTO retained_messages
                         (topic, payload, qos, dup, properties, expires_at,
                          origin_client_id, origin_username)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                        params![
                            topic,
                            payload,
                            qos_to_int(msg.qos),
                            msg.dup,
                            properties,
                            expires_at,
                            origin.map(|o| &o.client_id),
                            origin.and_then(|o| o.username.as_ref()),
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
            // Expired messages are dropped for good, not just skipped
            conn.execute(
                "DELETE FROM retained_messages WHERE expires_at <= ?1",
                params![unix_millis() as i64],
            )?;

            let mut stmt = conn.prepare(
                "SELECT topic, payload, qos, dup, properties, expires_at,
                        origin_client_id, origin_username
                 FROM retained_messages",
            )?;
            let rows = stmt.query_map([], |row| {
                let topic: String = row.get(0)?;
                let payload: Option<Vec<u8>> = row.get(1)?;
                let qos: i32 = row.get(2)?;
                let dup: bool = row.get(3)?;
                let properties: Vec<u8> = row.get(4)?;
                let expires_at: Option<i64> = row.get(5)?;
                let origin_client_id: Option<String> = row.get(6)?;
                let origin_username: Option<String> = row.get(7)?;
                let origin = origin_client_id.map(|client_id| {
                    Arc::new(MessageOrigin {
                        client_id,
                        username: origin_username,
                    })
                });
                Ok((topic, payload, qos, dup, properties, expires_at, origin))
            })?;

            let mut messages = Vec::new();

            for row in rows {
                let (topic, payload, qos, dup, properties, expires_at, origin) = row?;
                if topic_matches_filter(&topic, &filter) {
                    messages.push(Message {
                        packet_id: None,
                        topic: Arc::from(topic.as_str()),
                        dup,
                        qos: int_to_qos(qos),
                        retain: true,
                        payload: payload.map(Bytes::from),
                        properties: decode_properties(properties)?,
                        expires_at: expires_at.map(|t| t as u64),
                        subscription_identifiers: Vec::new(),
                        origin,
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

    async fn count_retained(&self) -> Result<usize> {
        self.blocking(move |conn| {
            let count: i64 =
                conn.query_row("SELECT COUNT(*) FROM retained_messages", [], |row| {
                    row.get(0)
                })?;
            Ok(count as usize)
        })
        .await
    }
}

#[async_trait]
impl WillStore for SqliteStore {
    async fn store_will(&self, client_id: &str, will: &StoredWillMessage) -> Result<()> {
        self.apply(client_id, &[Write::StoreWill(will.clone())])
            .await
    }

    async fn get_will(&self, client_id: &str) -> Result<Option<StoredWillMessage>> {
        let client_id = client_id.to_string();

        self.blocking(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT topic, payload, qos, retain, delay_secs, properties
                 FROM will_messages WHERE client_id = ?1",
            )?;

            let result = stmt
                .query_row(params![client_id], |row| {
                    let payload: Vec<u8> = row.get(1)?;
                    let properties: Vec<u8> = row.get(5)?;
                    Ok(StoredWillMessage {
                        topic: row.get(0)?,
                        payload: Bytes::from(payload),
                        qos: int_to_qos(row.get(2)?),
                        retain: row.get(3)?,
                        delay_secs: row.get(4)?,
                        properties: decode_properties(properties)?,
                    })
                })
                .optional()?;

            Ok(result)
        })
        .await
    }

    async fn delete_will(&self, client_id: &str) -> Result<()> {
        self.apply(client_id, &[Write::DeleteWill]).await
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
        let message = InflightMessage {
            packet_id,
            ..message.clone()
        };
        self.apply(client_id, &[Write::StoreInflight(message)])
            .await
    }

    async fn get_inflight(
        &self,
        client_id: &str,
        packet_id: u16,
    ) -> Result<Option<InflightMessage>> {
        let client_id = client_id.to_string();

        self.blocking(move |conn| {
            conn.query_row(
                &format!("{INFLIGHT_COLUMNS} WHERE client_id = ?1 AND packet_id = ?2"),
                params![client_id, packet_id],
                inflight_from_row,
            )
            .optional()
        })
        .await
    }

    async fn remove_inflight(&self, client_id: &str, packet_id: u16) -> Result<()> {
        self.apply(client_id, &[Write::RemoveInflight(packet_id)])
            .await
    }

    async fn release_inflight(&self, client_id: &str, packet_id: u16) -> Result<()> {
        self.apply(client_id, &[Write::ReleaseInflight(packet_id)])
            .await
    }

    async fn get_all_inflight(&self, client_id: &str) -> Result<Vec<InflightMessage>> {
        let client_id = client_id.to_string();

        self.blocking(move |conn| {
            let mut stmt = conn.prepare(&format!("{INFLIGHT_COLUMNS} WHERE client_id = ?1"))?;
            let rows = stmt.query_map(params![client_id], inflight_from_row)?;
            rows.collect()
        })
        .await
    }
}

#[async_trait]
impl QueueStore for SqliteStore {
    async fn enqueue(&self, client_id: &str, sequence: u64, message: &Message) -> Result<()> {
        self.apply(client_id, &[Write::Enqueue(sequence, message.clone())])
            .await
    }

    async fn load_queue(&self, client_id: &str) -> Result<Vec<(u64, Message)>> {
        let client_id = client_id.to_string();

        self.blocking(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT sequence, topic, payload, qos, retain, properties, expires_at,
                        subscription_identifiers
                 FROM queued_messages WHERE client_id = ?1 ORDER BY sequence",
            )?;
            let rows = stmt.query_map(params![client_id], |row| {
                let topic: String = row.get(1)?;
                let payload: Option<Vec<u8>> = row.get(2)?;
                let message = Message {
                    packet_id: None,
                    topic: Arc::from(topic.as_str()),
                    dup: false,
                    qos: int_to_qos(row.get(3)?),
                    retain: row.get(4)?,
                    payload: payload.map(Bytes::from),
                    properties: decode_properties(row.get(5)?)?,
                    expires_at: row.get::<_, Option<i64>>(6)?.map(|t| t as u64),
                    subscription_identifiers: row.get::<_, Identifiers>(7)?.0,
                    origin: None,
                };
                Ok((row.get::<_, i64>(0)? as u64, message))
            })?;

            let mut queue = Vec::new();
            for row in rows {
                queue.push(row?);
            }
            Ok(queue)
        })
        .await
    }

    async fn clear_queue(&self, client_id: &str) -> Result<()> {
        self.apply(client_id, &[Write::ClearQueue]).await
    }
}

#[cfg(test)]
mod tests {
    use mercurio_core::qos::QoS;

    use super::*;
    use crate::StoredSubscription;

    #[tokio::test]
    async fn test_bans() {
        let store = Arc::new(SqliteStore::in_memory().unwrap());
        crate::testing::bans_round_trip(store.as_ref()).await;
        assert!(store.ban_store().is_some());
    }

    #[tokio::test]
    async fn test_bans_survive_reopening() {
        let path =
            std::env::temp_dir().join(format!("mercurio-store-bans-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let ban = crate::testing::ban("username", "guest", true);
        SqliteStore::new(&path)
            .unwrap()
            .store_ban(&ban)
            .await
            .unwrap();

        let reopened = SqliteStore::new(&path).unwrap();
        assert_eq!(reopened.load_bans().await.unwrap(), vec![ban]);
        drop(reopened);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn test_backend_is_named() {
        assert_eq!(SqliteStore::in_memory().unwrap().backend_name(), "sqlite");
    }

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
            subscription_identifiers: Vec::new(),
            origin: None,
        }
    }

    #[tokio::test]
    async fn test_retained_keeps_its_origin() {
        let store = SqliteStore::in_memory().unwrap();
        let mut message = create_test_message("a/b", "payload");
        message.origin = Some(Arc::new(MessageOrigin {
            client_id: "c".into(),
            username: Some("alice".into()),
        }));
        store.store_retained("a/b", Some(message)).await.unwrap();
        store
            .store_retained("a/c", Some(create_test_message("a/c", "x")))
            .await
            .unwrap();

        let mut retained = store.get_retained("a/+").await.unwrap();
        retained.sort_by(|a, b| a.topic.cmp(&b.topic));
        let origin = retained[0].origin.as_deref().unwrap();
        assert_eq!(origin.client_id, "c");
        assert_eq!(origin.username.as_deref(), Some("alice"));
        assert!(retained[1].origin.is_none());
    }

    #[test]
    fn test_older_databases_gain_the_owner_columns() {
        let path = std::env::temp_dir().join(format!("mercurio-owner-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        Connection::open(&path)
            .unwrap()
            .execute_batch(
                "CREATE TABLE sessions (
                    client_id TEXT PRIMARY KEY, subscriptions TEXT NOT NULL,
                    clean_start INTEGER NOT NULL, expiry_interval INTEGER, expires_at INTEGER
                );
                INSERT INTO sessions VALUES ('old', '', 0, NULL, NULL);",
            )
            .unwrap();

        let store = SqliteStore::new(&path).unwrap();
        let loaded = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(store.load_session("old"))
            .unwrap()
            .unwrap();
        // Sessions from before owners were kept count as anonymous ones
        assert_eq!(loaded.owner, SessionOwner::default());
        drop(store);
        std::fs::remove_file(&path).unwrap();
    }

    #[tokio::test]
    async fn test_older_in_flight_messages_load_without_retain_or_expiry() {
        let path =
            std::env::temp_dir().join(format!("mercurio-inflight-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        Connection::open(&path)
            .unwrap()
            .execute_batch(
                "CREATE TABLE inflight_messages (
                    client_id TEXT NOT NULL, packet_id INTEGER NOT NULL, topic TEXT NOT NULL,
                    payload BLOB, qos INTEGER NOT NULL, dup INTEGER NOT NULL,
                    sequence INTEGER NOT NULL, released INTEGER NOT NULL,
                    properties BLOB NOT NULL, subscription_identifiers TEXT,
                    PRIMARY KEY (client_id, packet_id)
                );
                INSERT INTO inflight_messages VALUES ('c', 4, 't', x'70', 1, 0, 9, 0, x'', NULL);",
            )
            .unwrap();

        let store = SqliteStore::new(&path).unwrap();
        let loaded = store.get_all_inflight("c").await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!((loaded[0].packet_id, loaded[0].sequence), (4, 9));
        assert!(!loaded[0].retain);
        assert_eq!(loaded[0].expires_at, None);
        drop(store);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn test_older_databases_gain_the_origin_columns() {
        let path = std::env::temp_dir().join(format!("mercurio-migrate-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        Connection::open(&path)
            .unwrap()
            .execute_batch(
                "CREATE TABLE retained_messages (
                    topic TEXT PRIMARY KEY, payload BLOB, qos INTEGER NOT NULL,
                    dup INTEGER NOT NULL, properties BLOB NOT NULL, expires_at INTEGER
                );",
            )
            .unwrap();

        let store = SqliteStore::new(&path).unwrap();
        let conn = store.conn.lock().unwrap();
        assert!(conn
            .prepare("SELECT origin_client_id, origin_username FROM retained_messages")
            .is_ok());
        drop(conn);
        drop(store);
        std::fs::remove_file(&path).unwrap();
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
    async fn test_corrupt_properties_are_an_error() {
        let store = SqliteStore::in_memory().unwrap();
        store
            .blocking(|conn| {
                // 0xff is not a property identifier
                conn.execute(
                    "INSERT INTO retained_messages (topic, payload, qos, dup, properties)
                     VALUES ('t', x'00', 0, 0, x'ff')",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();

        assert!(store.get_retained("t").await.is_err());
    }

    #[tokio::test]
    async fn test_queue_store() {
        let store = SqliteStore::in_memory().unwrap();
        let mut second = create_test_message("b", "2");
        second.subscription_identifiers = vec![4, 268_435_455];
        store.enqueue("c", 2, &second).await.unwrap();
        store
            .enqueue("c", 1, &create_test_message("a", "1"))
            .await
            .unwrap();
        store
            .enqueue("other", 1, &create_test_message("x", "x"))
            .await
            .unwrap();

        let queue = store.load_queue("c").await.unwrap();
        let topics: Vec<(u64, &str)> = queue.iter().map(|(s, m)| (*s, &*m.topic)).collect();
        assert_eq!(topics, [(1, "a"), (2, "b")]);
        assert_eq!(queue[1].1.subscription_identifiers, [4, 268_435_455]);
        assert!(queue[0].1.subscription_identifiers.is_empty());

        store.clear_queue("c").await.unwrap();
        assert!(store.load_queue("c").await.unwrap().is_empty());
        assert_eq!(store.load_queue("other").await.unwrap().len(), 1);
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
            expiry_interval: Some(60),
            expires_at: Some(1_700_000_000_000),
            owner: SessionOwner {
                username: Some("alice".into()),
                mount_point: Some("tenant/".into()),
            },
        };

        // Save and load
        store.save_session("client1", &session).await.unwrap();
        let loaded = store.load_session("client1").await.unwrap().unwrap();
        assert_eq!(loaded.subscriptions, session.subscriptions);
        assert_eq!(loaded.expiry_interval, Some(60));
        assert_eq!(loaded.expires_at, Some(1_700_000_000_000));
        assert_eq!(loaded.owner, session.owner);

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
            delay_secs: 30,
            properties: MessageProperties {
                content_type: Some(mercurio_core::properties::ContentType::new("text".into())),
                ..Default::default()
            },
        };

        store.store_will("client1", &will).await.unwrap();
        let loaded = store.get_will("client1").await.unwrap().unwrap();
        assert_eq!(loaded.topic, "client/status");
        assert_eq!(loaded.delay_secs, 30);
        assert_eq!(loaded.properties, will.properties);

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
            retain: true,
            expires_at: Some(1_700_000_000_000),
            sequence: 7,
            released: false,
            properties: Default::default(),
            subscription_identifiers: vec![3],
        };

        store.store_inflight("client1", 1, &msg).await.unwrap();

        let loaded = store.get_inflight("client1", 1).await.unwrap().unwrap();
        assert_eq!(loaded.sequence, 7);
        assert_eq!(loaded.subscription_identifiers, [3]);
        assert!(!loaded.released);
        assert!(loaded.retain);
        assert_eq!(loaded.expires_at, Some(1_700_000_000_000));

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
    async fn test_expired_retained_messages_are_removed() {
        let store = SqliteStore::in_memory().unwrap();
        let mut expired = create_test_message("old", "x");
        expired.expires_at = Some(1);
        store.store_retained("old", Some(expired)).await.unwrap();
        store
            .store_retained("new", Some(create_test_message("new", "y")))
            .await
            .unwrap();

        let retained = store.get_retained("#").await.unwrap();
        assert_eq!(retained.len(), 1);
        assert_eq!(&*retained[0].topic, "new");
        let count: i64 = store
            .blocking(|conn| {
                conn.query_row("SELECT COUNT(*) FROM retained_messages", [], |r| r.get(0))
            })
            .await
            .unwrap();
        assert_eq!(count, 1);
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
    async fn test_readers_do_not_block_writes() {
        let path = std::env::temp_dir().join(format!("mercurio-wal-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = SqliteStore::new(&path).unwrap();
        {
            let conn = store.conn.lock().unwrap();
            let mode: String = conn
                .query_row("PRAGMA journal_mode", [], |row| row.get(0))
                .unwrap();
            assert_eq!(mode, "wal");
            // 1 is NORMAL
            let synchronous: i64 = conn
                .query_row("PRAGMA synchronous", [], |row| row.get(0))
                .unwrap();
            assert_eq!(synchronous, 1);
            conn.busy_timeout(std::time::Duration::ZERO).unwrap();
        }

        let reader = Connection::open(&path).unwrap();
        reader.execute_batch("BEGIN").unwrap();
        reader
            .query_row("SELECT COUNT(*) FROM retained_messages", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap();
        store
            .store_retained("a", Some(create_test_message("a", "1")))
            .await
            .unwrap();
        reader.execute_batch("COMMIT").unwrap();
        drop(reader);
        drop(store);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn test_in_memory_store_keeps_its_journal_mode() {
        let store = SqliteStore::in_memory().unwrap();
        let conn = store.conn.lock().unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "memory");
    }

    #[tokio::test]
    async fn test_failed_commit_does_not_wedge_the_store() {
        let path = std::env::temp_dir().join(format!("mercurio-commit-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = SqliteStore::new(&path).unwrap();
        {
            let conn = store.conn.lock().unwrap();
            // A reader blocks COMMIT only under the rollback journal
            conn.pragma_update(None, "journal_mode", "DELETE").unwrap();
            conn.busy_timeout(std::time::Duration::ZERO).unwrap();
        }
        let session = |client_id: &str| SessionState {
            client_id: client_id.to_string(),
            subscriptions: Vec::new(),
            expiry_interval: None,
            expires_at: None,
            owner: Default::default(),
        };

        let reader = Connection::open(&path).unwrap();
        reader.execute_batch("BEGIN").unwrap();
        reader
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap();
        assert!(store
            .save_session("blocked", &session("blocked"))
            .await
            .is_err());
        reader.execute_batch("COMMIT").unwrap();

        store
            .save_session("after", &session("after"))
            .await
            .unwrap();
        drop(store);
        let fresh = Connection::open(&path).unwrap();
        let saved: Vec<String> = fresh
            .prepare("SELECT client_id FROM sessions")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(saved, ["after"]);
        drop(fresh);
        std::fs::remove_file(&path).unwrap();
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
                expiry_interval: None,
                expires_at: None,
                owner: Default::default(),
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
