//! PostgreSQL storage backend.
//!
//! Keeps the broker's state in a PostgreSQL database, for deployments that
//! already run one or want the state off the broker's host.

use std::{path::Path, sync::Arc, time::Duration};

use async_trait::async_trait;
use bytes::Bytes;
use deadpool_postgres::{Manager, Object, Pool, Runtime};
use mercurio_core::{
    codec::Decoder,
    message::{unix_millis, Message, MessageOrigin, MessageProperties},
    topic::topic_matches_filter,
};
use rustls::{
    pki_types::{pem::PemObject, CertificateDer},
    ClientConfig, RootCertStore,
};
use tokio::sync::watch;
use tokio_postgres::{types::ToSql, Row};
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::{
    encoding::{
        decode_subscriptions, encode_properties, encode_subscriptions, int_to_qos, qos_to_int,
    },
    InflightMessage, InflightStore, QueueStore, Result, RetainedMessageStore, SessionState,
    SessionStore, StorageError, StoredWillMessage, WillStore,
};

const SCHEMA: &str = "
    SET client_min_messages = warning;
    -- CREATE TABLE IF NOT EXISTS fails when two brokers run it at once
    BEGIN;
    SELECT pg_advisory_xact_lock(7882832494279682414);

    CREATE TABLE IF NOT EXISTS sessions (
        client_id TEXT PRIMARY KEY,
        subscriptions BYTEA NOT NULL,
        clean_start BOOLEAN NOT NULL,
        expiry_interval BIGINT,
        expires_at BIGINT
    );

    CREATE TABLE IF NOT EXISTS retained_messages (
        topic TEXT PRIMARY KEY,
        payload BYTEA,
        qos INTEGER NOT NULL,
        dup BOOLEAN NOT NULL,
        properties BYTEA NOT NULL,
        expires_at BIGINT,
        origin_client_id TEXT,
        origin_username TEXT
    );

    CREATE TABLE IF NOT EXISTS will_messages (
        client_id TEXT PRIMARY KEY,
        topic TEXT NOT NULL,
        payload BYTEA NOT NULL,
        qos INTEGER NOT NULL,
        retain BOOLEAN NOT NULL,
        delay_secs BIGINT NOT NULL,
        properties BYTEA NOT NULL
    );

    CREATE TABLE IF NOT EXISTS inflight_messages (
        client_id TEXT NOT NULL,
        packet_id INTEGER NOT NULL,
        topic TEXT NOT NULL,
        payload BYTEA,
        qos INTEGER NOT NULL,
        dup BOOLEAN NOT NULL,
        sequence BIGINT NOT NULL,
        released BOOLEAN NOT NULL,
        properties BYTEA NOT NULL,
        subscription_identifiers BIGINT[] NOT NULL,
        PRIMARY KEY (client_id, packet_id)
    );

    CREATE TABLE IF NOT EXISTS queued_messages (
        client_id TEXT NOT NULL,
        sequence BIGINT NOT NULL,
        topic TEXT NOT NULL,
        payload BYTEA,
        qos INTEGER NOT NULL,
        retain BOOLEAN NOT NULL,
        properties BYTEA NOT NULL,
        expires_at BIGINT,
        subscription_identifiers BIGINT[] NOT NULL,
        PRIMARY KEY (client_id, sequence)
    );
    COMMIT;
";

/// PostgreSQL storage backend implementing all storage traits, over a pool
/// of connections that are reopened when the server drops them.
#[derive(Clone)]
pub struct PostgresStore {
    pool: Pool,
    config: tokio_postgres::Config,
    tls: MakeRustlsConnect,
}

/// Advisory lock key that brokers sharing a database take, "mercurio" in
/// ASCII. The schema is created under the key before it.
// ponytail: one key per database, make it configurable if several broker groups share one
const LOCK_KEY: i64 = 0x6d65_7263_7572_696f;

/// The database, held by one broker at a time; see [`PostgresStore::lock`].
/// Dropping it releases the database.
pub struct DatabaseLock {
    _client: tokio_postgres::Client,
    lost: watch::Receiver<bool>,
}

impl DatabaseLock {
    /// Completes if the connection holding the lock closes, after which
    /// another broker may hold it: this one should stop serving.
    pub async fn lost(&self) {
        let _ = self.lost.clone().wait_for(|lost| *lost).await;
    }

    pub fn is_lost(&self) -> bool {
        *self.lost.borrow()
    }
}

impl PostgresStore {
    /// Connect to the database at `url`, such as
    /// `postgres://user:password@localhost/mercurio`, and create the tables
    /// if they don't exist.
    ///
    /// The connection is encrypted when the server supports it, or always
    /// with `?sslmode=require` in the URL, and never with `sslmode=disable`.
    /// The server's certificate is verified against `ca_path`, a PEM file,
    /// or else the Mozilla root certificates.
    pub async fn connect(url: &str, ca_path: Option<&Path>) -> Result<Self> {
        let config: tokio_postgres::Config = url.parse()?;
        let tls = tls(ca_path)?;
        let pool = Pool::builder(Manager::new(config.clone(), tls.clone()))
            .runtime(Runtime::Tokio1)
            .build()
            .map_err(|e| StorageError::OperationFailed(e.to_string()))?;
        let store = PostgresStore { pool, config, tls };
        store.client().await?.batch_execute(SCHEMA).await?;
        Ok(store)
    }

    /// Take the database for this broker, or `None` if another broker
    /// holds it. Brokers sharing a database would overwrite each other's
    /// sessions, so only the holder should serve clients.
    pub async fn try_lock(&self) -> Result<Option<DatabaseLock>> {
        self.take_lock("SELECT pg_try_advisory_lock($1)").await
    }

    /// Wait until the database is free and take it, to stand by while
    /// another broker serves and take over once it stops.
    pub async fn lock(&self) -> Result<DatabaseLock> {
        let lock = self
            .take_lock("SELECT true FROM pg_advisory_lock($1)")
            .await?;
        Ok(lock.expect("pg_advisory_lock returns once the lock is held"))
    }

    /// The lock belongs to a connection of its own, outside the pool, so
    /// it lasts as long as that connection.
    async fn take_lock(&self, sql: &str) -> Result<Option<DatabaseLock>> {
        let mut config = self.config.clone();
        // Notice a vanished database within seconds, as a standby may take over
        config
            .keepalives_idle(Duration::from_secs(10))
            .keepalives_interval(Duration::from_secs(5))
            .keepalives_retries(3);
        let (client, connection) = config.connect(self.tls.clone()).await?;
        let (lost_tx, lost) = watch::channel(false);
        tokio::spawn(async move {
            let _ = connection.await;
            let _ = lost_tx.send(true);
        });
        let held: bool = client.query_one(sql, &[&LOCK_KEY]).await?.try_get(0)?;
        Ok(held.then_some(DatabaseLock {
            _client: client,
            lost,
        }))
    }

    async fn client(&self) -> Result<Object> {
        self.pool.get().await.map_err(|e| {
            // The cause, such as why a certificate was refused, is in the sources
            let mut message = e.to_string();
            let mut source = std::error::Error::source(&e);
            while let Some(cause) = source {
                let cause_text = cause.to_string();
                if !message.contains(&cause_text) {
                    message = format!("{message}: {cause_text}");
                }
                source = cause.source();
            }
            StorageError::OperationFailed(message)
        })
    }

    async fn execute(&self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<u64> {
        let client = self.client().await?;
        let statement = client.prepare_cached(sql).await?;
        Ok(client.execute(&statement, params).await?)
    }

    async fn query(&self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<Vec<Row>> {
        let client = self.client().await?;
        let statement = client.prepare_cached(sql).await?;
        Ok(client.query(&statement, params).await?)
    }
}

fn tls(ca_path: Option<&Path>) -> Result<MakeRustlsConnect> {
    let tls_error = |e: String| StorageError::OperationFailed(format!("PostgreSQL TLS: {e}"));
    let roots = match ca_path {
        Some(path) => {
            let mut roots = RootCertStore::empty();
            for cert in CertificateDer::pem_file_iter(path)
                .map_err(|e| tls_error(format!("{}: {e}", path.display())))?
            {
                roots
                    .add(cert.map_err(|e| tls_error(format!("{}: {e}", path.display())))?)
                    .map_err(|e| tls_error(e.to_string()))?;
            }
            if roots.is_empty() {
                return Err(tls_error(format!("no certificates in {}", path.display())));
            }
            roots
        }
        None => RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
    };
    // Named, since rustls can't pick a default when two providers are enabled
    let config = ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| tls_error(e.to_string()))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(MakeRustlsConnect::new(config))
}

/// Fails if the stored properties can't be decoded, rather than delivering
/// the message without them.
fn decode_properties(encoded: Vec<u8>) -> Result<MessageProperties> {
    MessageProperties::decode(&mut Bytes::from(encoded))
        .map_err(|e| StorageError::SerializationError(e.to_string()))
}

fn identifiers(ids: &[u32]) -> Vec<i64> {
    ids.iter().map(|&id| id.into()).collect()
}

fn decode_identifiers(ids: Vec<i64>) -> Vec<u32> {
    ids.into_iter().map(|id| id as u32).collect()
}

fn inflight_from_row(row: &Row) -> Result<InflightMessage> {
    Ok(InflightMessage {
        packet_id: row.try_get::<_, i32>("packet_id")? as u16,
        topic: row.try_get("topic")?,
        payload: row
            .try_get::<_, Option<Vec<u8>>>("payload")?
            .map(Bytes::from),
        qos: int_to_qos(row.try_get("qos")?),
        dup: row.try_get("dup")?,
        sequence: row.try_get::<_, i64>("sequence")? as u64,
        released: row.try_get("released")?,
        properties: decode_properties(row.try_get("properties")?)?,
        subscription_identifiers: decode_identifiers(row.try_get("subscription_identifiers")?),
    })
}

#[async_trait]
impl SessionStore for PostgresStore {
    async fn save_session(&self, client_id: &str, session: &SessionState) -> Result<()> {
        self.execute(
            "INSERT INTO sessions
             (client_id, subscriptions, clean_start, expiry_interval, expires_at)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (client_id) DO UPDATE SET
             subscriptions = EXCLUDED.subscriptions, clean_start = EXCLUDED.clean_start,
             expiry_interval = EXCLUDED.expiry_interval, expires_at = EXCLUDED.expires_at",
            &[
                &client_id,
                &encode_subscriptions(&session.subscriptions).into_bytes(),
                &session.clean_start,
                &session.expiry_interval.map(i64::from),
                &session.expires_at.map(|t| t as i64),
            ],
        )
        .await?;
        Ok(())
    }

    async fn load_session(&self, client_id: &str) -> Result<Option<SessionState>> {
        let rows = self
            .query(
                "SELECT subscriptions, clean_start, expiry_interval, expires_at
                 FROM sessions WHERE client_id = $1",
                &[&client_id],
            )
            .await?;
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        let subscriptions = String::from_utf8(row.try_get("subscriptions")?)
            .map_err(|e| StorageError::SerializationError(e.to_string()))?;
        Ok(Some(SessionState {
            client_id: client_id.to_string(),
            subscriptions: decode_subscriptions(&subscriptions),
            clean_start: row.try_get("clean_start")?,
            expiry_interval: row
                .try_get::<_, Option<i64>>("expiry_interval")?
                .map(|i| i as u32),
            expires_at: row
                .try_get::<_, Option<i64>>("expires_at")?
                .map(|t| t as u64),
        }))
    }

    async fn delete_session(&self, client_id: &str) -> Result<()> {
        self.execute("DELETE FROM sessions WHERE client_id = $1", &[&client_id])
            .await?;
        Ok(())
    }

    async fn list_sessions(&self) -> Result<Vec<String>> {
        self.query("SELECT client_id FROM sessions", &[])
            .await?
            .iter()
            .map(|row| Ok(row.try_get(0)?))
            .collect()
    }
}

#[async_trait]
impl RetainedMessageStore for PostgresStore {
    async fn store_retained(&self, topic: &str, message: Option<Message>) -> Result<()> {
        let Some(msg) = message else {
            self.execute("DELETE FROM retained_messages WHERE topic = $1", &[&topic])
                .await?;
            return Ok(());
        };
        let origin = msg.origin.as_deref();
        self.execute(
            "INSERT INTO retained_messages
             (topic, payload, qos, dup, properties, expires_at, origin_client_id, origin_username)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (topic) DO UPDATE SET
             payload = EXCLUDED.payload, qos = EXCLUDED.qos, dup = EXCLUDED.dup,
             properties = EXCLUDED.properties, expires_at = EXCLUDED.expires_at,
             origin_client_id = EXCLUDED.origin_client_id,
             origin_username = EXCLUDED.origin_username",
            &[
                &topic,
                &msg.payload.as_deref(),
                &qos_to_int(msg.qos),
                &msg.dup,
                &encode_properties(&msg.properties),
                &msg.expires_at.map(|t| t as i64),
                &origin.map(|o| o.client_id.as_str()),
                &origin.and_then(|o| o.username.as_deref()),
            ],
        )
        .await?;
        Ok(())
    }

    async fn get_retained(&self, topic_filter: &str) -> Result<Vec<Message>> {
        // Expired messages are dropped for good, not just skipped
        self.execute(
            "DELETE FROM retained_messages WHERE expires_at <= $1",
            &[&(unix_millis() as i64)],
        )
        .await?;

        // ponytail: matches every retained topic in Rust, push the filter into SQL if this gets slow
        let rows = self
            .query(
                "SELECT topic, payload, qos, dup, properties, expires_at,
                        origin_client_id, origin_username
                 FROM retained_messages",
                &[],
            )
            .await?;

        let mut messages = Vec::new();
        for row in rows {
            let topic: String = row.try_get("topic")?;
            if !topic_matches_filter(&topic, topic_filter) {
                continue;
            }
            let origin = row
                .try_get::<_, Option<String>>("origin_client_id")?
                .map(|client_id| -> Result<_> {
                    Ok(Arc::new(MessageOrigin {
                        client_id,
                        username: row.try_get("origin_username")?,
                    }))
                })
                .transpose()?;
            messages.push(Message {
                packet_id: None,
                topic: Arc::from(topic.as_str()),
                dup: row.try_get("dup")?,
                qos: int_to_qos(row.try_get("qos")?),
                retain: true,
                payload: row
                    .try_get::<_, Option<Vec<u8>>>("payload")?
                    .map(Bytes::from),
                properties: decode_properties(row.try_get("properties")?)?,
                expires_at: row
                    .try_get::<_, Option<i64>>("expires_at")?
                    .map(|t| t as u64),
                subscription_identifiers: Vec::new(),
                origin,
            });
        }
        Ok(messages)
    }

    async fn clear_all_retained(&self) -> Result<()> {
        self.execute("DELETE FROM retained_messages", &[]).await?;
        Ok(())
    }

    async fn count_retained(&self) -> Result<usize> {
        let rows = self
            .query("SELECT COUNT(*) FROM retained_messages", &[])
            .await?;
        Ok(rows[0].try_get::<_, i64>(0)? as usize)
    }
}

#[async_trait]
impl WillStore for PostgresStore {
    async fn store_will(&self, client_id: &str, will: &StoredWillMessage) -> Result<()> {
        self.execute(
            "INSERT INTO will_messages
             (client_id, topic, payload, qos, retain, delay_secs, properties)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (client_id) DO UPDATE SET
             topic = EXCLUDED.topic, payload = EXCLUDED.payload, qos = EXCLUDED.qos,
             retain = EXCLUDED.retain, delay_secs = EXCLUDED.delay_secs,
             properties = EXCLUDED.properties",
            &[
                &client_id,
                &will.topic,
                &&will.payload[..],
                &qos_to_int(will.qos),
                &will.retain,
                &i64::from(will.delay_secs),
                &encode_properties(&will.properties),
            ],
        )
        .await?;
        Ok(())
    }

    async fn get_will(&self, client_id: &str) -> Result<Option<StoredWillMessage>> {
        let rows = self
            .query(
                "SELECT topic, payload, qos, retain, delay_secs, properties
                 FROM will_messages WHERE client_id = $1",
                &[&client_id],
            )
            .await?;
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        Ok(Some(StoredWillMessage {
            topic: row.try_get("topic")?,
            payload: Bytes::from(row.try_get::<_, Vec<u8>>("payload")?),
            qos: int_to_qos(row.try_get("qos")?),
            retain: row.try_get("retain")?,
            delay_secs: row.try_get::<_, i64>("delay_secs")? as u32,
            properties: decode_properties(row.try_get("properties")?)?,
        }))
    }

    async fn delete_will(&self, client_id: &str) -> Result<()> {
        self.execute(
            "DELETE FROM will_messages WHERE client_id = $1",
            &[&client_id],
        )
        .await?;
        Ok(())
    }
}

#[async_trait]
impl InflightStore for PostgresStore {
    async fn store_inflight(
        &self,
        client_id: &str,
        packet_id: u16,
        message: &InflightMessage,
    ) -> Result<()> {
        self.execute(
            "INSERT INTO inflight_messages
             (client_id, packet_id, topic, payload, qos, dup, sequence, released, properties,
              subscription_identifiers)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
             ON CONFLICT (client_id, packet_id) DO UPDATE SET
             topic = EXCLUDED.topic, payload = EXCLUDED.payload, qos = EXCLUDED.qos,
             dup = EXCLUDED.dup, sequence = EXCLUDED.sequence, released = EXCLUDED.released,
             properties = EXCLUDED.properties,
             subscription_identifiers = EXCLUDED.subscription_identifiers",
            &[
                &client_id,
                &i32::from(packet_id),
                &message.topic,
                &message.payload.as_deref(),
                &qos_to_int(message.qos),
                &message.dup,
                &(message.sequence as i64),
                &message.released,
                &encode_properties(&message.properties),
                &identifiers(&message.subscription_identifiers),
            ],
        )
        .await?;
        Ok(())
    }

    async fn get_inflight(
        &self,
        client_id: &str,
        packet_id: u16,
    ) -> Result<Option<InflightMessage>> {
        self.query(
            "SELECT * FROM inflight_messages WHERE client_id = $1 AND packet_id = $2",
            &[&client_id, &i32::from(packet_id)],
        )
        .await?
        .first()
        .map(inflight_from_row)
        .transpose()
    }

    async fn remove_inflight(&self, client_id: &str, packet_id: u16) -> Result<()> {
        self.execute(
            "DELETE FROM inflight_messages WHERE client_id = $1 AND packet_id = $2",
            &[&client_id, &i32::from(packet_id)],
        )
        .await?;
        Ok(())
    }

    async fn release_inflight(&self, client_id: &str, packet_id: u16) -> Result<()> {
        self.execute(
            "UPDATE inflight_messages SET released = TRUE, payload = NULL
             WHERE client_id = $1 AND packet_id = $2",
            &[&client_id, &i32::from(packet_id)],
        )
        .await?;
        Ok(())
    }

    async fn get_all_inflight(&self, client_id: &str) -> Result<Vec<InflightMessage>> {
        self.query(
            "SELECT * FROM inflight_messages WHERE client_id = $1",
            &[&client_id],
        )
        .await?
        .iter()
        .map(inflight_from_row)
        .collect()
    }
}

#[async_trait]
impl QueueStore for PostgresStore {
    async fn enqueue(&self, client_id: &str, sequence: u64, message: &Message) -> Result<()> {
        self.execute(
            "INSERT INTO queued_messages
             (client_id, sequence, topic, payload, qos, retain, properties, expires_at,
              subscription_identifiers)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             ON CONFLICT (client_id, sequence) DO UPDATE SET
             topic = EXCLUDED.topic, payload = EXCLUDED.payload, qos = EXCLUDED.qos,
             retain = EXCLUDED.retain, properties = EXCLUDED.properties,
             expires_at = EXCLUDED.expires_at,
             subscription_identifiers = EXCLUDED.subscription_identifiers",
            &[
                &client_id,
                &(sequence as i64),
                &&*message.topic,
                &message.payload.as_deref(),
                &qos_to_int(message.qos),
                &message.retain,
                &encode_properties(&message.properties),
                &message.expires_at.map(|t| t as i64),
                &identifiers(&message.subscription_identifiers),
            ],
        )
        .await?;
        Ok(())
    }

    async fn load_queue(&self, client_id: &str) -> Result<Vec<(u64, Message)>> {
        self.query(
            "SELECT * FROM queued_messages WHERE client_id = $1 ORDER BY sequence",
            &[&client_id],
        )
        .await?
        .iter()
        .map(|row| {
            let topic: String = row.try_get("topic")?;
            let message = Message {
                packet_id: None,
                topic: Arc::from(topic.as_str()),
                dup: false,
                qos: int_to_qos(row.try_get("qos")?),
                retain: row.try_get("retain")?,
                payload: row
                    .try_get::<_, Option<Vec<u8>>>("payload")?
                    .map(Bytes::from),
                properties: decode_properties(row.try_get("properties")?)?,
                expires_at: row
                    .try_get::<_, Option<i64>>("expires_at")?
                    .map(|t| t as u64),
                subscription_identifiers: decode_identifiers(
                    row.try_get("subscription_identifiers")?,
                ),
                origin: None,
            };
            Ok((row.try_get::<_, i64>("sequence")? as u64, message))
        })
        .collect()
    }

    async fn clear_queue(&self, client_id: &str) -> Result<()> {
        self.execute(
            "DELETE FROM queued_messages WHERE client_id = $1",
            &[&client_id],
        )
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use mercurio_core::{properties::ResponseTopic, qos::QoS};

    use super::*;
    use crate::StoredSubscription;

    fn message(topic: &str, payload: &str) -> Message {
        Message {
            packet_id: None,
            topic: Arc::from(topic),
            dup: false,
            qos: QoS::AtLeastOnce,
            retain: false,
            payload: Some(Bytes::from(payload.to_string())),
            properties: Default::default(),
            expires_at: None,
            subscription_identifiers: Vec::new(),
            origin: None,
        }
    }

    #[tokio::test]
    async fn test_one_broker_holds_the_database() {
        let Ok(url) = std::env::var("MERCURIO_TEST_POSTGRES_URL") else {
            eprintln!("MERCURIO_TEST_POSTGRES_URL not set, skipping");
            return;
        };
        let (active, standby) = tokio::join!(
            PostgresStore::connect(&url, None),
            PostgresStore::connect(&url, None)
        );
        let (active, standby) = (active.unwrap(), standby.unwrap());

        let lock = active.try_lock().await.unwrap().unwrap();
        assert!(standby.try_lock().await.unwrap().is_none());
        let waiting = tokio::spawn(async move { standby.lock().await.unwrap() });
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!waiting.is_finished());
        drop(lock);
        let lock = tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("the standby takes over")
            .unwrap();
        assert!(!lock.is_lost());

        // The server ends the holder's connection, as on a failover
        active
            .client()
            .await
            .unwrap()
            .execute(
                "SELECT pg_terminate_backend(pid) FROM pg_locks
                 WHERE locktype = 'advisory' AND granted
                 AND (classid::bigint << 32 | objid::bigint) = $1",
                &[&LOCK_KEY],
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), lock.lost())
            .await
            .expect("losing the lock is noticed");
        assert!(lock.is_lost());
    }

    /// Runs against the database in `MERCURIO_TEST_POSTGRES_URL`, and is
    /// skipped without one. It owns the database: tables are emptied first.
    #[tokio::test]
    async fn test_round_trips_every_store() {
        let Ok(url) = std::env::var("MERCURIO_TEST_POSTGRES_URL") else {
            eprintln!("MERCURIO_TEST_POSTGRES_URL not set, skipping");
            return;
        };
        let store = PostgresStore::connect(&url, None).await.unwrap();
        store
            .client()
            .await
            .unwrap()
            .batch_execute(
                "TRUNCATE sessions, retained_messages, will_messages, inflight_messages,
                 queued_messages",
            )
            .await
            .unwrap();

        let mut subscription = StoredSubscription::new("a/#", QoS::ExactlyOnce);
        subscription.subscription_identifier = Some(268_435_455);
        let session = SessionState {
            client_id: "c".into(),
            subscriptions: vec![
                subscription.clone(),
                StoredSubscription::new("b", QoS::AtMostOnce),
            ],
            clean_start: false,
            expiry_interval: Some(u32::MAX),
            expires_at: Some(1),
        };
        store.save_session("c", &session).await.unwrap();
        store.save_session("c", &session).await.unwrap();
        let loaded = store.load_session("c").await.unwrap().unwrap();
        assert_eq!(loaded.subscriptions[0], subscription);
        assert_eq!(loaded.subscriptions.len(), 2);
        assert_eq!(loaded.expiry_interval, Some(u32::MAX));
        assert_eq!(store.list_sessions().await.unwrap(), vec!["c".to_string()]);
        store.delete_session("c").await.unwrap();
        assert!(store.load_session("c").await.unwrap().is_none());

        let mut retained = message("a/b", "kept");
        retained.properties.response_topic = Some(ResponseTopic::new("reply".into()));
        retained.origin = Some(Arc::new(MessageOrigin {
            client_id: "c".into(),
            username: None,
        }));
        store
            .store_retained("a/b", Some(retained.clone()))
            .await
            .unwrap();
        let mut expired = message("a/c", "gone");
        expired.expires_at = Some(1);
        store.store_retained("a/c", Some(expired)).await.unwrap();
        let found = store.get_retained("a/+").await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].payload, retained.payload);
        assert_eq!(found[0].properties, retained.properties);
        assert_eq!(found[0].origin.as_ref().unwrap().client_id, "c");
        assert_eq!(store.count_retained().await.unwrap(), 1);
        store.store_retained("a/b", None).await.unwrap();
        assert_eq!(store.count_retained().await.unwrap(), 0);

        let will = StoredWillMessage {
            topic: "w".into(),
            payload: Bytes::from_static(b"bye"),
            qos: QoS::AtLeastOnce,
            retain: true,
            delay_secs: u32::MAX,
            properties: Default::default(),
        };
        store.store_will("c", &will).await.unwrap();
        let loaded = store.get_will("c").await.unwrap().unwrap();
        assert_eq!(
            (loaded.payload, loaded.delay_secs),
            (will.payload, u32::MAX)
        );
        store.delete_will("c").await.unwrap();
        assert!(store.get_will("c").await.unwrap().is_none());

        let inflight = InflightMessage {
            packet_id: u16::MAX,
            topic: "t".into(),
            payload: Some(Bytes::from_static(b"p")),
            qos: QoS::ExactlyOnce,
            dup: false,
            sequence: u64::MAX,
            released: false,
            properties: Default::default(),
            subscription_identifiers: vec![1, u32::MAX],
        };
        store
            .store_inflight("c", u16::MAX, &inflight)
            .await
            .unwrap();
        store.release_inflight("c", u16::MAX).await.unwrap();
        let loaded = store.get_inflight("c", u16::MAX).await.unwrap().unwrap();
        assert!(loaded.released && loaded.payload.is_none());
        assert_eq!(loaded.sequence, u64::MAX);
        assert_eq!(loaded.subscription_identifiers, vec![1, u32::MAX]);
        assert_eq!(store.get_all_inflight("c").await.unwrap().len(), 1);
        store.remove_inflight("c", u16::MAX).await.unwrap();
        assert!(store.get_all_inflight("c").await.unwrap().is_empty());

        let mut second = message("q", "2");
        second.subscription_identifiers = vec![4];
        store.enqueue("c", 2, &second).await.unwrap();
        store.enqueue("c", 1, &message("q", "1")).await.unwrap();
        let queue = store.load_queue("c").await.unwrap();
        assert_eq!(queue.iter().map(|(s, _)| *s).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(queue[1].1.subscription_identifiers, vec![4]);
        store.clear_queue("c").await.unwrap();
        assert!(store.load_queue("c").await.unwrap().is_empty());
    }
}
