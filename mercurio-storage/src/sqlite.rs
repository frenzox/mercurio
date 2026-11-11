//! SQLite storage backend.
//!
//! This module provides a persistent SQLite implementation of all storage traits.
//! Suitable for single-node deployments requiring persistence across restarts.

use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions, TryLockError},
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use bytes::Bytes;
use mercurio_core::{
    codec::Decoder,
    message::{unix_millis, Message, MessageOrigin, MessageProperties},
    qos::QoS,
    topic::topic_matches_filter,
};
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, Null, ToSqlOutput, ValueRef};
use rusqlite::{params, Connection, OptionalExtension, ToSql};

use crate::{
    encoding::{decode_subscriptions, encode_properties, int_to_qos, qos_to_int},
    BanStore, BatchStore, InflightMessage, LoadedClient, Result, RetainedMessageStore, SessionKey,
    SessionOwner, SessionState, StorageError, StoredBan, StoredClient, StoredSubscription,
    StoredWillMessage, TopicRange, Write,
};

/// SQLite storage backend implementing all storage traits.
///
/// Uses a single SQLite connection protected by a mutex. For high-concurrency
/// scenarios, consider using a connection pool.
#[derive(Clone)]
pub struct SqliteStore {
    conn: Arc<Mutex<Connection>>,
    /// Holds the lock file of a database on disk while any clone lives
    _lock: Option<Arc<File>>,
}

impl SqliteStore {
    /// Create a new SQLite store with the given database path.
    ///
    /// Creates the database file, readable by its owner only, and the
    /// tables if they don't exist, and migrates the schema of a database
    /// from an older version. Fails for a database written by a newer
    /// version, and while another store uses the database.
    ///
    /// The database is put in WAL mode, so the broker keeps `-wal` and
    /// `-shm` files next to it while it runs, and holds a lock on a
    /// `.lock` file next to it. Other programs may read the database
    /// meanwhile, such as `sqlite3` making a backup.
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let lock = lock(path)?;
        create_private(path)?;
        Self::open(Connection::open(path), Some(Arc::new(lock)))
    }

    /// Create an in-memory SQLite store (useful for testing).
    pub fn in_memory() -> Result<Self> {
        Self::open(Connection::open_in_memory(), None)
    }

    fn open(conn: rusqlite::Result<Connection>, lock: Option<Arc<File>>) -> Result<Self> {
        let mut conn = conn.map_err(StorageError::database)?;
        configure(&conn).map_err(StorageError::database)?;
        migrate(&mut conn)?;
        Ok(SqliteStore {
            conn: Arc::new(Mutex::new(conn)),
            _lock: lock,
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

/// Takes the lock of the database at `path`, which brokers sharing it
/// would otherwise overwrite each other's sessions in. A lock file of its
/// own, as SQLite's locks on the database come and go with transactions:
/// `locking_mode=EXCLUSIVE` would also keep readers out, such as `sqlite3`
/// making a backup.
fn lock(path: &Path) -> Result<File> {
    let mut lock_path = path.as_os_str().to_owned();
    lock_path.push(".lock");
    let file = private_options().open(&lock_path)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(StorageError::OperationFailed(format!(
            "another broker uses the database {}: {} is locked",
            path.display(),
            Path::new(&lock_path).display()
        ))),
        Err(TryLockError::Error(e)) => Err(e.into()),
    }
}

/// Creates or opens files readable and writable by their owner only.
fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
}

/// Creates the database file, if missing, readable by its owner only.
/// SQLite gives the `-wal` and `-shm` files the database's permissions.
fn create_private(path: &Path) -> Result<()> {
    if !path.exists() {
        private_options().open(path)?;
    }
    Ok(())
}

/// Schema changes, applied in order, each in a transaction of its own; the
/// database records how many it has in `user_version`. Released entries
/// never change: a new schema change is a new entry.
const MIGRATIONS: &[fn(&Connection) -> rusqlite::Result<()>] = &[
    create_schema,
    index_retained_expiry,
    key_by_mount_point,
    mark_share_groups,
];

/// Records which shared subscription a queued or in-flight message came
/// through, so it can go to another member of the group.
fn mark_share_groups(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "ALTER TABLE queued_messages ADD COLUMN share_group TEXT;
         ALTER TABLE inflight_messages ADD COLUMN share_group TEXT;",
    )
}

/// Lets the removal of expired retained messages find them without
/// reading the table.
fn index_retained_expiry(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS retained_messages_expires_at
         ON retained_messages (expires_at) WHERE expires_at IS NOT NULL",
    )
}

/// The tables of a client's state other than `sessions`, with their
/// columns besides the key, as `key_by_mount_point` rebuilds them.
const CLIENT_TABLES: [(&str, &str, &str); 5] = [
    (
        "subscriptions",
        "topic_filter TEXT NOT NULL, qos INTEGER NOT NULL, no_local INTEGER NOT NULL,
         retain_as_published INTEGER NOT NULL, subscription_identifier INTEGER,
         PRIMARY KEY (mount_point, client_id, topic_filter)",
        "topic_filter, qos, no_local, retain_as_published, subscription_identifier",
    ),
    (
        "will_messages",
        "topic TEXT NOT NULL, payload BLOB NOT NULL, qos INTEGER NOT NULL,
         retain INTEGER NOT NULL, delay_secs INTEGER NOT NULL, properties BLOB NOT NULL,
         PRIMARY KEY (mount_point, client_id)",
        "topic, payload, qos, retain, delay_secs, properties",
    ),
    (
        "inflight_messages",
        "packet_id INTEGER NOT NULL, topic TEXT NOT NULL, payload BLOB, qos INTEGER NOT NULL,
         dup INTEGER NOT NULL, sequence INTEGER NOT NULL, released INTEGER NOT NULL,
         properties BLOB NOT NULL, subscription_identifiers TEXT,
         retain INTEGER NOT NULL DEFAULT 0, expires_at INTEGER,
         PRIMARY KEY (mount_point, client_id, packet_id)",
        "packet_id, topic, payload, qos, dup, sequence, released, properties,
         subscription_identifiers, retain, expires_at",
    ),
    (
        "queued_messages",
        "sequence INTEGER NOT NULL, topic TEXT NOT NULL, payload BLOB, qos INTEGER NOT NULL,
         retain INTEGER NOT NULL, properties BLOB NOT NULL, expires_at INTEGER,
         subscription_identifiers TEXT,
         PRIMARY KEY (mount_point, client_id, sequence)",
        "sequence, topic, payload, qos, retain, properties, expires_at,
         subscription_identifiers",
    ),
    (
        "incoming_qos2",
        "packet_id INTEGER NOT NULL, PRIMARY KEY (mount_point, client_id, packet_id)",
        "packet_id",
    ),
];

/// Makes the mount point part of the key of every table of a client's
/// state, so the same client id on two mount points keeps two sessions.
/// Rows take the mount point of their session's owner, and those without a
/// session none, stored as an empty string. SQLite can't change a primary
/// key, so each table is rebuilt; the index on in-flight messages' client
/// ids goes with its table, as the new primary key serves its lookups.
fn key_by_mount_point(conn: &Connection) -> rusqlite::Result<()> {
    for (table, columns, names) in CLIENT_TABLES {
        conn.execute_batch(&format!(
            "CREATE TABLE {table}_keyed (
                 mount_point TEXT NOT NULL DEFAULT '',
                 client_id TEXT NOT NULL,
                 {columns}
             );
             INSERT INTO {table}_keyed (mount_point, client_id, {names})
             SELECT COALESCE(
                        (SELECT s.owner_mount_point FROM sessions s
                         WHERE s.client_id = t.client_id),
                        ''
                    ),
                    t.client_id, {names}
             FROM {table} t WHERE t.client_id IS NOT NULL;
             DROP TABLE {table};
             ALTER TABLE {table}_keyed RENAME TO {table};"
        ))?;
    }
    conn.execute_batch(
        "CREATE TABLE sessions_keyed (
             mount_point TEXT NOT NULL DEFAULT '',
             client_id TEXT NOT NULL,
             subscriptions TEXT NOT NULL,
             clean_start INTEGER NOT NULL,
             expiry_interval INTEGER,
             expires_at INTEGER,
             owner_username TEXT,
             owner_mount_point TEXT,
             PRIMARY KEY (mount_point, client_id)
         );
         INSERT INTO sessions_keyed
             (mount_point, client_id, subscriptions, clean_start, expiry_interval, expires_at,
              owner_username, owner_mount_point)
         SELECT COALESCE(owner_mount_point, ''), client_id, subscriptions, clean_start,
                expiry_interval, expires_at, owner_username, owner_mount_point
         FROM sessions WHERE client_id IS NOT NULL;
         DROP TABLE sessions;
         ALTER TABLE sessions_keyed RENAME TO sessions;",
    )
}

/// Brings the schema up to date.
fn migrate(conn: &mut Connection) -> Result<()> {
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(StorageError::database)?;
    let pending = usize::try_from(version)
        .ok()
        .and_then(|version| MIGRATIONS.get(version..).map(|m| (version, m)));
    let Some((version, pending)) = pending else {
        return Err(StorageError::OperationFailed(format!(
            "the database has schema version {version}, newer than this broker's {}; \
             it was written by a newer version of the broker",
            MIGRATIONS.len()
        )));
    };
    for (index, migration) in pending.iter().enumerate() {
        let applied = (|| {
            let tx = conn.transaction()?;
            migration(&tx)?;
            tx.pragma_update(None, "user_version", (version + index + 1) as i64)?;
            tx.commit()
        })();
        applied.map_err(StorageError::database)?;
    }
    Ok(())
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
/// lack: those from before schema versions, whose layout shows which
/// version wrote them.
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

            CREATE TABLE IF NOT EXISTS subscriptions (
                client_id TEXT NOT NULL,
                topic_filter TEXT NOT NULL,
                qos INTEGER NOT NULL,
                no_local INTEGER NOT NULL,
                retain_as_published INTEGER NOT NULL,
                subscription_identifier INTEGER,
                PRIMARY KEY (client_id, topic_filter)
            );

            CREATE TABLE IF NOT EXISTS incoming_qos2 (
                client_id TEXT NOT NULL,
                packet_id INTEGER NOT NULL,
                PRIMARY KEY (client_id, packet_id)
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

    move_subscriptions(conn)?;

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

/// Moves the subscriptions sessions kept encoded in their row, before
/// subscriptions had a table of their own, to that table. Those that can't
/// be read stay where they are, and the session with them, for an operator
/// to look at; see `load_session`.
fn move_subscriptions(conn: &Connection) -> rusqlite::Result<()> {
    let encoded: Vec<(String, String)> = conn
        .prepare("SELECT client_id, subscriptions FROM sessions WHERE subscriptions != ''")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (client_id, subscriptions) in encoded {
        let Ok(subscriptions) = decode_subscriptions(&subscriptions) else {
            continue;
        };
        // The table as it was before the mount point was part of its key
        for subscription in subscriptions {
            conn.execute(
                "INSERT OR IGNORE INTO subscriptions
                 (client_id, topic_filter, qos, no_local, retain_as_published,
                  subscription_identifier)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    client_id,
                    subscription.topic_filter,
                    qos_to_int(subscription.qos),
                    subscription.no_local,
                    subscription.retain_as_published,
                    subscription.subscription_identifier
                ],
            )?;
        }
        conn.execute(
            "UPDATE sessions SET subscriptions = '' WHERE client_id = ?1",
            params![client_id],
        )?;
    }
    Ok(())
}

/// Inserts or replaces one subscription row.
fn insert_subscription(
    conn: &Connection,
    key: &SessionKey,
    subscription: &StoredSubscription,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO subscriptions
         (mount_point, client_id, topic_filter, qos, no_local, retain_as_published,
          subscription_identifier)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            key.stored_mount_point(),
            key.client_id,
            subscription.topic_filter,
            qos_to_int(subscription.qos),
            subscription.no_local,
            subscription.retain_as_published,
            subscription.subscription_identifier
        ],
    )?;
    Ok(())
}

/// Fails like any other unreadable column if the stored properties can't
/// be decoded, rather than delivering the message without them.
fn decode_properties(encoded: Vec<u8>) -> rusqlite::Result<MessageProperties> {
    MessageProperties::decode(&mut Bytes::from(encoded)).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Blob, Box::new(e))
    })
}

/// Fails for a session whose subscriptions an older version kept in its row
/// and the move to their table couldn't read.
fn unmoved_subscriptions(encoded: String) -> rusqlite::Result<()> {
    if encoded.is_empty() {
        return Ok(());
    }
    let error = decode_subscriptions(&encoded).err().unwrap_or_else(|| {
        StorageError::SerializationError("subscriptions left in the session's row".into())
    });
    Err(rusqlite::Error::FromSqlConversionFailure(
        4,
        rusqlite::types::Type::Text,
        Box::new(error),
    ))
}

/// A stored QoS, which fails to read for a value no version stores.
struct Qos(QoS);

impl FromSql for Qos {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let value = i32::column_result(value)?;
        int_to_qos(value)
            .map(Qos)
            .map_err(|_| FromSqlError::OutOfRange(value.into()))
    }
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

/// The columns `session_from_row` reads.
const SESSION_COLUMNS: &str =
    "expiry_interval, expires_at, owner_username, owner_mount_point, subscriptions";

/// A session, without its subscriptions, which have a table of their own.
fn session_from_row(row: &rusqlite::Row, key: &SessionKey) -> rusqlite::Result<SessionState> {
    unmoved_subscriptions(row.get(4)?)?;
    Ok(SessionState {
        client_id: key.client_id.clone(),
        subscriptions: Vec::new(),
        expiry_interval: row.get(0)?,
        expires_at: row.get::<_, Option<i64>>(1)?.map(|t| t as u64),
        owner: SessionOwner {
            username: row.get(2)?,
            mount_point: row.get(3)?,
        },
    })
}

/// The columns `subscription_from_row` reads.
const SUBSCRIPTION_COLUMNS: &str =
    "topic_filter, qos, no_local, retain_as_published, subscription_identifier";

fn subscription_from_row(row: &rusqlite::Row) -> rusqlite::Result<StoredSubscription> {
    Ok(StoredSubscription {
        topic_filter: row.get(0)?,
        qos: row.get::<_, Qos>(1)?.0,
        no_local: row.get(2)?,
        retain_as_published: row.get(3)?,
        subscription_identifier: row.get(4)?,
    })
}

/// The columns `will_from_row` reads.
const WILL_COLUMNS: &str = "topic, payload, qos, retain, delay_secs, properties";

fn will_from_row(row: &rusqlite::Row) -> rusqlite::Result<StoredWillMessage> {
    let payload: Vec<u8> = row.get(1)?;
    Ok(StoredWillMessage {
        topic: row.get(0)?,
        payload: Bytes::from(payload),
        qos: row.get::<_, Qos>(2)?.0,
        retain: row.get(3)?,
        delay_secs: row.get(4)?,
        properties: decode_properties(row.get(5)?)?,
    })
}

/// The columns `queued_from_row` reads.
const QUEUED_COLUMNS: &str = "sequence, topic, payload, qos, retain, properties, expires_at,
    subscription_identifiers, share_group";

fn queued_from_row(row: &rusqlite::Row) -> rusqlite::Result<(u64, Message)> {
    let topic: String = row.get(1)?;
    let payload: Option<Vec<u8>> = row.get(2)?;
    let message = Message {
        packet_id: None,
        topic: Arc::from(topic.as_str()),
        dup: false,
        qos: row.get::<_, Qos>(3)?.0,
        retain: row.get(4)?,
        payload: payload.map(Bytes::from),
        properties: decode_properties(row.get(5)?)?,
        expires_at: row.get::<_, Option<i64>>(6)?.map(|t| t as u64),
        subscription_identifiers: row.get::<_, Identifiers>(7)?.0,
        origin: None,
        share_group: row.get::<_, Option<String>>(8)?.map(Arc::from),
    };
    // SQLite integers are signed; the bit pattern round-trips
    Ok((row.get::<_, i64>(0)? as u64, message))
}

/// The columns `inflight_from_row` reads.
const INFLIGHT_COLUMNS: &str = "packet_id, topic, payload, qos, dup, sequence, released,
    properties, subscription_identifiers, retain, expires_at, share_group";

fn inflight_from_row(row: &rusqlite::Row) -> rusqlite::Result<InflightMessage> {
    let payload: Option<Vec<u8>> = row.get(2)?;
    Ok(InflightMessage {
        packet_id: row.get(0)?,
        topic: row.get(1)?,
        payload: payload.map(Bytes::from),
        qos: row.get::<_, Qos>(3)?.0,
        dup: row.get(4)?,
        // SQLite integers are signed; the bit pattern round-trips
        sequence: row.get::<_, i64>(5)? as u64,
        released: row.get(6)?,
        properties: decode_properties(row.get(7)?)?,
        subscription_identifiers: row.get::<_, Identifiers>(8)?.0,
        retain: row.get(9)?,
        expires_at: row.get::<_, Option<i64>>(10)?.map(|t| t as u64),
        share_group: row.get(11)?,
    })
}

/// The retained lookup of `retained_page`: `?1` now, `?2` the topic or the
/// range's start, `?3` the topic to read after, `?4` the limit and `?5` the
/// range's end, if `bounded`.
fn retained_sql(exact: bool, bounded: bool) -> String {
    let mut sql = String::from(
        "SELECT topic, payload, qos, dup, properties, expires_at,
                origin_client_id, origin_username
         FROM retained_messages
         WHERE (expires_at IS NULL OR expires_at > ?1)",
    );
    sql.push_str(match exact {
        true => " AND topic = ?2 AND (?3 IS NULL OR topic > ?3)",
        false => " AND topic >= ?2 AND (?3 IS NULL OR topic > ?3)",
    });
    if bounded {
        sql.push_str(" AND topic < ?5");
    }
    sql.push_str(" ORDER BY topic LIMIT ?4");
    sql
}

/// Up to `limit` live retained messages matching `filter`, after `after`,
/// in topic order. Reads only the topics the filter's literal prefix allows,
/// through the primary key, in chunks, as wildcards past the prefix are
/// matched here.
fn retained_page(
    conn: &Connection,
    filter: &str,
    after: Option<String>,
    limit: usize,
) -> rusqlite::Result<Vec<Message>> {
    let (start, end) = match TopicRange::of(filter) {
        TopicRange::Exact(topic) => (topic, None),
        TopicRange::From { start, end } => (start, end),
    };
    let exact = !filter.contains(['+', '#']);
    let mut stmt = conn.prepare_cached(&retained_sql(exact, end.is_some()))?;
    let chunk = limit.max(256);
    let now = unix_millis() as i64;
    let mut cursor = after;
    let mut messages = Vec::new();
    loop {
        let mut read = 0;
        let mut last = None;
        let mut rows = match &end {
            Some(end) => stmt.query(params![now, start, cursor, chunk as i64, end])?,
            None => stmt.query(params![now, start, cursor, chunk as i64])?,
        };
        while let Some(row) = rows.next()? {
            read += 1;
            let topic: String = row.get(0)?;
            last = Some(topic.clone());
            if !topic_matches_filter(&topic, filter) {
                continue;
            }
            let payload: Option<Vec<u8>> = row.get(1)?;
            let origin_client_id: Option<String> = row.get(6)?;
            messages.push(Message {
                packet_id: None,
                topic: Arc::from(topic.as_str()),
                dup: row.get(3)?,
                qos: row.get::<_, Qos>(2)?.0,
                retain: true,
                payload: payload.map(Bytes::from),
                properties: decode_properties(row.get(4)?)?,
                expires_at: row.get::<_, Option<i64>>(5)?.map(|t| t as u64),
                subscription_identifiers: Vec::new(),
                origin: origin_client_id
                    .map(|client_id| -> rusqlite::Result<_> {
                        Ok(Arc::new(MessageOrigin {
                            client_id,
                            username: row.get(7)?,
                        }))
                    })
                    .transpose()?,
                share_group: None,
            });
            if messages.len() == limit {
                return Ok(messages);
            }
        }
        if read < chunk {
            return Ok(messages);
        }
        drop(rows);
        cursor = last;
    }
}

/// The rows of one client.
const KEY: &str = "mount_point = ?1 AND client_id = ?2";

/// Applies one change; the caller holds the transaction.
fn apply_write(conn: &Connection, key: &SessionKey, write: &Write) -> rusqlite::Result<()> {
    let (mount_point, client_id) = (key.stored_mount_point(), key.client_id.as_str());
    match write {
        Write::SaveSession(session) => {
            conn.execute(
                "INSERT OR REPLACE INTO sessions
                 (mount_point, client_id, subscriptions, clean_start, expiry_interval,
                  expires_at, owner_username, owner_mount_point)
                 VALUES (?1, ?2, '', ?3, ?4, ?5, ?6, ?7)",
                params![
                    mount_point,
                    client_id,
                    // No longer used; kept for databases that require it
                    false,
                    session.expiry_interval,
                    session.expires_at.map(|t| t as i64),
                    session.owner.username,
                    session.owner.mount_point
                ],
            )?;
            conn.execute(
                &format!("DELETE FROM subscriptions WHERE {KEY}"),
                params![mount_point, client_id],
            )?;
            for subscription in &session.subscriptions {
                insert_subscription(conn, key, subscription)?;
            }
        }
        Write::AddSubscription(subscription) => {
            let stored = conn
                .prepare_cached(&format!("SELECT 1 FROM sessions WHERE {KEY}"))?
                .exists(params![mount_point, client_id])?;
            if stored {
                insert_subscription(conn, key, subscription)?;
            }
        }
        Write::RemoveSubscription(topic_filter) => {
            conn.execute(
                &format!("DELETE FROM subscriptions WHERE {KEY} AND topic_filter = ?3"),
                params![mount_point, client_id, topic_filter],
            )?;
        }
        Write::SetExpiry {
            interval,
            expires_at,
        } => {
            conn.execute(
                &format!("UPDATE sessions SET expiry_interval = ?3, expires_at = ?4 WHERE {KEY}"),
                params![mount_point, client_id, interval, *expires_at as i64],
            )?;
        }
        Write::StoreWill(will) => {
            conn.execute(
                "INSERT OR REPLACE INTO will_messages
                 (mount_point, client_id, topic, payload, qos, retain, delay_secs, properties)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    mount_point,
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
                &format!("DELETE FROM will_messages WHERE {KEY}"),
                params![mount_point, client_id],
            )?;
        }
        Write::StoreInflight(message) => {
            conn.execute(
                "INSERT OR REPLACE INTO inflight_messages
                 (mount_point, client_id, packet_id, topic, payload, qos, dup, sequence, released,
                  properties, subscription_identifiers, retain, expires_at, share_group)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                params![
                    mount_point,
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
                    message.expires_at.map(|t| t as i64),
                    message.share_group
                ],
            )?;
        }
        Write::ReleaseInflight(packet_id) => {
            conn.execute(
                &format!(
                    "UPDATE inflight_messages SET released = 1, payload = NULL
                     WHERE {KEY} AND packet_id = ?3"
                ),
                params![mount_point, client_id, packet_id],
            )?;
        }
        Write::RemoveInflight(packet_id) => {
            conn.execute(
                &format!("DELETE FROM inflight_messages WHERE {KEY} AND packet_id = ?3"),
                params![mount_point, client_id, packet_id],
            )?;
        }
        Write::StoreIncomingQos2(packet_id) => {
            conn.execute(
                "INSERT OR IGNORE INTO incoming_qos2 (mount_point, client_id, packet_id)
                 VALUES (?1, ?2, ?3)",
                params![mount_point, client_id, packet_id],
            )?;
        }
        Write::RemoveIncomingQos2(packet_id) => {
            conn.execute(
                &format!("DELETE FROM incoming_qos2 WHERE {KEY} AND packet_id = ?3"),
                params![mount_point, client_id, packet_id],
            )?;
        }
        Write::Enqueue(sequence, message) => {
            conn.execute(
                "INSERT OR REPLACE INTO queued_messages
                 (mount_point, client_id, sequence, topic, payload, qos, retain, properties,
                  expires_at, subscription_identifiers, share_group)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    mount_point,
                    client_id,
                    *sequence as i64,
                    &*message.topic,
                    message.payload.as_deref(),
                    qos_to_int(message.qos),
                    message.retain,
                    encode_properties(&message.properties),
                    message.expires_at.map(|t| t as i64),
                    Identifiers(message.subscription_identifiers.clone()),
                    message.share_group.as_deref()
                ],
            )?;
        }
        Write::Dequeue(sequence) => {
            conn.execute(
                &format!("DELETE FROM queued_messages WHERE {KEY} AND sequence = ?3"),
                params![mount_point, client_id, *sequence as i64],
            )?;
        }
        Write::ClearQueue => {
            conn.execute(
                &format!("DELETE FROM queued_messages WHERE {KEY}"),
                params![mount_point, client_id],
            )?;
        }
        Write::DeleteSession => {
            for table in [
                "sessions",
                "subscriptions",
                "will_messages",
                "queued_messages",
                "inflight_messages",
                "incoming_qos2",
            ] {
                conn.execute(
                    &format!("DELETE FROM {table} WHERE {KEY}"),
                    params![mount_point, client_id],
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

    async fn apply(&self, key: &SessionKey, writes: &[Write]) -> Result<()> {
        let key = key.clone();
        let writes = writes.to_vec();
        self.blocking(move |conn| {
            // Rolls back on drop, also when COMMIT itself fails
            let tx = conn.unchecked_transaction()?;
            for write in &writes {
                apply_write(&tx, &key, write)?;
            }
            tx.commit()
        })
        .await
    }

    async fn load_session(&self, key: &SessionKey) -> Result<Option<SessionState>> {
        let key = key.clone();
        self.blocking(move |conn| {
            let params = params![key.stored_mount_point(), key.client_id];
            let Some(mut session) = conn
                .query_row(
                    &format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE {KEY}"),
                    params,
                    |row| session_from_row(row, &key),
                )
                .optional()?
            else {
                return Ok(None);
            };
            session.subscriptions = conn
                .prepare_cached(&format!(
                    "SELECT {SUBSCRIPTION_COLUMNS} FROM subscriptions WHERE {KEY}"
                ))?
                .query_map(params, subscription_from_row)?
                .collect::<rusqlite::Result<_>>()?;
            Ok(Some(session))
        })
        .await
    }

    async fn list_sessions(&self) -> Result<Vec<SessionKey>> {
        self.blocking(move |conn| {
            let mut stmt = conn.prepare("SELECT mount_point, client_id FROM sessions")?;
            let rows = stmt.query_map([], |row| {
                Ok(SessionKey::new(row.get(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect()
        })
        .await
    }

    async fn load_will(&self, key: &SessionKey) -> Result<Option<StoredWillMessage>> {
        let key = key.clone();
        self.blocking(move |conn| {
            conn.query_row(
                &format!("SELECT {WILL_COLUMNS} FROM will_messages WHERE {KEY}"),
                params![key.stored_mount_point(), key.client_id],
                will_from_row,
            )
            .optional()
        })
        .await
    }

    async fn load_inflight(&self, key: &SessionKey) -> Result<Vec<InflightMessage>> {
        let key = key.clone();
        self.blocking(move |conn| {
            conn.prepare_cached(&format!(
                "SELECT {INFLIGHT_COLUMNS} FROM inflight_messages WHERE {KEY}"
            ))?
            .query_map(
                params![key.stored_mount_point(), key.client_id],
                inflight_from_row,
            )?
            .collect()
        })
        .await
    }

    async fn load_incoming_qos2(&self, key: &SessionKey) -> Result<Vec<u16>> {
        let key = key.clone();
        self.blocking(move |conn| {
            conn.prepare_cached(&format!("SELECT packet_id FROM incoming_qos2 WHERE {KEY}"))?
                .query_map(params![key.stored_mount_point(), key.client_id], |row| {
                    row.get(0)
                })?
                .collect()
        })
        .await
    }

    async fn load_queue(&self, key: &SessionKey) -> Result<Vec<(u64, Message)>> {
        let key = key.clone();
        self.blocking(move |conn| {
            conn.prepare_cached(&format!(
                "SELECT {QUEUED_COLUMNS} FROM queued_messages WHERE {KEY} ORDER BY sequence"
            ))?
            .query_map(
                params![key.stored_mount_point(), key.client_id],
                queued_from_row,
            )?
            .collect()
        })
        .await
    }

    /// One query per table for the whole page: its sessions, then the rows
    /// of every other table in the range of keys the page covers.
    async fn load_clients(
        &self,
        after: Option<&SessionKey>,
        limit: usize,
    ) -> Result<Vec<LoadedClient>> {
        let after = after.cloned();
        let clients = self
            .blocking(move |conn| Ok(load_clients(conn, after, limit)))
            .await?;
        clients
    }
}

/// What `load_clients` gathers for each client of the page.
type Gathered = BTreeMap<SessionKey, Result<StoredClient>>;

/// The key of a row whose last two columns are its mount point and client
/// id.
fn key_from_row(row: &rusqlite::Row) -> rusqlite::Result<SessionKey> {
    let columns = row.as_ref().column_count();
    Ok(SessionKey::new(
        row.get(columns - 2)?,
        row.get::<_, String>(columns - 1)?,
    ))
}

/// Reads, for each row of `sql` over the page's range of keys, its key, the
/// last two columns, and passes `read` the row for that client's entry. A
/// row that can't be decoded fails its client's entry only.
fn gather<T>(
    conn: &Connection,
    sql: &str,
    (after, last): (&Option<SessionKey>, &SessionKey),
    clients: &mut Gathered,
    read: impl Fn(&rusqlite::Row) -> rusqlite::Result<T>,
    mut put: impl FnMut(&mut StoredClient, T),
) -> Result<()> {
    let mut stmt = conn.prepare_cached(sql).map_err(StorageError::database)?;
    let mut rows = stmt
        .query(params![
            after.as_ref().map(SessionKey::stored_mount_point),
            after.as_ref().map(|key| &key.client_id),
            last.stored_mount_point(),
            last.client_id
        ])
        .map_err(StorageError::database)?;
    while let Some(row) = rows.next().map_err(StorageError::database)? {
        let key = key_from_row(row).map_err(StorageError::database)?;
        // Rows of a client without a session are left out
        let Some(Ok(client)) = clients.get_mut(&key) else {
            continue;
        };
        match read(row).map_err(StorageError::database) {
            Ok(value) => put(client, value),
            Err(e @ StorageError::SerializationError(_)) => {
                clients.insert(key, Err(e));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Rows after the key `?1`, `?2`, if given; SQLite compares row values in
/// the order of the primary key, so the lookup reads it in that order.
const AFTER: &str = "(?1 IS NULL OR (mount_point, client_id) > (?1, ?2))";

fn load_clients(
    conn: &Connection,
    after: Option<SessionKey>,
    limit: usize,
) -> Result<Vec<LoadedClient>> {
    let mut clients = Gathered::new();
    {
        let mut stmt = conn
            .prepare_cached(&format!(
                "SELECT {SESSION_COLUMNS}, mount_point, client_id FROM sessions
                 WHERE {AFTER} ORDER BY mount_point, client_id LIMIT ?3"
            ))
            .map_err(StorageError::database)?;
        let mut rows = stmt
            .query(params![
                after.as_ref().map(SessionKey::stored_mount_point),
                after.as_ref().map(|key| &key.client_id),
                limit as i64
            ])
            .map_err(StorageError::database)?;
        while let Some(row) = rows.next().map_err(StorageError::database)? {
            let key = key_from_row(row).map_err(StorageError::database)?;
            let stored = match session_from_row(row, &key).map_err(StorageError::database) {
                Ok(session) => Ok(StoredClient {
                    session,
                    will: None,
                    queue: Vec::new(),
                    inflight: Vec::new(),
                    incoming_qos2: Vec::new(),
                }),
                Err(e @ StorageError::SerializationError(_)) => Err(e),
                Err(e) => return Err(e),
            };
            clients.insert(key, stored);
        }
    }
    let Some(last) = clients.keys().next_back().cloned() else {
        return Ok(Vec::new());
    };
    let range = (&after, &last);
    let within = format!(
        "WHERE {AFTER} AND (mount_point, client_id) <= (?3, ?4)
         ORDER BY mount_point, client_id"
    );
    let key = "mount_point, client_id";
    gather(
        conn,
        &format!("SELECT {SUBSCRIPTION_COLUMNS}, {key} FROM subscriptions {within}"),
        range,
        &mut clients,
        subscription_from_row,
        |client, subscription| client.session.subscriptions.push(subscription),
    )?;
    gather(
        conn,
        &format!("SELECT {WILL_COLUMNS}, {key} FROM will_messages {within}"),
        range,
        &mut clients,
        will_from_row,
        |client, will| client.will = Some(will),
    )?;
    gather(
        conn,
        &format!("SELECT {QUEUED_COLUMNS}, {key} FROM queued_messages {within}, sequence"),
        range,
        &mut clients,
        queued_from_row,
        |client, queued| client.queue.push(queued),
    )?;
    gather(
        conn,
        &format!("SELECT {INFLIGHT_COLUMNS}, {key} FROM inflight_messages {within}"),
        range,
        &mut clients,
        inflight_from_row,
        |client, message| client.inflight.push(message),
    )?;
    gather(
        conn,
        &format!("SELECT packet_id, {key} FROM incoming_qos2 {within}"),
        range,
        &mut clients,
        |row| row.get::<_, u16>(0),
        |client, packet_id| client.incoming_qos2.push(packet_id),
    )?;
    Ok(clients
        .into_iter()
        .map(|(key, stored)| LoadedClient { key, stored })
        .collect())
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
impl RetainedMessageStore for SqliteStore {
    async fn store_retained(&self, topic: &str, message: Option<Message>) -> Result<bool> {
        let topic = topic.to_string();

        self.blocking(move |conn| {
            // The one connection is held, so nothing changes in between
            let existed = conn
                .prepare_cached("SELECT 1 FROM retained_messages WHERE topic = ?1")?
                .exists(params![topic])?;
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
            Ok(existed)
        })
        .await
    }

    async fn get_retained_page(
        &self,
        topic_filter: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Message>> {
        let filter = topic_filter.to_string();
        let after = after.map(str::to_string);
        self.blocking(move |conn| retained_page(conn, &filter, after, limit))
            .await
    }

    async fn remove_expired_retained(&self) -> Result<usize> {
        self.blocking(move |conn| {
            conn.execute(
                "DELETE FROM retained_messages WHERE expires_at <= ?1",
                params![unix_millis() as i64],
            )
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

#[cfg(test)]
mod tests {
    use mercurio_core::qos::QoS;

    use super::*;
    use crate::testing::{key, message};

    /// A database file of this test only, removed when dropped.
    struct TempPath(std::path::PathBuf);

    impl TempPath {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("mercurio-{name}-{}.db", std::process::id()));
            let path = TempPath(path);
            path.remove();
            path
        }

        fn remove(&self) {
            for suffix in ["", "-wal", "-shm", ".lock"] {
                let mut name = self.0.clone().into_os_string();
                name.push(suffix);
                let _ = std::fs::remove_file(name);
            }
        }
    }

    impl Drop for TempPath {
        fn drop(&mut self) {
            self.remove();
        }
    }

    impl AsRef<Path> for TempPath {
        fn as_ref(&self) -> &Path {
            &self.0
        }
    }

    fn session(client_id: &str) -> SessionState {
        SessionState {
            client_id: client_id.to_string(),
            subscriptions: Vec::new(),
            expiry_interval: None,
            expires_at: None,
            owner: Default::default(),
        }
    }

    #[tokio::test]
    async fn test_conformance() {
        crate::testing::conformance(&SqliteStore::in_memory().unwrap()).await;
    }

    #[tokio::test]
    async fn test_conformance_on_disk() {
        let path = TempPath::new("conformance");
        crate::testing::conformance(&SqliteStore::new(&path).unwrap()).await;
    }

    #[tokio::test]
    async fn test_bans() {
        let store = Arc::new(SqliteStore::in_memory().unwrap());
        crate::testing::bans(store.as_ref()).await;
        assert!(store.ban_store().is_some());
    }

    #[tokio::test]
    async fn test_bans_survive_reopening() {
        let path = TempPath::new("store-bans");
        let ban = crate::testing::ban("username", "guest", true);
        SqliteStore::new(&path)
            .unwrap()
            .store_ban(&ban)
            .await
            .unwrap();

        let reopened = SqliteStore::new(&path).unwrap();
        assert_eq!(reopened.load_bans().await.unwrap(), vec![ban]);
    }

    #[test]
    fn test_backend_is_named() {
        assert_eq!(SqliteStore::in_memory().unwrap().backend_name(), "sqlite");
    }

    #[tokio::test]
    async fn test_older_databases_gain_the_owner_columns() {
        let path = TempPath::new("owner");
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
        let loaded = store.load_session(&key("old")).await.unwrap().unwrap();
        // Sessions from before owners were kept count as anonymous ones
        assert_eq!(loaded.owner, SessionOwner::default());
    }

    #[tokio::test]
    async fn test_older_in_flight_messages_load_without_retain_or_expiry() {
        let path = TempPath::new("inflight");
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
        let loaded = store.load_inflight(&key("c")).await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!((loaded[0].packet_id, loaded[0].sequence), (4, 9));
        assert!(!loaded[0].retain);
        assert_eq!(loaded[0].expires_at, None);
    }

    #[test]
    fn test_older_databases_gain_the_origin_columns() {
        let path = TempPath::new("migrate");
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

    /// Sessions of databases from before subscriptions had a table of
    /// their own keep their subscriptions.
    #[tokio::test]
    async fn test_encoded_subscriptions_move_to_their_table() {
        let path = TempPath::new("subs");
        let subscriptions = vec![
            StoredSubscription {
                no_local: true,
                retain_as_published: true,
                subscription_identifier: Some(268_435_455),
                ..StoredSubscription::new("topic/+:x,y", QoS::ExactlyOnce)
            },
            StoredSubscription::new("$share/g/a/#", QoS::AtMostOnce),
        ];
        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "CREATE TABLE sessions (
                client_id TEXT PRIMARY KEY, subscriptions TEXT NOT NULL,
                clean_start INTEGER NOT NULL, expiry_interval INTEGER, expires_at INTEGER,
                owner_username TEXT, owner_mount_point TEXT
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions VALUES ('old', ?1, 0, 60, NULL, 'alice', NULL),
                                         ('none', '', 0, 60, NULL, NULL, NULL)",
            params![crate::encoding::encode_subscriptions(&subscriptions)],
        )
        .unwrap();
        drop(conn);

        let store = SqliteStore::new(&path).unwrap();
        let mut loaded = store.load_session(&key("old")).await.unwrap().unwrap();
        loaded
            .subscriptions
            .sort_by(|a, b| b.topic_filter.cmp(&a.topic_filter));
        assert_eq!(loaded.subscriptions, subscriptions);
        assert_eq!(loaded.owner.username.as_deref(), Some("alice"));
        let none = store.load_session(&key("none")).await.unwrap().unwrap();
        assert!(none.subscriptions.is_empty());
        drop(store);

        // Moved once: opening again finds them where they were put
        let store = SqliteStore::new(&path).unwrap();
        let blobs: i64 = store
            .blocking(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM sessions WHERE subscriptions != ''",
                    [],
                    |row| row.get(0),
                )
            })
            .await
            .unwrap();
        assert_eq!(blobs, 0);
        let loaded = store.load_session(&key("old")).await.unwrap().unwrap();
        assert_eq!(loaded.subscriptions.len(), 2);
    }

    #[tokio::test]
    async fn test_expired_retained_messages_are_removed() {
        let store = SqliteStore::in_memory().unwrap();
        let mut expired = message("old", "x");
        expired.expires_at = Some(1);
        store.store_retained("old", Some(expired)).await.unwrap();
        store
            .store_retained("new", Some(message("new", "y")))
            .await
            .unwrap();

        // Lookups leave them out, and leave them be
        let retained = store.get_retained("#").await.unwrap();
        assert_eq!(retained.len(), 1);
        assert_eq!(&*retained[0].topic, "new");
        assert_eq!(store.count_retained().await.unwrap(), 2);

        assert_eq!(store.remove_expired_retained().await.unwrap(), 1);
        assert_eq!(store.count_retained().await.unwrap(), 1);
    }

    /// How SQLite plans `sql`, one line per step.
    fn plan(conn: &Connection, sql: &str) -> String {
        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let unbound = std::iter::repeat_n(Null, stmt.parameter_count());
        let steps: Vec<String> = stmt
            .query_map(rusqlite::params_from_iter(unbound), |row| row.get(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        steps.join("\n")
    }

    /// Lookups read the topics their filter allows through the primary
    /// key, and the removal of expired messages finds them by their index.
    #[tokio::test]
    async fn test_retained_lookups_use_the_indexes() {
        let store = SqliteStore::in_memory().unwrap();
        let conn = store.conn.lock().unwrap();
        for (exact, bounded) in [(true, false), (false, true), (false, false)] {
            let plan = plan(&conn, &retained_sql(exact, bounded));
            assert!(
                plan.contains("USING INDEX sqlite_autoindex_retained_messages_1"),
                "{plan}"
            );
            assert!(!plan.contains("TEMP B-TREE"), "{plan}");
        }
        let exact = plan(&conn, &retained_sql(true, false));
        assert!(exact.contains("(topic=?)"), "{exact}");
        let bounded = plan(&conn, &retained_sql(false, true));
        assert!(bounded.contains("(topic>? AND topic<?)"), "{bounded}");
        let removal = plan(&conn, "DELETE FROM retained_messages WHERE expires_at <= 5");
        assert!(
            removal.contains("retained_messages_expires_at"),
            "{removal}"
        );
    }

    #[tokio::test]
    async fn test_readers_do_not_block_writes() {
        let path = TempPath::new("wal");
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
            .store_retained("a", Some(message("a", "1")))
            .await
            .unwrap();
        reader.execute_batch("COMMIT").unwrap();
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
        let path = TempPath::new("commit");
        let store = SqliteStore::new(&path).unwrap();
        {
            let conn = store.conn.lock().unwrap();
            // A reader blocks COMMIT only under the rollback journal
            conn.pragma_update(None, "journal_mode", "DELETE").unwrap();
            conn.busy_timeout(std::time::Duration::ZERO).unwrap();
        }
        let save = |client_id: &'static str| {
            let store = store.clone();
            async move {
                store
                    .apply(&key(client_id), &[Write::SaveSession(session(client_id))])
                    .await
            }
        };

        let reader = Connection::open(&path).unwrap();
        reader.execute_batch("BEGIN").unwrap();
        reader
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap();
        assert!(save("blocked").await.is_err());
        reader.execute_batch("COMMIT").unwrap();

        save("after").await.unwrap();
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
    }

    fn is_undecodable<T: std::fmt::Debug>(result: Result<T>) -> bool {
        matches!(result, Err(StorageError::SerializationError(_)))
    }

    /// Rows that can't be read fail to load as undecodable, each in the
    /// loads of its own client only, and stay in the database.
    #[tokio::test]
    async fn test_rows_that_cannot_be_read_are_an_error() {
        let path = TempPath::new("undecodable");
        let store = SqliteStore::new(&path).unwrap();
        for client_id in ["bad", "good"] {
            store
                .apply(
                    &key(client_id),
                    &[
                        Write::SaveSession(crate::testing::session(client_id)),
                        Write::StoreInflight(crate::testing::inflight_message(1)),
                        Write::Enqueue(1, message("q", "1")),
                        Write::StoreWill(StoredWillMessage {
                            topic: "w".into(),
                            payload: Bytes::new(),
                            qos: QoS::AtMostOnce,
                            retain: false,
                            delay_secs: 0,
                            properties: Default::default(),
                        }),
                    ],
                )
                .await
                .unwrap();
        }
        Connection::open(&path)
            .unwrap()
            .execute_batch(
                "UPDATE subscriptions SET qos = 7 WHERE client_id = 'bad';
                 UPDATE inflight_messages SET subscription_identifiers = 'x' WHERE client_id = 'bad';
                 UPDATE queued_messages SET properties = x'ff' WHERE client_id = 'bad';
                 UPDATE will_messages SET qos = 'text' WHERE client_id = 'bad';",
            )
            .unwrap();

        assert!(is_undecodable(store.load_session(&key("bad")).await));
        assert!(is_undecodable(store.load_inflight(&key("bad")).await));
        assert!(is_undecodable(store.load_queue(&key("bad")).await));
        assert!(is_undecodable(store.load_will(&key("bad")).await));
        assert!(store.load_session(&key("good")).await.unwrap().is_some());
        assert_eq!(store.load_inflight(&key("good")).await.unwrap().len(), 1);
        assert_eq!(store.load_queue(&key("good")).await.unwrap().len(), 1);
        assert!(store.load_will(&key("good")).await.unwrap().is_some());
        let loaded = store.load_clients(None, 10).await.unwrap();
        assert_eq!(loaded.len(), 2);
        assert!(matches!(
            loaded[0].stored,
            Err(StorageError::SerializationError(_))
        ));
        assert_eq!(loaded[0].key, key("bad"));
        assert!(loaded[1].stored.is_ok());

        // Deleting the session removes the rows that can't be read too
        store.delete_session(&key("bad")).await.unwrap();
        assert!(store.load_session(&key("bad")).await.unwrap().is_none());
        assert!(store.load_queue(&key("bad")).await.unwrap().is_empty());
    }

    /// Subscriptions an older version kept in the session's row that can't
    /// be read stay there, and the session fails to load.
    #[tokio::test]
    async fn test_encoded_subscriptions_that_cannot_be_read_stay() {
        let path = TempPath::new("bad-subs");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                client_id TEXT PRIMARY KEY, subscriptions TEXT NOT NULL,
                clean_start INTEGER NOT NULL, expiry_interval INTEGER, expires_at INTEGER,
                owner_username TEXT, owner_mount_point TEXT
            );
            INSERT INTO sessions VALUES ('bad', '9xx:a', 0, 60, NULL, NULL, NULL),
                                        ('good', '100:a', 0, 60, NULL, NULL, NULL);",
        )
        .unwrap();
        drop(conn);

        let store = SqliteStore::new(&path).unwrap();
        assert!(is_undecodable(store.load_session(&key("bad")).await));
        let good = store.load_session(&key("good")).await.unwrap().unwrap();
        assert_eq!(
            good.subscriptions,
            [StoredSubscription::new("a", QoS::AtLeastOnce)]
        );
        let kept: String = Connection::open(&path)
            .unwrap()
            .query_row(
                "SELECT subscriptions FROM sessions WHERE client_id = 'bad'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(kept, "9xx:a");
    }

    fn user_version(path: &TempPath) -> i64 {
        Connection::open(path)
            .unwrap()
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn test_the_schema_version_is_recorded() {
        let path = TempPath::new("version");
        drop(SqliteStore::new(&path).unwrap());
        assert_eq!(user_version(&path), MIGRATIONS.len() as i64);
        // Opening again changes nothing
        drop(SqliteStore::new(&path).unwrap());
        assert_eq!(user_version(&path), MIGRATIONS.len() as i64);
    }

    #[test]
    fn test_a_newer_schema_is_refused() {
        let path = TempPath::new("newer");
        drop(SqliteStore::new(&path).unwrap());
        let newer = MIGRATIONS.len() as i64 + 1;
        Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", newer)
            .unwrap();

        let error = SqliteStore::new(&path).err().unwrap().to_string();
        assert!(error.contains("newer"), "{error}");
        assert_eq!(user_version(&path), newer);
    }

    /// A database from before schema versions, at the layout of the
    /// latest release without one, is migrated and keeps its rows.
    #[tokio::test]
    async fn test_an_unversioned_database_is_migrated() {
        let path = TempPath::new("unversioned");
        drop(SqliteStore::new(&path).unwrap());
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", 0).unwrap();
        conn.execute(
            "INSERT INTO sessions (client_id, subscriptions, clean_start) VALUES ('c', '', 0)",
            [],
        )
        .unwrap();
        drop(conn);

        let store = SqliteStore::new(&path).unwrap();
        assert!(store.load_session(&key("c")).await.unwrap().is_some());
        drop(store);
        assert_eq!(user_version(&path), MIGRATIONS.len() as i64);
    }

    #[test]
    fn test_two_stores_never_share_a_database() {
        let path = TempPath::new("exclusive");
        let first = SqliteStore::new(&path).unwrap();
        let error = SqliteStore::new(&path).err().unwrap().to_string();
        assert!(error.contains("another"), "{error}");

        // Readers, such as an operator's sqlite3 shell, still can
        let reader =
            Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let sessions: i64 = reader
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(sessions, 0);
        let copy = path.0.with_extension("copy");
        let _ = std::fs::remove_file(&copy);
        reader
            .execute("VACUUM INTO ?1", params![copy.to_str().unwrap()])
            .unwrap();
        std::fs::remove_file(&copy).unwrap();
        drop(reader);

        drop(first);
        SqliteStore::new(&path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn test_a_new_database_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let path = TempPath::new("private");
        drop(SqliteStore::new(&path).unwrap());
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    /// A database written before the mount point was part of the key moves
    /// each session's rows under the mount point of its owner, and keeps
    /// rows without a session under none.
    #[tokio::test]
    async fn test_rows_move_under_their_sessions_mount_point() {
        let path = TempPath::new("mount-points");
        let mut conn = Connection::open(&path).unwrap();
        for migration in &MIGRATIONS[..2] {
            migration(&conn).unwrap();
        }
        conn.pragma_update(None, "user_version", 2).unwrap();
        let tx = conn.transaction().unwrap();
        tx.execute_batch(
            "INSERT INTO sessions VALUES
                 ('a', '', 0, 60, NULL, 'alice', 'tenant-a/'),
                 ('b', '', 0, NULL, NULL, 'bob', NULL);
             INSERT INTO subscriptions VALUES ('a', 's/a', 1, 0, 0, NULL), ('b', 's/b', 0, 0, 0, NULL);
             INSERT INTO will_messages VALUES ('a', 'w/a', x'', 0, 0, 0, x'');
             INSERT INTO inflight_messages
                 (client_id, packet_id, topic, payload, qos, dup, sequence, released, properties)
                 VALUES ('a', 1, 'i/a', NULL, 1, 0, 1, 0, x''),
                        ('orphan', 1, 'i/o', NULL, 1, 0, 1, 0, x'');
             INSERT INTO queued_messages
                 (client_id, sequence, topic, payload, qos, retain, properties)
                 VALUES ('a', 4, 'q/a', NULL, 1, 0, x'');
             INSERT INTO incoming_qos2 VALUES ('a', 7), ('orphan', 8);",
        )
        .unwrap();
        tx.commit().unwrap();
        drop(conn);

        let store = SqliteStore::new(&path).unwrap();
        let mounted = SessionKey::new(Some("tenant-a/".into()), "a");
        assert!(store.load_session(&key("a")).await.unwrap().is_none());
        let a = store.load_client(&mounted).await.unwrap().unwrap();
        assert_eq!(a.session.owner.username.as_deref(), Some("alice"));
        assert_eq!(a.session.subscriptions[0].topic_filter, "s/a");
        assert_eq!(a.will.unwrap().topic, "w/a");
        assert_eq!(a.inflight[0].topic, "i/a");
        assert_eq!(a.queue[0].0, 4);
        assert_eq!(a.incoming_qos2, [7]);
        let b = store.load_client(&key("b")).await.unwrap().unwrap();
        assert_eq!(b.session.subscriptions[0].topic_filter, "s/b");
        assert_eq!(store.load_inflight(&key("orphan")).await.unwrap().len(), 1);
        assert_eq!(store.load_incoming_qos2(&key("orphan")).await.unwrap(), [8]);
        let mut listed = store.list_sessions().await.unwrap();
        listed.sort();
        assert_eq!(listed, [key("b"), mounted.clone()]);

        // The same client id now has a session of its own on another mount
        // point
        store
            .apply(&key("a"), &[Write::SaveSession(session("a"))])
            .await
            .unwrap();
        assert_eq!(store.load_clients(None, 10).await.unwrap().len(), 3);
        drop(store);
        assert_eq!(user_version(&path), MIGRATIONS.len() as i64);
    }

    #[tokio::test]
    async fn test_persistence_across_instances() {
        let path = TempPath::new("persistence");
        let stored = SessionState {
            subscriptions: vec![StoredSubscription::new("topic/test", QoS::AtMostOnce)],
            ..session("persistent_client")
        };
        SqliteStore::new(&path)
            .unwrap()
            .apply(
                &key("persistent_client"),
                &[Write::SaveSession(stored.clone())],
            )
            .await
            .unwrap();

        let store = SqliteStore::new(&path).unwrap();
        assert_eq!(
            store.load_session(&key("persistent_client")).await.unwrap(),
            Some(stored)
        );
    }
}
