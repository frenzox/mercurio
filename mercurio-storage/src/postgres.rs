//! PostgreSQL storage backend.
//!
//! Keeps the broker's state in a PostgreSQL database, 12 or later, for
//! deployments that already run one or want the state off the broker's host.

use std::{
    collections::{BTreeMap, HashMap},
    net::IpAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use bytes::Bytes;
use deadpool_postgres::{Manager, Object, Pool, PoolError, Runtime};
use futures_util::future::join_all;
use mercurio_core::{
    codec::Decoder,
    message::{unix_millis, Message, MessageOrigin, MessageProperties},
    topic::topic_matches_filter,
};
use rustls::{
    pki_types::{pem::PemObject, CertificateDer},
    ClientConfig, RootCertStore,
};
use tokio::{sync::watch, task::JoinHandle};
use tokio_postgres::{
    config::{Host, SslMode},
    error::SqlState,
    types::ToSql,
    Row,
};
use tokio_postgres_rustls::MakeRustlsConnect;
use tracing::{info, warn};

use crate::{
    encoding::{decode_subscriptions, encode_properties, int_to_qos, qos_to_int},
    BanStore, BatchStore, InflightMessage, LoadedClient, Result, RetainedMessageStore, SessionKey,
    SessionOwner, SessionState, StorageError, StoredBan, StoredClient, StoredSubscription,
    StoredWillMessage, TopicRange, Write,
};

/// Longest wait for a connection or a statement, so a database that stops
/// answering fails the broker's calls instead of stalling them.
// ponytail: fixed, make it configurable when a deployment needs another value
const TIMEOUT: Duration = Duration::from_secs(5);

/// How often the broker holding the database checks it still does.
const HEARTBEAT: Duration = Duration::from_secs(2);

/// Advisory lock key that brokers sharing a database take, "mercurio" in
/// ASCII.
// ponytail: one key per database, make it configurable if several broker groups share one
const LOCK_KEY: i64 = 0x6d65_7263_7572_696f;

/// Advisory lock key the schema is migrated under.
const SCHEMA_LOCK_KEY: i64 = LOCK_KEY - 1;

/// Schema changes, applied in order; the database records how many it has.
/// Released entries never change: a new schema change is a new entry, written
/// so that applying it again is harmless.
const MIGRATIONS: &[&str] = &[
    "
    CREATE TABLE sessions (
        client_id TEXT PRIMARY KEY,
        subscriptions BYTEA NOT NULL,
        clean_start BOOLEAN NOT NULL,
        expiry_interval BIGINT,
        expires_at BIGINT
    );

    -- The C collation lets the primary key index serve topic prefix searches
    CREATE TABLE retained_messages (
        topic TEXT COLLATE \"C\" PRIMARY KEY,
        payload BYTEA,
        qos INTEGER NOT NULL,
        dup BOOLEAN NOT NULL,
        properties BYTEA NOT NULL,
        expires_at BIGINT,
        origin_client_id TEXT,
        origin_username TEXT
    );

    CREATE TABLE will_messages (
        client_id TEXT PRIMARY KEY,
        topic TEXT NOT NULL,
        payload BYTEA NOT NULL,
        qos INTEGER NOT NULL,
        retain BOOLEAN NOT NULL,
        delay_secs BIGINT NOT NULL,
        properties BYTEA NOT NULL
    );

    CREATE TABLE inflight_messages (
        client_id TEXT NOT NULL,
        packet_id INTEGER NOT NULL,
        topic TEXT NOT NULL,
        payload BYTEA,
        qos INTEGER NOT NULL,
        dup BOOLEAN NOT NULL,
        sequence BIGINT NOT NULL,
        released BOOLEAN NOT NULL,
        properties BYTEA NOT NULL,
        -- Comma-separated, so batches can insert them with unnest
        subscription_identifiers TEXT,
        PRIMARY KEY (client_id, packet_id)
    );

    CREATE TABLE queued_messages (
        client_id TEXT NOT NULL,
        sequence BIGINT NOT NULL,
        topic TEXT NOT NULL,
        payload BYTEA,
        qos INTEGER NOT NULL,
        retain BOOLEAN NOT NULL,
        properties BYTEA NOT NULL,
        expires_at BIGINT,
        -- Comma-separated, so batches can insert them with unnest
        subscription_identifiers TEXT,
        PRIMARY KEY (client_id, sequence)
    );

    -- Incremented by each broker that takes the database over. A broker
    -- that held it before writes under an older epoch, and the triggers
    -- below refuse its writes, however late they reach the database.
    CREATE TABLE mercurio_lease (epoch BIGINT NOT NULL);
    INSERT INTO mercurio_lease VALUES (0);

    -- The key share lock makes a takeover wait for writes already checked.
    -- It lasts until the transaction ends, so one check per transaction
    -- covers all its writes.
    CREATE FUNCTION mercurio_fence() RETURNS trigger LANGUAGE plpgsql AS $$
    DECLARE
        held TEXT := current_setting('mercurio.epoch', true);
    BEGIN
        IF held IS NULL OR held = ''
            OR current_setting('mercurio.fenced', true) = 'yes' THEN
            RETURN NULL;
        END IF;
        PERFORM 1 FROM mercurio_lease WHERE epoch = held::BIGINT FOR KEY SHARE;
        IF NOT FOUND THEN
            RAISE EXCEPTION 'another broker took over the database';
        END IF;
        PERFORM set_config('mercurio.fenced', 'yes', true);
        RETURN NULL;
    END
    $$;

    CREATE TRIGGER fence BEFORE INSERT OR UPDATE OR DELETE OR TRUNCATE ON sessions
        FOR EACH STATEMENT EXECUTE FUNCTION mercurio_fence();
    CREATE TRIGGER fence BEFORE INSERT OR UPDATE OR DELETE OR TRUNCATE ON retained_messages
        FOR EACH STATEMENT EXECUTE FUNCTION mercurio_fence();
    CREATE TRIGGER fence BEFORE INSERT OR UPDATE OR DELETE OR TRUNCATE ON will_messages
        FOR EACH STATEMENT EXECUTE FUNCTION mercurio_fence();
    CREATE TRIGGER fence BEFORE INSERT OR UPDATE OR DELETE OR TRUNCATE ON inflight_messages
        FOR EACH STATEMENT EXECUTE FUNCTION mercurio_fence();
    CREATE TRIGGER fence BEFORE INSERT OR UPDATE OR DELETE OR TRUNCATE ON queued_messages
        FOR EACH STATEMENT EXECUTE FUNCTION mercurio_fence();
",
    "
    ALTER TABLE sessions ADD COLUMN owner_username TEXT;
    ALTER TABLE sessions ADD COLUMN owner_mount_point TEXT;
",
    "
    CREATE TABLE bans (
        kind TEXT NOT NULL,
        value TEXT NOT NULL,
        prefix BOOLEAN NOT NULL,
        reason TEXT,
        created_at BIGINT NOT NULL,
        expires_at BIGINT,
        PRIMARY KEY (kind, value, prefix)
    );

    CREATE TRIGGER fence BEFORE INSERT OR UPDATE OR DELETE OR TRUNCATE ON bans
        FOR EACH STATEMENT EXECUTE FUNCTION mercurio_fence();
",
    // Fenced writes fail with a code of their own, FENCED
    "
    CREATE OR REPLACE FUNCTION mercurio_fence() RETURNS trigger LANGUAGE plpgsql AS $$
    DECLARE
        held TEXT := current_setting('mercurio.epoch', true);
    BEGIN
        IF held IS NULL OR held = ''
            OR current_setting('mercurio.fenced', true) = 'yes' THEN
            RETURN NULL;
        END IF;
        PERFORM 1 FROM mercurio_lease WHERE epoch = held::BIGINT FOR KEY SHARE;
        IF NOT FOUND THEN
            RAISE EXCEPTION 'another broker took over the database'
                USING ERRCODE = 'MQ001';
        END IF;
        PERFORM set_config('mercurio.fenced', 'yes', true);
        RETURN NULL;
    END
    $$;
",
    "
    ALTER TABLE inflight_messages ADD COLUMN IF NOT EXISTS retain BOOLEAN NOT NULL DEFAULT FALSE;
    ALTER TABLE inflight_messages ADD COLUMN IF NOT EXISTS expires_at BIGINT;
",
    "
    CREATE TABLE IF NOT EXISTS incoming_qos2 (
        client_id TEXT NOT NULL,
        packet_id INTEGER NOT NULL,
        PRIMARY KEY (client_id, packet_id)
    );

    DROP TRIGGER IF EXISTS fence ON incoming_qos2;
    CREATE TRIGGER fence BEFORE INSERT OR UPDATE OR DELETE OR TRUNCATE ON incoming_qos2
        FOR EACH STATEMENT EXECUTE FUNCTION mercurio_fence();
",
    // Followed by `move_subscriptions`
    "
    CREATE TABLE IF NOT EXISTS subscriptions (
        client_id TEXT NOT NULL,
        topic_filter TEXT NOT NULL,
        qos INTEGER NOT NULL,
        no_local BOOLEAN NOT NULL,
        retain_as_published BOOLEAN NOT NULL,
        subscription_identifier BIGINT,
        PRIMARY KEY (client_id, topic_filter)
    );

    DROP TRIGGER IF EXISTS fence ON subscriptions;
    CREATE TRIGGER fence BEFORE INSERT OR UPDATE OR DELETE OR TRUNCATE ON subscriptions
        FOR EACH STATEMENT EXECUTE FUNCTION mercurio_fence();
",
    // Lets the removal of expired retained messages find them without
    // reading the table
    "
    CREATE INDEX IF NOT EXISTS retained_messages_expires_at
        ON retained_messages (expires_at) WHERE expires_at IS NOT NULL;
",
    // The mount point becomes part of the key of every table of a client's
    // state, so the same client id on two mount points keeps two sessions.
    // Rows take the mount point of their session's owner, and those without
    // a session none, an empty string. Done once, while the primary key of
    // sessions lacks the mount point: run again, the rows of a client id
    // with sessions on two mount points would match both.
    "
    DO $$
    BEGIN
        IF EXISTS (
            SELECT 1 FROM pg_index i
            JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY (i.indkey)
            WHERE i.indrelid = 'sessions'::regclass AND i.indisprimary
                AND a.attname = 'mount_point'
        ) THEN
            RETURN;
        END IF;

        ALTER TABLE sessions ADD COLUMN IF NOT EXISTS mount_point TEXT NOT NULL DEFAULT '';
        UPDATE sessions SET mount_point = COALESCE(owner_mount_point, '');

        ALTER TABLE subscriptions ADD COLUMN IF NOT EXISTS mount_point TEXT NOT NULL DEFAULT '';
        UPDATE subscriptions t SET mount_point = s.mount_point
            FROM sessions s WHERE s.client_id = t.client_id;
        ALTER TABLE will_messages ADD COLUMN IF NOT EXISTS mount_point TEXT NOT NULL DEFAULT '';
        UPDATE will_messages t SET mount_point = s.mount_point
            FROM sessions s WHERE s.client_id = t.client_id;
        ALTER TABLE queued_messages ADD COLUMN IF NOT EXISTS mount_point TEXT NOT NULL DEFAULT '';
        UPDATE queued_messages t SET mount_point = s.mount_point
            FROM sessions s WHERE s.client_id = t.client_id;
        ALTER TABLE inflight_messages ADD COLUMN IF NOT EXISTS mount_point TEXT NOT NULL DEFAULT '';
        UPDATE inflight_messages t SET mount_point = s.mount_point
            FROM sessions s WHERE s.client_id = t.client_id;
        ALTER TABLE incoming_qos2 ADD COLUMN IF NOT EXISTS mount_point TEXT NOT NULL DEFAULT '';
        UPDATE incoming_qos2 t SET mount_point = s.mount_point
            FROM sessions s WHERE s.client_id = t.client_id;

        ALTER TABLE sessions DROP CONSTRAINT IF EXISTS sessions_pkey,
            ADD PRIMARY KEY (mount_point, client_id);
        ALTER TABLE subscriptions DROP CONSTRAINT IF EXISTS subscriptions_pkey,
            ADD PRIMARY KEY (mount_point, client_id, topic_filter);
        ALTER TABLE will_messages DROP CONSTRAINT IF EXISTS will_messages_pkey,
            ADD PRIMARY KEY (mount_point, client_id);
        ALTER TABLE queued_messages DROP CONSTRAINT IF EXISTS queued_messages_pkey,
            ADD PRIMARY KEY (mount_point, client_id, sequence);
        ALTER TABLE inflight_messages DROP CONSTRAINT IF EXISTS inflight_messages_pkey,
            ADD PRIMARY KEY (mount_point, client_id, packet_id);
        ALTER TABLE incoming_qos2 DROP CONSTRAINT IF EXISTS incoming_qos2_pkey,
            ADD PRIMARY KEY (mount_point, client_id, packet_id);
    END
    $$;
",
    // Which shared subscription a queued or in-flight message came through,
    // so it can go to another member of the group
    "
    ALTER TABLE queued_messages ADD COLUMN IF NOT EXISTS share_group TEXT;
    ALTER TABLE inflight_messages ADD COLUMN IF NOT EXISTS share_group TEXT;
",
];

/// The migration that creates the subscriptions table, after which
/// `move_subscriptions` runs.
const SUBSCRIPTIONS_TABLE: usize = 6;

/// The SQLSTATE the fence triggers refuse a write with.
const FENCED: &str = "MQ001";

/// Brings the schema up to date, one broker at a time.
async fn migrate(client: &tokio_postgres::Client) -> Result<()> {
    client
        .batch_execute(&format!(
            "SET client_min_messages = warning;
             BEGIN;
             SELECT pg_advisory_xact_lock({SCHEMA_LOCK_KEY});
             CREATE TABLE IF NOT EXISTS mercurio_schema (version INTEGER NOT NULL);"
        ))
        .await
        .db()?;
    let result = async {
        let version: i32 = match client
            .query_opt("SELECT version FROM mercurio_schema", &[])
            .await
            .db()?
        {
            Some(row) => row.try_get(0).db()?,
            None => {
                client
                    .execute("INSERT INTO mercurio_schema VALUES (0)", &[])
                    .await
                    .db()?;
                0
            }
        };
        let pending = MIGRATIONS
            .get(version as usize..)
            .ok_or_else(|| newer_schema(version))?;
        for (index, migration) in pending.iter().enumerate() {
            client.batch_execute(migration).await.db()?;
            if version as usize + index == SUBSCRIPTIONS_TABLE {
                move_subscriptions(client).await?;
            }
        }
        client
            .execute(
                "UPDATE mercurio_schema SET version = $1",
                &[&(MIGRATIONS.len() as i32)],
            )
            .await
            .db()?;
        Ok(())
    }
    .await;
    client
        .batch_execute(if result.is_ok() { "COMMIT" } else { "ROLLBACK" })
        .await
        .db()?;
    result
}

/// Moves the subscriptions sessions kept encoded in their row, before
/// subscriptions had a table of their own, to that table. Those that can't
/// be read stay where they are, and the session with them, for an operator
/// to look at; see `load_session`.
async fn move_subscriptions(client: &tokio_postgres::Client) -> Result<()> {
    let sessions = client
        .query(
            "SELECT client_id, subscriptions FROM sessions WHERE length(subscriptions) > 0",
            &[],
        )
        .await
        .db()?;
    for row in sessions {
        let client_id: String = row.try_get(0).db()?;
        let encoded: Vec<u8> = row.try_get(1).db()?;
        let Some(subscriptions) = String::from_utf8(encoded)
            .ok()
            .and_then(|encoded| decode_subscriptions(&encoded).ok())
        else {
            continue;
        };
        // The table as it was before the mount point was part of its key
        client
            .execute(
                "INSERT INTO subscriptions
                 (client_id, topic_filter, qos, no_local, retain_as_published,
                  subscription_identifier)
                 SELECT $1, * FROM unnest($2::TEXT[], $3::INTEGER[], $4::BOOLEAN[],
                                          $5::BOOLEAN[], $6::BIGINT[])
                 ON CONFLICT DO NOTHING",
                &[
                    &client_id,
                    &subscriptions
                        .iter()
                        .map(|s| s.topic_filter.clone())
                        .collect::<Vec<_>>(),
                    &subscriptions
                        .iter()
                        .map(|s| qos_to_int(s.qos))
                        .collect::<Vec<_>>(),
                    &subscriptions.iter().map(|s| s.no_local).collect::<Vec<_>>(),
                    &subscriptions
                        .iter()
                        .map(|s| s.retain_as_published)
                        .collect::<Vec<_>>(),
                    &subscriptions
                        .iter()
                        .map(|s| s.subscription_identifier.map(i64::from))
                        .collect::<Vec<_>>(),
                ],
            )
            .await
            .db()?;
        client
            .execute(
                "UPDATE sessions SET subscriptions = ''::BYTEA WHERE client_id = $1",
                &[&client_id],
            )
            .await
            .db()?;
    }
    Ok(())
}

fn newer_schema(version: i32) -> StorageError {
    StorageError::OperationFailed(format!(
        "the database has schema version {version}, newer than this broker's {}",
        MIGRATIONS.len()
    ))
}

/// Fails if the database's schema is newer than this broker knows, without
/// changing it.
async fn check_schema(client: &tokio_postgres::Client) -> Result<()> {
    let exists: bool = client
        .query_one("SELECT to_regclass('mercurio_schema') IS NOT NULL", &[])
        .await
        .db()?
        .try_get(0)
        .db()?;
    if !exists {
        return Ok(());
    }
    if let Some(row) = client
        .query_opt("SELECT version FROM mercurio_schema", &[])
        .await
        .db()?
    {
        let version: i32 = row.try_get(0).db()?;
        if version as usize > MIGRATIONS.len() {
            return Err(newer_schema(version));
        }
    }
    Ok(())
}

/// Turns tokio-postgres errors into the crate's, keeping its types out of
/// the API.
trait Db<T> {
    fn db(self) -> Result<T>;
}

impl<T> Db<T> for std::result::Result<T, tokio_postgres::Error> {
    fn db(self) -> Result<T> {
        self.map_err(classify)
    }
}

/// Turns the errors of reading a column into the crate's: a stored value
/// that can't be read as what it stands for.
trait Col<T> {
    fn col(self) -> Result<T>;
}

impl<T> Col<T> for std::result::Result<T, tokio_postgres::Error> {
    fn col(self) -> Result<T> {
        self.map_err(|e| StorageError::SerializationError(crate::error_chain(&e)))
    }
}

/// `value` read as a narrower integer, failing for one out of its range.
fn narrow<T: TryFrom<i64>>(value: impl Into<i64>, what: &str) -> Result<T> {
    let value = value.into();
    T::try_from(value)
        .map_err(|_| StorageError::SerializationError(format!("{value} is not a valid {what}")))
}

fn classify(error: tokio_postgres::Error) -> StorageError {
    let Some(code) = error.code() else {
        // No answer from the server: the connection failed or closed
        let io = std::iter::successors(std::error::Error::source(&error), |e| e.source())
            .any(|e| e.is::<std::io::Error>());
        return if error.is_closed() || io {
            StorageError::Transient(Box::new(error))
        } else {
            StorageError::database(error)
        };
    };
    if code.code() == FENCED {
        return StorageError::Fenced;
    }
    let class = &code.code()[..2];
    let transient = [
        &SqlState::T_R_SERIALIZATION_FAILURE,
        &SqlState::T_R_DEADLOCK_DETECTED,
        &SqlState::QUERY_CANCELED,
        &SqlState::LOCK_NOT_AVAILABLE,
        &SqlState::ADMIN_SHUTDOWN,
        &SqlState::CRASH_SHUTDOWN,
        &SqlState::CANNOT_CONNECT_NOW,
        // A standby promoted late in a failover
        &SqlState::READ_ONLY_SQL_TRANSACTION,
    ]
    .contains(&code)
        // Connection exceptions and insufficient resources
        || class == "08"
        || class == "53";
    if transient {
        StorageError::Transient(Box::new(error))
    } else {
        StorageError::database(error)
    }
}

/// How [`PostgresStore`] connects, besides the URL.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ConnectOptions {
    /// PEM file of the certificate authority that signed the server's
    /// certificate; the Mozilla root certificates otherwise.
    pub ca_path: Option<PathBuf>,
    /// Most connections the store keeps open. A store from
    /// [`PostgresStore::connect_exclusive`] opens one more, which holds the
    /// database. 16 by default.
    pub pool_size: usize,
    /// Longest wait for a free connection once all are in use, after which
    /// the operation fails with [`StorageError::Transient`]. 5 seconds by
    /// default.
    pub pool_timeout: Duration,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        ConnectOptions {
            ca_path: None,
            pool_size: 16,
            pool_timeout: TIMEOUT,
        }
    }
}

/// PostgreSQL storage backend implementing all storage traits, over a pool
/// of connections that are reopened when the server drops them.
#[derive(Clone)]
pub struct PostgresStore {
    pool: Pool,
    /// Tells the [`DatabaseLock`], if any, that a write was fenced
    lost: Option<watch::Sender<bool>>,
}

/// The database, held by one broker at a time; see
/// [`PostgresStore::connect_exclusive`]. Dropping it releases the database.
pub struct DatabaseLock {
    heartbeat: JoinHandle<()>,
    lost: watch::Receiver<bool>,
}

impl DatabaseLock {
    /// Completes once the database may have been taken over by another
    /// broker, as the connection holding it closed or stopped answering,
    /// or the store had a write refused with [`StorageError::Fenced`].
    /// The store refuses writes from then on; the broker should stop.
    pub async fn lost(&self) {
        let _ = self.lost.clone().wait_for(|lost| *lost).await;
    }

    /// Whether `lost` has completed.
    pub fn is_lost(&self) -> bool {
        *self.lost.borrow()
    }
}

impl Drop for DatabaseLock {
    fn drop(&mut self) {
        // Closes the connection, which releases the lock
        self.heartbeat.abort();
    }
}

impl PostgresStore {
    /// Connect to the database at `url`, such as
    /// `postgres://user:password@localhost/mercurio`, and create or update
    /// the tables.
    ///
    /// The connection is encrypted, with the server's certificate and host
    /// name verified against `options.ca_path`, a PEM file, or else the
    /// Mozilla root certificates. Without `sslmode` in the URL, a server on
    /// this host, over loopback or a Unix socket, may decline encryption
    /// unless `ca_path` is given. `sslmode=require`, `verify-ca` and
    /// `verify-full` all verify the certificate and host name;
    /// `sslmode=prefer` and `disable` allow plaintext.
    ///
    /// Nothing stops another broker from using the database meanwhile; a
    /// broker serving clients should use [`Self::connect_exclusive`].
    pub async fn connect(url: &str, options: &ConnectOptions) -> Result<Self> {
        let config = base_config(url, options.ca_path.is_some())?;
        let tls = tls(options.ca_path.as_deref())?;
        let store = Self::with_pool(config, tls, options, None, None)?;
        let client = store.client().await?;
        log_encryption(&client).await?;
        migrate(&client).await?;
        Ok(store)
    }

    /// Like [`Self::connect`], for a broker that will serve from the
    /// database: brokers sharing one would overwrite each other's sessions.
    /// Takes the database for this broker, waiting until it is free with
    /// `wait`, or else returns `None` if another broker holds it.
    ///
    /// Once another broker takes the database over, this store's writes
    /// fail, even those already on their way.
    pub async fn connect_exclusive(
        url: &str,
        options: &ConnectOptions,
        wait: bool,
    ) -> Result<Option<(Self, DatabaseLock)>> {
        let config = base_config(url, options.ca_path.is_some())?;
        let tls = tls(options.ca_path.as_deref())?;

        // The lock belongs to a connection of its own, outside the pool, so
        // it lasts as long as that connection
        let (client, connection) = config.connect(tls.clone()).await.db()?;
        let (lost_tx, lost) = watch::channel(false);
        let closed = lost_tx.clone();
        tokio::spawn(async move {
            let _ = connection.await;
            let _ = closed.send(true);
        });
        log_encryption(&client).await?;
        // The database releases the lock within seconds of this host
        // vanishing, so a standby can take over
        client
            .batch_execute(
                "SET tcp_keepalives_idle = 5;
                 SET tcp_keepalives_interval = 2;
                 SET tcp_keepalives_count = 3;
                 SET tcp_user_timeout = 10000;",
            )
            .await
            .db()?;
        // A role's timeouts, common on managed PostgreSQL, would cancel a
        // standby's wait for the lock or end its idle connection
        client
            .execute(
                "SELECT set_config(name, '0', false) FROM pg_settings
                 WHERE name IN ('statement_timeout', 'lock_timeout', 'idle_session_timeout')",
                &[],
            )
            .await
            .db()?;
        // The schema changes only under the lock, as the broker holding it
        // may not know a newer one
        check_schema(&client).await?;

        let sql = if wait {
            "SELECT true FROM pg_advisory_lock($1)"
        } else {
            "SELECT pg_try_advisory_lock($1)"
        };
        if !client
            .query_one(sql, &[&LOCK_KEY])
            .await
            .db()?
            .try_get(0)
            .db()?
        {
            return Ok(None);
        }
        migrate(&client).await?;
        // Waits for writes of the previous holder already past their check
        let epoch: i64 = client
            .query_one(
                "WITH current AS (SELECT epoch FROM mercurio_lease FOR UPDATE)
                 UPDATE mercurio_lease SET epoch = (SELECT epoch FROM current) + 1
                 RETURNING epoch",
                &[],
            )
            .await
            .db()?
            .try_get(0)
            .db()?;

        let fenced = lost_tx.clone();
        let heartbeat = tokio::spawn(async move {
            loop {
                tokio::time::sleep(HEARTBEAT).await;
                let answer = tokio::time::timeout(TIMEOUT, client.simple_query("")).await;
                if !matches!(answer, Ok(Ok(_))) {
                    let _ = lost_tx.send(true);
                    return;
                }
            }
        });
        let store = Self::with_pool(config, tls, options, Some(epoch), Some(fenced))?;
        Ok(Some((store, DatabaseLock { heartbeat, lost })))
    }

    fn with_pool(
        mut config: tokio_postgres::Config,
        tls: MakeRustlsConnect,
        options: &ConnectOptions,
        epoch: Option<i64>,
        lost: Option<watch::Sender<bool>>,
    ) -> Result<Self> {
        let mut settings = format!(
            "{} -c statement_timeout={}",
            config.get_options().unwrap_or_default(),
            TIMEOUT.as_millis()
        );
        if let Some(epoch) = epoch {
            settings.push_str(&format!(" -c mercurio.epoch={epoch}"));
        }
        config.options(settings.trim());
        let pool = Pool::builder(Manager::new(config, tls))
            .runtime(Runtime::Tokio1)
            .max_size(options.pool_size)
            .wait_timeout(Some(options.pool_timeout))
            .create_timeout(Some(TIMEOUT))
            .recycle_timeout(Some(TIMEOUT))
            .build()
            .map_err(|e| StorageError::OperationFailed(e.to_string()))?;
        Ok(PostgresStore { pool, lost })
    }

    async fn client(&self) -> Result<Object> {
        self.pool.get().await.map_err(|e| match e {
            PoolError::Backend(e) => classify(e),
            PoolError::Timeout(_) => StorageError::Transient(Box::new(e)),
            e => StorageError::OperationFailed(crate::error_chain(&e)),
        })
    }

    /// Passes `result` on, giving the database up if another broker took
    /// it over, so the broker stops without waiting for the lock's
    /// heartbeat to notice.
    fn fence<T>(&self, result: Result<T>) -> Result<T> {
        if let (Err(StorageError::Fenced), Some(lost)) = (&result, &self.lost) {
            lost.send_replace(true);
        }
        result
    }

    async fn query(&self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<Vec<Row>> {
        let client = self.client().await?;
        let statement = client.prepare_cached(sql).await.db()?;
        // Some, such as the retained lookup, delete what has expired
        self.fence(client.query(&statement, params).await.db())
    }
}

/// The connection settings in `url`, with the timeouts and keepalives the
/// broker needs where the URL sets none.
///
/// TLS is required where the URL sets no `sslmode` and the connection
/// leaves the host or `ca` is given, as tokio-postgres would otherwise fall
/// back to plaintext when the server has TLS off. libpq's `verify-ca` and
/// `verify-full` become `require`, which verifies the certificate chain
/// and host name with rustls.
fn base_config(url: &str, ca: bool) -> Result<tokio_postgres::Config> {
    let url = url
        .replace("sslmode=verify-full", "sslmode=require")
        .replace("sslmode=verify-ca", "sslmode=require");
    let mut config: tokio_postgres::Config = url.parse().db()?;
    let local = config.get_hosts().iter().all(|host| match host {
        Host::Tcp(name) => {
            name == "localhost"
                || name
                    .trim_matches(['[', ']'])
                    .parse::<IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        }
        #[cfg(unix)]
        Host::Unix(_) => true,
    });
    if !url.contains("sslmode=") {
        if ca || !local {
            config.ssl_mode(SslMode::Require);
        }
    } else if config.get_ssl_mode() != SslMode::Require && !local {
        warn!("The PostgreSQL URL allows an unencrypted connection to a remote host");
    }
    if config.get_connect_timeout().is_none() {
        config.connect_timeout(TIMEOUT);
    }
    // Notice a vanished database within seconds, as a standby may take over
    config
        .keepalives_idle(Duration::from_secs(5))
        .keepalives_interval(Duration::from_secs(2))
        .keepalives_retries(3);
    Ok(config)
}

/// Logs whether the session is encrypted, which a URL allowing plaintext
/// leaves to the server.
async fn log_encryption(client: &tokio_postgres::Client) -> Result<()> {
    let row = client
        .query_one(
            "SELECT ssl, version FROM pg_stat_ssl WHERE pid = pg_backend_pid()",
            &[],
        )
        .await
        .db()?;
    match row.try_get::<_, Option<String>>(1).db()? {
        Some(version) if row.try_get(0).db()? => {
            info!("Connected to PostgreSQL over {version}")
        }
        _ => info!("Connected to PostgreSQL without encryption"),
    }
    Ok(())
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

type Params = Vec<Box<dyn ToSql + Sync + Send>>;

/// The client's mount point and id, `$1` and `$2` of its statements.
fn key_params(key: &SessionKey) -> Params {
    vec![
        Box::new(key.stored_mount_point().to_string()),
        Box::new(key.client_id.clone()),
    ]
}

/// `key_params` followed by `more`.
fn with_key(key: &SessionKey, more: Params) -> Params {
    let mut params = key_params(key);
    params.extend(more);
    params
}

/// Inserts or updates `subscriptions` of the client of `key`, if it has a
/// stored session.
fn insert_subscriptions(
    key: &SessionKey,
    subscriptions: &[StoredSubscription],
) -> (&'static str, Params) {
    (
        "INSERT INTO subscriptions
         (mount_point, client_id, topic_filter, qos, no_local, retain_as_published,
          subscription_identifier)
         SELECT $1, $2, * FROM unnest($3::TEXT[], $4::INTEGER[], $5::BOOLEAN[], $6::BOOLEAN[],
                                      $7::BIGINT[])
         WHERE EXISTS (SELECT 1 FROM sessions WHERE mount_point = $1 AND client_id = $2)
         ON CONFLICT (mount_point, client_id, topic_filter) DO UPDATE SET
         qos = EXCLUDED.qos, no_local = EXCLUDED.no_local,
         retain_as_published = EXCLUDED.retain_as_published,
         subscription_identifier = EXCLUDED.subscription_identifier",
        with_key(
            key,
            vec![
                column(subscriptions, |s| s.topic_filter.clone()),
                column(subscriptions, |s| qos_to_int(s.qos)),
                column(subscriptions, |s| s.no_local),
                column(subscriptions, |s| s.retain_as_published),
                column(subscriptions, |s| s.subscription_identifier.map(i64::from)),
            ],
        ),
    )
}

/// The statements that apply `write` for the client of `key`, other than
/// those applied in bulk.
fn statements(key: &SessionKey, write: &Write) -> Vec<(&'static str, Params)> {
    match write {
        Write::SaveSession(session) => vec![
            (
                "INSERT INTO sessions
                 (mount_point, client_id, subscriptions, clean_start, expiry_interval,
                  expires_at, owner_username, owner_mount_point)
                 VALUES ($1, $2, ''::BYTEA, $3, $4, $5, $6, $7)
                 ON CONFLICT (mount_point, client_id) DO UPDATE SET
                 subscriptions = EXCLUDED.subscriptions, clean_start = EXCLUDED.clean_start,
                 expiry_interval = EXCLUDED.expiry_interval, expires_at = EXCLUDED.expires_at,
                 owner_username = EXCLUDED.owner_username,
                 owner_mount_point = EXCLUDED.owner_mount_point",
                with_key(
                    key,
                    vec![
                        // No longer used; kept for databases that require it
                        Box::new(false),
                        Box::new(session.expiry_interval.map(i64::from)),
                        Box::new(session.expires_at.map(|t| t as i64)),
                        Box::new(session.owner.username.clone()),
                        Box::new(session.owner.mount_point.clone()),
                    ],
                ),
            ),
            (
                "DELETE FROM subscriptions WHERE mount_point = $1 AND client_id = $2",
                key_params(key),
            ),
            insert_subscriptions(key, &session.subscriptions),
        ],
        Write::AddSubscription(subscription) => {
            vec![insert_subscriptions(
                key,
                std::slice::from_ref(subscription),
            )]
        }
        Write::RemoveSubscription(topic_filter) => vec![(
            "DELETE FROM subscriptions
             WHERE mount_point = $1 AND client_id = $2 AND topic_filter = $3",
            with_key(key, vec![Box::new(topic_filter.clone())]),
        )],
        Write::SetExpiry {
            interval,
            expires_at,
        } => vec![(
            "UPDATE sessions SET expiry_interval = $3, expires_at = $4
             WHERE mount_point = $1 AND client_id = $2",
            with_key(
                key,
                vec![Box::new(i64::from(*interval)), Box::new(*expires_at as i64)],
            ),
        )],
        Write::StoreWill(will) => vec![(
            "INSERT INTO will_messages
             (mount_point, client_id, topic, payload, qos, retain, delay_secs, properties)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (mount_point, client_id) DO UPDATE SET
             topic = EXCLUDED.topic, payload = EXCLUDED.payload, qos = EXCLUDED.qos,
             retain = EXCLUDED.retain, delay_secs = EXCLUDED.delay_secs,
             properties = EXCLUDED.properties",
            with_key(
                key,
                vec![
                    Box::new(will.topic.clone()),
                    Box::new(will.payload.to_vec()),
                    Box::new(qos_to_int(will.qos)),
                    Box::new(will.retain),
                    Box::new(i64::from(will.delay_secs)),
                    Box::new(encode_properties(&will.properties)),
                ],
            ),
        )],
        Write::DeleteWill => vec![(
            "DELETE FROM will_messages WHERE mount_point = $1 AND client_id = $2",
            key_params(key),
        )],
        Write::StoreInflight(_)
        | Write::ReleaseInflight(_)
        | Write::RemoveInflight(_)
        | Write::StoreIncomingQos2(_)
        | Write::RemoveIncomingQos2(_)
        | Write::Enqueue(..)
        | Write::Dequeue(_) => {
            unreachable!("applied in bulk by bulk_statements")
        }
        Write::ClearQueue => vec![(
            "DELETE FROM queued_messages WHERE mount_point = $1 AND client_id = $2",
            key_params(key),
        )],
        Write::DeleteSession => [
            "DELETE FROM sessions WHERE mount_point = $1 AND client_id = $2",
            "DELETE FROM subscriptions WHERE mount_point = $1 AND client_id = $2",
            "DELETE FROM will_messages WHERE mount_point = $1 AND client_id = $2",
            "DELETE FROM queued_messages WHERE mount_point = $1 AND client_id = $2",
            "DELETE FROM inflight_messages WHERE mount_point = $1 AND client_id = $2",
            "DELETE FROM incoming_qos2 WHERE mount_point = $1 AND client_id = $2",
        ]
        .into_iter()
        .map(|sql| (sql, key_params(key)))
        .collect(),
    }
}

/// What a run of in-flight changes leaves for one packet id.
enum Fate<'a> {
    /// With whether it was released since
    Stored(&'a InflightMessage, bool),
    Released,
    Removed,
}

/// Statements applying a run of in-flight or queue changes in bulk: at
/// most one per kind of change, however long the run, as each statement
/// costs about as much as the rows it writes. The changes of a run touch
/// rows by key only, so applying each key's final state in any order is
/// the same as applying them in order.
fn bulk_statements(key: &SessionKey, run: &[&Write]) -> Vec<(&'static str, Params)> {
    let mut fates: HashMap<u16, Fate> = HashMap::new();
    // None once removed
    let mut queued: BTreeMap<u64, Option<&Message>> = BTreeMap::new();
    // Whether each packet id ends up stored
    let mut incoming: HashMap<u16, bool> = HashMap::new();
    for write in run {
        match write {
            Write::StoreInflight(message) => {
                fates.insert(message.packet_id, Fate::Stored(message, false));
            }
            Write::ReleaseInflight(packet_id) => match fates.get_mut(packet_id) {
                Some(Fate::Stored(_, released)) => *released = true,
                Some(Fate::Removed) => {}
                _ => {
                    fates.insert(*packet_id, Fate::Released);
                }
            },
            Write::RemoveInflight(packet_id) => {
                fates.insert(*packet_id, Fate::Removed);
            }
            Write::Enqueue(sequence, message) => {
                queued.insert(*sequence, Some(message));
            }
            Write::Dequeue(sequence) => {
                queued.insert(*sequence, None);
            }
            Write::StoreIncomingQos2(packet_id) => {
                incoming.insert(*packet_id, true);
            }
            Write::RemoveIncomingQos2(packet_id) => {
                incoming.insert(*packet_id, false);
            }
            _ => unreachable!("only in-flight and queue changes run in bulk"),
        }
    }

    let mut statements: Vec<(&'static str, Params)> = Vec::new();
    let ids_of = |wanted: fn(&Fate) -> bool| -> Vec<i32> {
        fates
            .iter()
            .filter(|(_, fate)| wanted(fate))
            .map(|(packet_id, _)| i32::from(*packet_id))
            .collect()
    };
    let removed = ids_of(|fate| matches!(fate, Fate::Removed));
    if !removed.is_empty() {
        statements.push((
            "DELETE FROM inflight_messages
             WHERE mount_point = $1 AND client_id = $2 AND packet_id = ANY($3)",
            with_key(key, vec![Box::new(removed)]),
        ));
    }
    let released = ids_of(|fate| matches!(fate, Fate::Released));
    if !released.is_empty() {
        statements.push((
            "UPDATE inflight_messages SET released = TRUE, payload = NULL
             WHERE mount_point = $1 AND client_id = $2 AND packet_id = ANY($3)",
            with_key(key, vec![Box::new(released)]),
        ));
    }
    let stored: Vec<(&InflightMessage, bool)> = fates
        .values()
        .filter_map(|fate| match fate {
            Fate::Stored(message, released) => Some((*message, *released)),
            _ => None,
        })
        .collect();
    if !stored.is_empty() {
        statements.push((
            "INSERT INTO inflight_messages
             (mount_point, client_id, packet_id, topic, payload, qos, dup, sequence, released,
              properties, subscription_identifiers, retain, expires_at, share_group)
             SELECT $1, $2, * FROM unnest($3::INTEGER[], $4::TEXT[], $5::BYTEA[],
                                          $6::INTEGER[], $7::BOOLEAN[], $8::BIGINT[],
                                          $9::BOOLEAN[], $10::BYTEA[], $11::TEXT[],
                                          $12::BOOLEAN[], $13::BIGINT[], $14::TEXT[])
             ON CONFLICT (mount_point, client_id, packet_id) DO UPDATE SET
             topic = EXCLUDED.topic, payload = EXCLUDED.payload, qos = EXCLUDED.qos,
             dup = EXCLUDED.dup, sequence = EXCLUDED.sequence, released = EXCLUDED.released,
             properties = EXCLUDED.properties,
             subscription_identifiers = EXCLUDED.subscription_identifiers,
             retain = EXCLUDED.retain, expires_at = EXCLUDED.expires_at,
             share_group = EXCLUDED.share_group",
            with_key(
                key,
                vec![
                    column(&stored, |(m, _)| i32::from(m.packet_id)),
                    column(&stored, |(m, _)| m.topic.clone()),
                    column(&stored, |(m, released)| {
                        m.payload.as_ref().filter(|_| !released).map(|p| p.to_vec())
                    }),
                    column(&stored, |(m, _)| qos_to_int(m.qos)),
                    column(&stored, |(m, _)| m.dup),
                    column(&stored, |(m, _)| m.sequence as i64),
                    column(&stored, |(m, released)| m.released || *released),
                    column(&stored, |(m, _)| encode_properties(&m.properties)),
                    column(&stored, |(m, _)| identifiers(&m.subscription_identifiers)),
                    column(&stored, |(m, _)| m.retain),
                    column(&stored, |(m, _)| m.expires_at.map(|t| t as i64)),
                    column(&stored, |(m, _)| m.share_group.clone()),
                ],
            ),
        ));
    }
    let incoming_ids = |stored: bool| -> Vec<i32> {
        incoming
            .iter()
            .filter(|(_, s)| **s == stored)
            .map(|(packet_id, _)| i32::from(*packet_id))
            .collect()
    };
    let released_ids = incoming_ids(false);
    if !released_ids.is_empty() {
        statements.push((
            "DELETE FROM incoming_qos2
             WHERE mount_point = $1 AND client_id = $2 AND packet_id = ANY($3)",
            with_key(key, vec![Box::new(released_ids)]),
        ));
    }
    let received_ids = incoming_ids(true);
    if !received_ids.is_empty() {
        statements.push((
            "INSERT INTO incoming_qos2 (mount_point, client_id, packet_id)
             SELECT $1, $2, * FROM unnest($3::INTEGER[])
             ON CONFLICT DO NOTHING",
            with_key(key, vec![Box::new(received_ids)]),
        ));
    }
    let dequeued: Vec<i64> = queued
        .iter()
        .filter(|(_, message)| message.is_none())
        .map(|(sequence, _)| *sequence as i64)
        .collect();
    if !dequeued.is_empty() {
        statements.push((
            "DELETE FROM queued_messages
             WHERE mount_point = $1 AND client_id = $2 AND sequence = ANY($3)",
            with_key(key, vec![Box::new(dequeued)]),
        ));
    }
    let queued: Vec<(&u64, &&Message)> = queued
        .iter()
        .filter_map(|(sequence, message)| Some((sequence, message.as_ref()?)))
        .collect();
    if !queued.is_empty() {
        statements.push((
            "INSERT INTO queued_messages
             (mount_point, client_id, sequence, topic, payload, qos, retain, properties,
              expires_at, subscription_identifiers, share_group)
             SELECT $1, $2, * FROM unnest($3::BIGINT[], $4::TEXT[], $5::BYTEA[],
                                          $6::INTEGER[], $7::BOOLEAN[], $8::BYTEA[],
                                          $9::BIGINT[], $10::TEXT[], $11::TEXT[])
             ON CONFLICT (mount_point, client_id, sequence) DO UPDATE SET
             topic = EXCLUDED.topic, payload = EXCLUDED.payload, qos = EXCLUDED.qos,
             retain = EXCLUDED.retain, properties = EXCLUDED.properties,
             expires_at = EXCLUDED.expires_at,
             subscription_identifiers = EXCLUDED.subscription_identifiers,
             share_group = EXCLUDED.share_group",
            with_key(
                key,
                vec![
                    column(&queued, |(s, _)| **s as i64),
                    column(&queued, |(_, m)| m.topic.to_string()),
                    column(&queued, |(_, m)| m.payload.as_ref().map(|p| p.to_vec())),
                    column(&queued, |(_, m)| qos_to_int(m.qos)),
                    column(&queued, |(_, m)| m.retain),
                    column(&queued, |(_, m)| encode_properties(&m.properties)),
                    column(&queued, |(_, m)| m.expires_at.map(|t| t as i64)),
                    column(&queued, |(_, m)| identifiers(&m.subscription_identifiers)),
                    column(&queued, |(_, m)| {
                        m.share_group.as_deref().map(str::to_string)
                    }),
                ],
            ),
        ));
    }
    statements
}

fn column<T, V: ToSql + Sync + Send + 'static>(
    rows: &[T],
    value: impl Fn(&T) -> V,
) -> Box<dyn ToSql + Sync + Send> {
    Box::new(rows.iter().map(value).collect::<Vec<V>>())
}

/// Whether a change is applied in bulk with its neighbours.
fn in_bulk(write: &Write) -> bool {
    matches!(
        write,
        Write::StoreInflight(_)
            | Write::ReleaseInflight(_)
            | Write::RemoveInflight(_)
            | Write::StoreIncomingQos2(_)
            | Write::RemoveIncomingQos2(_)
            | Write::Enqueue(..)
            | Write::Dequeue(_)
    )
}

#[async_trait]
impl BatchStore for PostgresStore {
    fn backend_name(&self) -> &'static str {
        "postgres"
    }

    fn ban_store(self: Arc<Self>) -> Option<Arc<dyn BanStore>> {
        Some(self)
    }

    async fn apply(&self, key: &SessionKey, writes: &[Write]) -> Result<()> {
        self.fence(self.apply_batch(key, writes).await)
    }

    async fn load_session(&self, key: &SessionKey) -> Result<Option<SessionState>> {
        // One row per subscription, or one without any
        let rows = self
            .query(
                &format!(
                    "SELECT {SESSION_COLUMNS}, f.topic_filter, f.qos, f.no_local,
                            f.retain_as_published, f.subscription_identifier
                     FROM sessions s
                     LEFT JOIN subscriptions f
                         ON f.mount_point = s.mount_point AND f.client_id = s.client_id
                     WHERE s.mount_point = $1 AND s.client_id = $2"
                ),
                &[&key.stored_mount_point(), &key.client_id],
            )
            .await?;
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        let mut session = session_from_row(row, key)?;
        for row in &rows {
            if row
                .try_get::<_, Option<&str>>("topic_filter")
                .col()?
                .is_some()
            {
                session.subscriptions.push(subscription_from_row(row)?);
            }
        }
        Ok(Some(session))
    }

    async fn list_sessions(&self) -> Result<Vec<SessionKey>> {
        self.query("SELECT mount_point, client_id FROM sessions", &[])
            .await?
            .iter()
            .map(key_from_row)
            .collect()
    }

    async fn load_will(&self, key: &SessionKey) -> Result<Option<StoredWillMessage>> {
        self.query(
            "SELECT * FROM will_messages WHERE mount_point = $1 AND client_id = $2",
            &[&key.stored_mount_point(), &key.client_id],
        )
        .await?
        .first()
        .map(will_from_row)
        .transpose()
    }

    async fn load_inflight(&self, key: &SessionKey) -> Result<Vec<InflightMessage>> {
        self.query(
            "SELECT * FROM inflight_messages WHERE mount_point = $1 AND client_id = $2",
            &[&key.stored_mount_point(), &key.client_id],
        )
        .await?
        .iter()
        .map(inflight_from_row)
        .collect()
    }

    async fn load_incoming_qos2(&self, key: &SessionKey) -> Result<Vec<u16>> {
        self.query(
            "SELECT packet_id FROM incoming_qos2 WHERE mount_point = $1 AND client_id = $2",
            &[&key.stored_mount_point(), &key.client_id],
        )
        .await?
        .iter()
        .map(incoming_from_row)
        .collect()
    }

    async fn load_queue(&self, key: &SessionKey) -> Result<Vec<(u64, Message)>> {
        self.query(
            "SELECT * FROM queued_messages WHERE mount_point = $1 AND client_id = $2
             ORDER BY sequence",
            &[&key.stored_mount_point(), &key.client_id],
        )
        .await?
        .iter()
        .map(queued_from_row)
        .collect()
    }

    /// One query per table for the whole page, those after the sessions
    /// pipelined: its sessions, then the rows of every other table in the
    /// range of keys the page covers. Keys are compared as rows, in the
    /// order of the primary keys, so each lookup reads its index in order.
    async fn load_clients(
        &self,
        after: Option<&SessionKey>,
        limit: usize,
    ) -> Result<Vec<LoadedClient>> {
        let after_clause = match after {
            Some(_) => "(mount_point, client_id) > ($1, $2)",
            None => "$1::TEXT IS NULL AND $2::TEXT IS NULL",
        };
        let after_mount_point = after.map(SessionKey::stored_mount_point);
        let after_client_id = after.map(|key| key.client_id.as_str());
        let sessions = self
            .query(
                &format!(
                    "SELECT s.mount_point, s.client_id, {SESSION_COLUMNS} FROM sessions s
                     WHERE {after_clause} ORDER BY s.mount_point, s.client_id LIMIT $3"
                ),
                &[&after_mount_point, &after_client_id, &(limit as i64)],
            )
            .await?;
        let mut clients: HashMap<SessionKey, Result<StoredClient>> = HashMap::new();
        let mut order = Vec::with_capacity(sessions.len());
        for row in &sessions {
            let key = key_from_row(row)?;
            let stored = session_from_row(row, &key).map(|session| StoredClient {
                session,
                will: None,
                queue: Vec::new(),
                inflight: Vec::new(),
                incoming_qos2: Vec::new(),
            });
            order.push(key.clone());
            clients.insert(key, stored);
        }
        let Some(last) = order.last().cloned() else {
            return Ok(Vec::new());
        };

        let within = format!(
            "WHERE {after_clause} AND (mount_point, client_id) <= ($3, $4)
             ORDER BY mount_point, client_id"
        );
        let sql = [
            format!("SELECT * FROM subscriptions {within}"),
            format!("SELECT * FROM will_messages {within}"),
            format!("SELECT * FROM queued_messages {within}, sequence"),
            format!("SELECT * FROM inflight_messages {within}"),
            format!("SELECT mount_point, client_id, packet_id FROM incoming_qos2 {within}"),
        ];
        let client = self.client().await?;
        let mut statements = Vec::with_capacity(sql.len());
        for sql in &sql {
            statements.push(client.prepare_cached(sql).await.db()?);
        }
        let last_mount_point = last.stored_mount_point();
        let params: [&(dyn ToSql + Sync); 4] = [
            &after_mount_point,
            &after_client_id,
            &last_mount_point,
            &last.client_id,
        ];
        let results = join_all(statements.iter().map(|s| client.query(s, &params))).await;
        let mut results = results.into_iter();
        let mut next = || results.next().expect("one result per statement").db();

        type Put = fn(&mut StoredClient, &Row) -> Result<()>;
        let puts: [Put; 5] = [
            |client, row| {
                client
                    .session
                    .subscriptions
                    .push(subscription_from_row(row)?);
                Ok(())
            },
            |client, row| {
                client.will = Some(will_from_row(row)?);
                Ok(())
            },
            |client, row| {
                client.queue.push(queued_from_row(row)?);
                Ok(())
            },
            |client, row| {
                client.inflight.push(inflight_from_row(row)?);
                Ok(())
            },
            |client, row| {
                client.incoming_qos2.push(incoming_from_row(row)?);
                Ok(())
            },
        ];
        for put in puts {
            for row in next()? {
                let key = key_from_row(&row)?;
                // Rows of a client without a session are left out
                let Some(Ok(stored)) = clients.get_mut(&key) else {
                    continue;
                };
                if let Err(e) = put(stored, &row) {
                    clients.insert(key, Err(e));
                }
            }
        }
        Ok(order
            .into_iter()
            .map(|key| LoadedClient {
                stored: clients.remove(&key).expect("each client is gathered"),
                key,
            })
            .collect())
    }
}

/// The key of a row with `mount_point` and `client_id` columns.
fn key_from_row(row: &Row) -> Result<SessionKey> {
    Ok(SessionKey::new(
        Some(row.try_get("mount_point").col()?),
        row.try_get::<_, String>("client_id").col()?,
    ))
}

/// The columns of a session `session_from_row` reads.
const SESSION_COLUMNS: &str = "s.expiry_interval, s.expires_at, s.owner_username,
    s.owner_mount_point, length(s.subscriptions) > 0 AS unmoved";

/// A session, without its subscriptions, which have a table of their own.
fn session_from_row(row: &Row, key: &SessionKey) -> Result<SessionState> {
    // Subscriptions an older version kept in the session's row that the
    // move to their table couldn't read
    if row.try_get("unmoved").col()? {
        return Err(StorageError::SerializationError(
            "subscriptions left in the session's row can't be read".into(),
        ));
    }
    Ok(SessionState {
        client_id: key.client_id.clone(),
        subscriptions: Vec::new(),
        expiry_interval: row
            .try_get::<_, Option<i64>>("expiry_interval")
            .col()?
            .map(|i| narrow(i, "session expiry interval"))
            .transpose()?,
        expires_at: row
            .try_get::<_, Option<i64>>("expires_at")
            .col()?
            .map(|t| t as u64),
        owner: SessionOwner {
            username: row.try_get("owner_username").col()?,
            mount_point: row.try_get("owner_mount_point").col()?,
        },
    })
}

fn subscription_from_row(row: &Row) -> Result<StoredSubscription> {
    Ok(StoredSubscription {
        topic_filter: row.try_get("topic_filter").col()?,
        qos: int_to_qos(row.try_get("qos").col()?)?,
        no_local: row.try_get("no_local").col()?,
        retain_as_published: row.try_get("retain_as_published").col()?,
        subscription_identifier: row
            .try_get::<_, Option<i64>>("subscription_identifier")
            .col()?
            .map(|id| narrow(id, "subscription identifier"))
            .transpose()?,
    })
}

fn will_from_row(row: &Row) -> Result<StoredWillMessage> {
    Ok(StoredWillMessage {
        topic: row.try_get("topic").col()?,
        payload: Bytes::from(row.try_get::<_, Vec<u8>>("payload").col()?),
        qos: int_to_qos(row.try_get("qos").col()?)?,
        retain: row.try_get("retain").col()?,
        delay_secs: narrow(row.try_get::<_, i64>("delay_secs").col()?, "will delay")?,
        properties: decode_properties(row.try_get("properties").col()?)?,
    })
}

fn queued_from_row(row: &Row) -> Result<(u64, Message)> {
    let topic: String = row.try_get("topic").col()?;
    let message = Message {
        packet_id: None,
        topic: Arc::from(topic.as_str()),
        dup: false,
        qos: int_to_qos(row.try_get("qos").col()?)?,
        retain: row.try_get("retain").col()?,
        payload: row
            .try_get::<_, Option<Vec<u8>>>("payload")
            .col()?
            .map(Bytes::from),
        properties: decode_properties(row.try_get("properties").col()?)?,
        expires_at: row
            .try_get::<_, Option<i64>>("expires_at")
            .col()?
            .map(|t| t as u64),
        subscription_identifiers: decode_identifiers(
            row.try_get("subscription_identifiers").col()?,
        )?,
        origin: None,
        share_group: row
            .try_get::<_, Option<String>>("share_group")
            .col()?
            .map(Arc::from),
    };
    Ok((row.try_get::<_, i64>("sequence").col()? as u64, message))
}

fn incoming_from_row(row: &Row) -> Result<u16> {
    narrow(row.try_get::<_, i32>("packet_id").col()?, "packet id")
}

impl PostgresStore {
    /// Applies the batch in one transaction, its statements pipelined, so
    /// it costs about one round trip.
    async fn apply_batch(&self, key: &SessionKey, writes: &[Write]) -> Result<()> {
        let mut statements = Vec::new();
        let mut run = Vec::new();
        for write in writes {
            if in_bulk(write) {
                run.push(write);
                continue;
            }
            statements.extend(bulk_statements(key, &run));
            run.clear();
            statements.extend(self::statements(key, write));
        }
        statements.extend(bulk_statements(key, &run));
        if statements.is_empty() {
            return Ok(());
        }
        let client = self.client().await?;
        let mut prepared = Vec::with_capacity(statements.len());
        for (sql, params) in &statements {
            let params: Vec<&(dyn ToSql + Sync)> = params
                .iter()
                .map(|p| p.as_ref() as &(dyn ToSql + Sync))
                .collect();
            prepared.push((client.prepare_cached(sql).await.db()?, params));
        }

        if let [(statement, params)] = prepared.as_slice() {
            client.execute(statement, params).await.db()?;
            return Ok(());
        }
        // Requests go out in the order the futures are first polled, which
        // join_all does in order; after a failure the transaction is
        // aborted and COMMIT rolls it back
        let begin = client.batch_execute("BEGIN");
        let executes = join_all(
            prepared
                .iter()
                .map(|(statement, params)| client.execute(statement, params)),
        );
        let commit = client.batch_execute("COMMIT");
        let (begun, executed, committed) = futures_util::join!(begin, executes, commit);
        begun.db()?;
        for result in executed {
            result.db()?;
        }
        committed.db()?;
        Ok(())
    }
}

/// Fails if the stored properties can't be decoded, rather than delivering
/// the message without them.
fn decode_properties(encoded: Vec<u8>) -> Result<MessageProperties> {
    MessageProperties::decode(&mut Bytes::from(encoded))
        .map_err(|e| StorageError::SerializationError(e.to_string()))
}

/// Comma-separated, or no value when there are none.
fn identifiers(ids: &[u32]) -> Option<String> {
    (!ids.is_empty()).then(|| ids.iter().map(u32::to_string).collect::<Vec<_>>().join(","))
}

fn decode_identifiers(ids: Option<String>) -> Result<Vec<u32>> {
    ids.as_deref()
        .map_or(Ok(Vec::new()), |ids| {
            ids.split(',').map(str::parse).collect()
        })
        .map_err(|e| StorageError::SerializationError(format!("subscription identifiers: {e}")))
}

fn inflight_from_row(row: &Row) -> Result<InflightMessage> {
    Ok(InflightMessage {
        packet_id: narrow(row.try_get::<_, i32>("packet_id").col()?, "packet id")?,
        topic: row.try_get("topic").col()?,
        payload: row
            .try_get::<_, Option<Vec<u8>>>("payload")
            .col()?
            .map(Bytes::from),
        qos: int_to_qos(row.try_get("qos").col()?)?,
        dup: row.try_get("dup").col()?,
        sequence: row.try_get::<_, i64>("sequence").col()? as u64,
        released: row.try_get("released").col()?,
        properties: decode_properties(row.try_get("properties").col()?)?,
        subscription_identifiers: decode_identifiers(
            row.try_get("subscription_identifiers").col()?,
        )?,
        retain: row.try_get("retain").col()?,
        expires_at: row
            .try_get::<_, Option<i64>>("expires_at")
            .col()?
            .map(|t| t as u64),
        share_group: row.try_get("share_group").col()?,
    })
}

fn retained_from_row(row: &Row) -> Result<Message> {
    let topic: String = row.try_get("topic").col()?;
    let origin = row
        .try_get::<_, Option<String>>("origin_client_id")
        .col()?
        .map(|client_id| -> Result<_> {
            Ok(Arc::new(MessageOrigin {
                client_id,
                username: row.try_get("origin_username").col()?,
            }))
        })
        .transpose()?;
    Ok(Message {
        packet_id: None,
        topic: Arc::from(topic.as_str()),
        dup: row.try_get("dup").col()?,
        qos: int_to_qos(row.try_get("qos").col()?)?,
        retain: true,
        payload: row
            .try_get::<_, Option<Vec<u8>>>("payload")
            .col()?
            .map(Bytes::from),
        properties: decode_properties(row.try_get("properties").col()?)?,
        expires_at: row
            .try_get::<_, Option<i64>>("expires_at")
            .col()?
            .map(|t| t as u64),
        subscription_identifiers: Vec::new(),
        origin,
        share_group: None,
    })
}

/// The retained lookup of `get_retained_page`: `$1` the first topic, read
/// from on, or after if not `inclusive`, `$2` now, `$3` the limit, and `$4`
/// the topic before which to stop, if `bounded`. Only reads, so it never
/// waits for the lock the fence triggers take.
fn retained_sql(inclusive: bool, bounded: bool) -> String {
    format!(
        "SELECT topic, payload, qos, dup, properties, expires_at,
                origin_client_id, origin_username
         FROM retained_messages
         WHERE topic {} $1 {} AND (expires_at IS NULL OR expires_at > $2)
         ORDER BY topic LIMIT $3",
        if inclusive { ">=" } else { ">" },
        if bounded { "AND topic < $4" } else { "" },
    )
}

#[async_trait]
impl RetainedMessageStore for PostgresStore {
    async fn store_retained(&self, topic: &str, message: Option<Message>) -> Result<bool> {
        let client = self.client().await?;
        let Some(msg) = message else {
            let statement = client
                .prepare_cached("DELETE FROM retained_messages WHERE topic = $1")
                .await
                .db()?;
            let removed = self.fence(client.execute(&statement, &[&topic]).await.db())?;
            return Ok(removed > 0);
        };
        let origin = msg.origin.as_deref();
        // xmax is set on a row an ON CONFLICT clause updated
        let statement = client
            .prepare_cached(
                "INSERT INTO retained_messages
                 (topic, payload, qos, dup, properties, expires_at, origin_client_id,
                  origin_username)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                 ON CONFLICT (topic) DO UPDATE SET
                 payload = EXCLUDED.payload, qos = EXCLUDED.qos, dup = EXCLUDED.dup,
                 properties = EXCLUDED.properties, expires_at = EXCLUDED.expires_at,
                 origin_client_id = EXCLUDED.origin_client_id,
                 origin_username = EXCLUDED.origin_username
                 RETURNING xmax <> 0",
            )
            .await
            .db()?;
        let stored = client
            .query_one(
                &statement,
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
            .await
            .db();
        self.fence(stored)?.try_get(0).db()
    }

    /// Reads only the topics the filter's literal prefix allows, through the
    /// primary key, whose C collation orders them by their bytes, in chunks,
    /// as wildcards past the prefix are matched here.
    async fn get_retained_page(
        &self,
        topic_filter: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Message>> {
        let now = unix_millis() as i64;
        let (start, end) = match TopicRange::of(topic_filter) {
            TopicRange::Exact(topic) => {
                if after.is_some_and(|after| after >= topic.as_str()) || limit == 0 {
                    return Ok(Vec::new());
                }
                return self
                    .query(
                        "SELECT topic, payload, qos, dup, properties, expires_at,
                                origin_client_id, origin_username
                         FROM retained_messages
                         WHERE topic = $1 AND (expires_at IS NULL OR expires_at > $2)",
                        &[&topic, &now],
                    )
                    .await?
                    .iter()
                    .map(retained_from_row)
                    .collect();
            }
            TopicRange::From { start, end } => (start, end),
        };
        let (mut from, mut inclusive) = match after {
            Some(after) if after >= start.as_str() => (after.to_string(), false),
            _ => (start, true),
        };
        let chunk = limit.max(256) as i64;
        let mut messages = Vec::new();
        loop {
            let sql = retained_sql(inclusive, end.is_some());
            let rows = match &end {
                Some(end) => self.query(&sql, &[&from, &now, &chunk, end]).await?,
                None => self.query(&sql, &[&from, &now, &chunk]).await?,
            };
            let read = rows.len() as i64;
            for row in &rows {
                let topic: String = row.try_get("topic").col()?;
                if topic_matches_filter(&topic, topic_filter) {
                    messages.push(retained_from_row(row)?);
                    if messages.len() == limit {
                        return Ok(messages);
                    }
                }
                from = topic;
                inclusive = false;
            }
            if read < chunk {
                return Ok(messages);
            }
        }
    }

    /// Checks first, as the deletion takes the fence's lock even when it
    /// removes nothing.
    async fn remove_expired_retained(&self) -> Result<usize> {
        let now = unix_millis() as i64;
        let any = self
            .query(
                "SELECT EXISTS (SELECT 1 FROM retained_messages WHERE expires_at <= $1)",
                &[&now],
            )
            .await?;
        if !any[0].try_get::<_, bool>(0).db()? {
            return Ok(0);
        }
        let client = self.client().await?;
        let statement = client
            .prepare_cached("DELETE FROM retained_messages WHERE expires_at <= $1")
            .await
            .db()?;
        let removed = self.fence(client.execute(&statement, &[&now]).await.db())?;
        Ok(removed as usize)
    }

    async fn clear_all_retained(&self) -> Result<()> {
        let client = self.client().await?;
        self.fence(
            client
                .execute("DELETE FROM retained_messages", &[])
                .await
                .db(),
        )?;
        Ok(())
    }

    async fn count_retained(&self) -> Result<usize> {
        let rows = self
            .query("SELECT COUNT(*) FROM retained_messages", &[])
            .await?;
        Ok(rows[0].try_get::<_, i64>(0).db()? as usize)
    }
}

#[async_trait]
impl BanStore for PostgresStore {
    async fn load_bans(&self) -> Result<Vec<StoredBan>> {
        self.query(
            "SELECT kind, value, prefix, reason, created_at, expires_at FROM bans",
            &[],
        )
        .await?
        .iter()
        .map(|row| {
            Ok(StoredBan {
                kind: row.try_get(0).db()?,
                value: row.try_get(1).db()?,
                prefix: row.try_get(2).db()?,
                reason: row.try_get(3).db()?,
                created_at: row.try_get::<_, i64>(4).db()? as u64,
                expires_at: row.try_get::<_, Option<i64>>(5).db()?.map(|t| t as u64),
            })
        })
        .collect()
    }

    async fn store_ban(&self, ban: &StoredBan) -> Result<()> {
        let client = self.client().await?;
        let statement = client
            .prepare_cached(
                "INSERT INTO bans (kind, value, prefix, reason, created_at, expires_at)
                 VALUES ($1, $2, $3, $4, $5, $6)
                 ON CONFLICT (kind, value, prefix) DO UPDATE SET reason = EXCLUDED.reason,
                     created_at = EXCLUDED.created_at, expires_at = EXCLUDED.expires_at",
            )
            .await
            .db()?;
        let stored = client
            .execute(
                &statement,
                &[
                    &ban.kind,
                    &ban.value,
                    &ban.prefix,
                    &ban.reason,
                    &(ban.created_at as i64),
                    &ban.expires_at.map(|t| t as i64),
                ],
            )
            .await
            .db();
        self.fence(stored)?;
        Ok(())
    }

    async fn delete_ban(&self, kind: &str, value: &str, prefix: bool) -> Result<()> {
        let client = self.client().await?;
        let statement = client
            .prepare_cached("DELETE FROM bans WHERE kind = $1 AND value = $2 AND prefix = $3")
            .await
            .db()?;
        self.fence(
            client
                .execute(&statement, &[&kind, &value, &prefix])
                .await
                .db(),
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use mercurio_core::qos::QoS;
    use tokio::sync::Mutex;

    use super::*;
    use crate::{testing::key, StoredSubscription};

    /// The tests share one database, and some take it over or empty it.
    static DATABASE: Mutex<()> = Mutex::const_new(());

    /// The database the tests that need one use. They are ignored, and
    /// listed as such, unless `MERCURIO_TEST_POSTGRES_URL` was set when the
    /// tests were built; see `build.rs`.
    fn database_url() -> String {
        std::env::var("MERCURIO_TEST_POSTGRES_URL")
            .expect("MERCURIO_TEST_POSTGRES_URL was set when the tests were built, but not now")
    }

    async fn empty_store(url: &str) -> PostgresStore {
        let store = PostgresStore::connect(url, &ConnectOptions::default())
            .await
            .unwrap();
        store
            .client()
            .await
            .unwrap()
            .batch_execute(
                "TRUNCATE sessions, retained_messages, will_messages, inflight_messages,
                 queued_messages, bans, incoming_qos2, subscriptions",
            )
            .await
            .unwrap();
        store
    }

    /// A database created by 0.2.0, at schema version 1, keeps its
    /// sessions when a newer broker migrates it.
    #[tokio::test]
    #[cfg_attr(not(postgres_tests), ignore = "set MERCURIO_TEST_POSTGRES_URL to run")]
    async fn test_migrates_a_version_1_database() {
        let url = database_url();
        let _database = DATABASE.lock().await;
        let store = PostgresStore::connect(&url, &ConnectOptions::default())
            .await
            .unwrap();
        let client = store.client().await.unwrap();
        client
            .batch_execute(&format!(
                "DROP SCHEMA public CASCADE; CREATE SCHEMA public;
                 SET client_min_messages = warning;
                 {}
                 CREATE TABLE mercurio_schema (version INTEGER NOT NULL);
                 INSERT INTO mercurio_schema VALUES (1);
                 INSERT INTO sessions (client_id, subscriptions, clean_start)
                 VALUES ('old', '', FALSE);",
                MIGRATIONS[0]
            ))
            .await
            .unwrap();
        drop(client);

        let store = PostgresStore::connect(&url, &ConnectOptions::default())
            .await
            .unwrap();
        let version: i32 = store
            .client()
            .await
            .unwrap()
            .query_one("SELECT version FROM mercurio_schema", &[])
            .await
            .unwrap()
            .get(0);
        assert_eq!(version as usize, MIGRATIONS.len());
        let old = store.load_session(&key("old")).await.unwrap().unwrap();
        assert_eq!(old.owner, SessionOwner::default());
    }

    /// A store on a database whose schema is at `version`, as a release
    /// that had only the first `version` migrations left it, after `setup`.
    async fn database_at(url: &str, version: usize, setup: &str) {
        let store = PostgresStore::connect(url, &ConnectOptions::default())
            .await
            .unwrap();
        store
            .client()
            .await
            .unwrap()
            .batch_execute(&format!(
                "DROP SCHEMA public CASCADE; CREATE SCHEMA public;
                 SET client_min_messages = warning;
                 {}
                 CREATE TABLE mercurio_schema (version INTEGER NOT NULL);
                 INSERT INTO mercurio_schema VALUES ({version});
                 {setup}",
                MIGRATIONS[..version].concat()
            ))
            .await
            .unwrap();
    }

    /// Messages stored before share groups were recorded load without one,
    /// and running the migration again changes nothing.
    #[tokio::test]
    #[cfg_attr(not(postgres_tests), ignore = "set MERCURIO_TEST_POSTGRES_URL to run")]
    async fn test_migrates_messages_without_a_share_group() {
        let url = database_url();
        let _database = DATABASE.lock().await;
        let marking = MIGRATIONS.len() - 1;
        database_at(
            &url,
            marking,
            "INSERT INTO queued_messages
                 (mount_point, client_id, sequence, topic, payload, qos, retain, properties)
                 VALUES ('', 'a', 4, 'q', NULL, 1, FALSE, '');
             INSERT INTO inflight_messages
                 (mount_point, client_id, packet_id, topic, payload, qos, dup, sequence,
                  released, properties)
                 VALUES ('', 'a', 1, 'i', NULL, 1, FALSE, 1, FALSE, '');",
        )
        .await;
        let store = PostgresStore::connect(&url, &ConnectOptions::default())
            .await
            .unwrap();
        store
            .client()
            .await
            .unwrap()
            .batch_execute(MIGRATIONS[marking])
            .await
            .unwrap();
        assert_eq!(
            store.load_queue(&key("a")).await.unwrap()[0].1.share_group,
            None
        );
        assert_eq!(
            store.load_inflight(&key("a")).await.unwrap()[0].share_group,
            None
        );
    }

    /// In-flight messages stored before they kept their RETAIN flag and
    /// expiry load without either.
    #[tokio::test]
    #[cfg_attr(not(postgres_tests), ignore = "set MERCURIO_TEST_POSTGRES_URL to run")]
    async fn test_migrates_in_flight_messages_without_retain_or_expiry() {
        let url = database_url();
        let _database = DATABASE.lock().await;
        database_at(
            &url,
            4,
            "INSERT INTO inflight_messages VALUES ('c', 4, 't', 'p', 1, FALSE, 9, FALSE, '', NULL);",
        )
        .await;

        let store = PostgresStore::connect(&url, &ConnectOptions::default())
            .await
            .unwrap();
        let loaded = store.load_inflight(&key("c")).await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!((loaded[0].packet_id, loaded[0].sequence), (4, 9));
        assert!(!loaded[0].retain);
        assert_eq!(loaded[0].expires_at, None);
    }

    /// Sessions of a database from before subscriptions had a table of
    /// their own keep their subscriptions.
    #[tokio::test]
    #[cfg_attr(not(postgres_tests), ignore = "set MERCURIO_TEST_POSTGRES_URL to run")]
    async fn test_migrates_encoded_subscriptions_to_their_table() {
        let url = database_url();
        let _database = DATABASE.lock().await;
        let subscriptions = vec![
            StoredSubscription {
                no_local: true,
                retain_as_published: true,
                subscription_identifier: Some(268_435_455),
                ..StoredSubscription::new("topic/+:x,y", QoS::ExactlyOnce)
            },
            StoredSubscription::new("$share/g/a/#", QoS::AtMostOnce),
        ];
        let encoded = crate::encoding::encode_subscriptions(&subscriptions);
        database_at(
            &url,
            SUBSCRIPTIONS_TABLE,
            &format!(
                "INSERT INTO sessions (client_id, subscriptions, clean_start, owner_username)
                 VALUES ('old', decode('{}', 'hex'), FALSE, 'alice'),
                        ('none', '', FALSE, NULL),
                        ('bad', '9xx:a', FALSE, NULL);",
                encoded
                    .bytes()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            ),
        )
        .await;

        let store = PostgresStore::connect(&url, &ConnectOptions::default())
            .await
            .unwrap();
        let mut loaded = store.load_session(&key("old")).await.unwrap().unwrap();
        loaded
            .subscriptions
            .sort_by(|a, b| b.topic_filter.cmp(&a.topic_filter));
        assert_eq!(loaded.subscriptions, subscriptions);
        assert_eq!(loaded.owner.username.as_deref(), Some("alice"));
        let none = store.load_session(&key("none")).await.unwrap().unwrap();
        assert!(none.subscriptions.is_empty());
        // Those that can't be read stay, and their session fails to load
        assert!(matches!(
            store.load_session(&key("bad")).await,
            Err(StorageError::SerializationError(_))
        ));
        let blobs: i64 = store
            .client()
            .await
            .unwrap()
            .query_one(
                "SELECT COUNT(*) FROM sessions WHERE length(subscriptions) > 0",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(blobs, 1);
    }

    /// A database written before the mount point was part of the key moves
    /// each session's rows under the mount point of its owner, and keeps
    /// rows without a session under none. Migrating again changes nothing.
    #[tokio::test]
    #[cfg_attr(not(postgres_tests), ignore = "set MERCURIO_TEST_POSTGRES_URL to run")]
    async fn test_migrates_rows_under_their_sessions_mount_point() {
        let url = database_url();
        let _database = DATABASE.lock().await;
        let keyed = MIGRATIONS.len() - 2;
        database_at(
            &url,
            keyed,
            "INSERT INTO sessions
                 (client_id, subscriptions, clean_start, expiry_interval, owner_username,
                  owner_mount_point)
                 VALUES ('a', '', FALSE, 60, 'alice', 'tenant-a/'),
                        ('b', '', FALSE, NULL, 'bob', NULL);
             INSERT INTO subscriptions VALUES ('a', 's/a', 1, FALSE, FALSE, NULL),
                                              ('b', 's/b', 0, FALSE, FALSE, NULL);
             INSERT INTO will_messages VALUES ('a', 'w/a', '', 0, FALSE, 0, '');
             INSERT INTO inflight_messages
                 (client_id, packet_id, topic, payload, qos, dup, sequence, released, properties)
                 VALUES ('a', 1, 'i/a', NULL, 1, FALSE, 1, FALSE, ''),
                        ('orphan', 1, 'i/o', NULL, 1, FALSE, 1, FALSE, '');
             INSERT INTO queued_messages
                 (client_id, sequence, topic, payload, qos, retain, properties)
                 VALUES ('a', 4, 'q/a', NULL, 1, FALSE, '');
             INSERT INTO incoming_qos2 VALUES ('a', 7), ('orphan', 8);",
        )
        .await;

        let store = PostgresStore::connect(&url, &ConnectOptions::default())
            .await
            .unwrap();
        let mounted = SessionKey::new(Some("tenant-a/".into()), "a");
        let check = |store: PostgresStore| {
            let mounted = mounted.clone();
            async move {
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
            }
        };
        check(store.clone()).await;

        // The same client id gets a session of its own on another mount
        // point, which running the migration again leaves alone
        store
            .apply(
                &key("a"),
                &[
                    Write::SaveSession(crate::testing::session("a")),
                    Write::StoreIncomingQos2(9),
                ],
            )
            .await
            .unwrap();
        let client = store.client().await.unwrap();
        client.batch_execute(MIGRATIONS[keyed]).await.unwrap();
        client
            .execute(
                "UPDATE mercurio_schema SET version = $1",
                &[&(keyed as i32)],
            )
            .await
            .unwrap();
        drop(client);
        let store = PostgresStore::connect(&url, &ConnectOptions::default())
            .await
            .unwrap();
        assert_eq!(store.load_incoming_qos2(&key("a")).await.unwrap(), [9]);
        store.delete_session(&key("a")).await.unwrap();
        check(store).await;
    }

    #[tokio::test]
    #[cfg_attr(not(postgres_tests), ignore = "set MERCURIO_TEST_POSTGRES_URL to run")]
    async fn test_conformance() {
        let url = database_url();
        let _database = DATABASE.lock().await;
        crate::testing::conformance(&empty_store(&url).await).await;
    }

    #[tokio::test]
    #[cfg_attr(not(postgres_tests), ignore = "set MERCURIO_TEST_POSTGRES_URL to run")]
    async fn test_bans() {
        let url = database_url();
        let _database = DATABASE.lock().await;
        let store = empty_store(&url).await;
        crate::testing::bans(&store).await;
        assert!(Arc::new(store).ban_store().is_some());
    }

    /// A failing statement undoes the whole batch, its deletion included.
    #[tokio::test]
    #[cfg_attr(not(postgres_tests), ignore = "set MERCURIO_TEST_POSTGRES_URL to run")]
    async fn test_batches_apply_atomically() {
        let url = database_url();
        let _database = DATABASE.lock().await;
        let store = empty_store(&url).await;
        store
            .apply(
                &key("c"),
                &[Write::SaveSession(crate::testing::session("c"))],
            )
            .await
            .unwrap();

        // Text can't hold NUL
        let mut bad = crate::testing::inflight_message(3);
        bad.topic = "\0".into();
        assert!(store
            .apply(
                &key("c"),
                &[Write::DeleteSession, Write::StoreInflight(bad)]
            )
            .await
            .is_err());
        assert!(store.load_session(&key("c")).await.unwrap().is_some());
    }

    /// Rows that can't be read fail to load as undecodable, each in the
    /// loads of its own client only, and stay in the database.
    #[tokio::test]
    #[cfg_attr(not(postgres_tests), ignore = "set MERCURIO_TEST_POSTGRES_URL to run")]
    async fn test_rows_that_cannot_be_read_are_an_error() {
        let url = database_url();
        let _database = DATABASE.lock().await;
        let store = empty_store(&url).await;
        for client_id in ["bad", "good"] {
            store
                .apply(
                    &key(client_id),
                    &[
                        Write::SaveSession(crate::testing::session(client_id)),
                        Write::StoreInflight(crate::testing::inflight_message(1)),
                        Write::Enqueue(1, crate::testing::message("q", "1")),
                    ],
                )
                .await
                .unwrap();
        }
        store
            .client()
            .await
            .unwrap()
            .batch_execute(
                "UPDATE subscriptions SET qos = 7 WHERE client_id = 'bad';
                 UPDATE inflight_messages SET packet_id = 70000 WHERE client_id = 'bad';
                 UPDATE queued_messages SET properties = '\\xff' WHERE client_id = 'bad';",
            )
            .await
            .unwrap();
        let undecodable =
            |result: Result<_>| matches!(result, Err(StorageError::SerializationError(_)));

        assert!(undecodable(
            store.load_session(&key("bad")).await.map(|_| ())
        ));
        assert!(undecodable(
            store.load_inflight(&key("bad")).await.map(|_| ())
        ));
        assert!(undecodable(store.load_queue(&key("bad")).await.map(|_| ())));
        assert!(store.load_session(&key("good")).await.unwrap().is_some());
        assert_eq!(store.load_inflight(&key("good")).await.unwrap().len(), 1);
        assert_eq!(store.load_queue(&key("good")).await.unwrap().len(), 1);
        let loaded = store.load_clients(None, 10).await.unwrap();
        let stored = |client_id: &str| {
            &loaded
                .iter()
                .find(|c| c.key == key(client_id))
                .unwrap()
                .stored
        };
        assert!(undecodable(stored("bad").as_ref().map(|_| ()).map_err(
            |e| {
                assert!(matches!(e, StorageError::SerializationError(_)), "{e}");
                StorageError::SerializationError(e.to_string())
            }
        )));
        assert!(stored("good").is_ok());

        store.delete_session(&key("bad")).await.unwrap();
        assert!(store.load_session(&key("bad")).await.unwrap().is_none());
    }

    /// A retained lookup only reads, so it doesn't wait for the lock a
    /// takeover holds on the lease, as a deletion through the fence trigger
    /// would on every SUBSCRIBE.
    #[tokio::test]
    #[cfg_attr(not(postgres_tests), ignore = "set MERCURIO_TEST_POSTGRES_URL to run")]
    async fn test_retained_lookups_take_no_lock() {
        let url = database_url();
        let _database = DATABASE.lock().await;
        let database = empty_store(&url).await;
        let (store, _lock) =
            PostgresStore::connect_exclusive(&url, &ConnectOptions::default(), false)
                .await
                .unwrap()
                .unwrap();
        let mut expired = crate::testing::message("a/x", "");
        expired.expires_at = Some(1);
        store.store_retained("a/x", Some(expired)).await.unwrap();

        let admin = database.client().await.unwrap();
        admin
            .batch_execute("BEGIN; SELECT * FROM mercurio_lease FOR UPDATE;")
            .await
            .unwrap();
        let found = tokio::time::timeout(Duration::from_secs(2), store.get_retained("a/#")).await;
        admin.batch_execute("ROLLBACK").await.unwrap();
        assert!(found
            .expect("the lookup waited for the lock")
            .unwrap()
            .is_empty());
        assert_eq!(store.remove_expired_retained().await.unwrap(), 1);
    }

    /// `url` with `user` and `password` in place of its credentials.
    fn url_as(url: &str, user: &str, password: &str) -> String {
        let (scheme, rest) = url.split_once("://").unwrap();
        let host = rest.split_once('@').map_or(rest, |(_, host)| host);
        format!("{scheme}://{user}:{password}@{host}")
    }

    /// Managed PostgreSQL often gives roles a statement or lock timeout,
    /// which would cancel a standby's wait for the lock.
    #[tokio::test]
    #[cfg_attr(not(postgres_tests), ignore = "set MERCURIO_TEST_POSTGRES_URL to run")]
    async fn test_a_standby_waits_despite_its_roles_timeouts() {
        let url = database_url();
        let _database = DATABASE.lock().await;
        let database = empty_store(&url).await;
        let admin = database.client().await.unwrap();
        admin
            .batch_execute(
                "DROP ROLE IF EXISTS mercurio_hurried;
                 CREATE ROLE mercurio_hurried LOGIN SUPERUSER PASSWORD 'hurried';
                 ALTER ROLE mercurio_hurried SET statement_timeout = '200ms';
                 ALTER ROLE mercurio_hurried SET lock_timeout = '200ms';",
            )
            .await
            .unwrap();
        let (_active, lock) =
            PostgresStore::connect_exclusive(&url, &ConnectOptions::default(), false)
                .await
                .unwrap()
                .unwrap();

        let hurried = url_as(&url, "mercurio_hurried", "hurried");
        let waiting = tokio::spawn(async move {
            PostgresStore::connect_exclusive(&hurried, &ConnectOptions::default(), true).await
        });
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert!(!waiting.is_finished(), "{:?}", waiting.await.unwrap().err());

        drop(lock);
        let (_, standby_lock) = tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("the standby takes over")
            .unwrap()
            .unwrap()
            .unwrap();
        drop(standby_lock);
        let _ = admin.batch_execute("DROP ROLE mercurio_hurried").await;
    }

    /// A newer standby leaves the schema alone until it holds the
    /// database, as the broker serving from it may not know the new one.
    #[tokio::test]
    #[cfg_attr(not(postgres_tests), ignore = "set MERCURIO_TEST_POSTGRES_URL to run")]
    async fn test_a_standby_migrates_once_it_holds_the_database() {
        let url = database_url();
        let _database = DATABASE.lock().await;
        let database = empty_store(&url).await;
        let admin = database.client().await.unwrap();
        let version = || async {
            let version: i32 = admin
                .query_one("SELECT version FROM mercurio_schema", &[])
                .await
                .unwrap()
                .get(0);
            version as usize
        };
        let (_active, lock) =
            PostgresStore::connect_exclusive(&url, &ConnectOptions::default(), false)
                .await
                .unwrap()
                .unwrap();
        // As if the serving broker were the version before the last change
        admin
            .execute(
                "UPDATE mercurio_schema SET version = $1",
                &[&(MIGRATIONS.len() as i32 - 1)],
            )
            .await
            .unwrap();

        let url_standby = url.clone();
        let waiting = tokio::spawn(async move {
            PostgresStore::connect_exclusive(&url_standby, &ConnectOptions::default(), true).await
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!waiting.is_finished());
        assert_eq!(version().await, MIGRATIONS.len() - 1);

        drop(lock);
        let (_, standby_lock) = tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("the standby takes over")
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(version().await, MIGRATIONS.len());
        drop(standby_lock);
    }

    /// A standby older than the database's schema fails at once, rather
    /// than wait for a database it can't serve from.
    #[tokio::test]
    #[cfg_attr(not(postgres_tests), ignore = "set MERCURIO_TEST_POSTGRES_URL to run")]
    async fn test_an_older_standby_refuses_a_newer_schema() {
        let url = database_url();
        let _database = DATABASE.lock().await;
        let database = empty_store(&url).await;
        let admin = database.client().await.unwrap();
        let (_active, _lock) =
            PostgresStore::connect_exclusive(&url, &ConnectOptions::default(), false)
                .await
                .unwrap()
                .unwrap();
        let set_version = |version: usize| {
            let admin = &admin;
            async move {
                admin
                    .execute(
                        "UPDATE mercurio_schema SET version = $1",
                        &[&(version as i32)],
                    )
                    .await
                    .unwrap();
            }
        };
        set_version(MIGRATIONS.len() + 1).await;
        let standby = tokio::time::timeout(
            Duration::from_secs(2),
            PostgresStore::connect_exclusive(&url, &ConnectOptions::default(), true),
        )
        .await
        .expect("an older standby doesn't wait");
        set_version(MIGRATIONS.len()).await;
        let error = standby.err().unwrap();
        assert!(error.to_string().contains("newer"), "{error}");
    }

    #[tokio::test]
    #[cfg_attr(not(postgres_tests), ignore = "set MERCURIO_TEST_POSTGRES_URL to run")]
    async fn test_one_broker_holds_the_database() {
        let url = database_url();
        let _database = DATABASE.lock().await;
        let database = empty_store(&url).await;

        let (active, lock) =
            PostgresStore::connect_exclusive(&url, &ConnectOptions::default(), false)
                .await
                .unwrap()
                .unwrap();
        let ban = crate::testing::ban("address", "10.0.0.0/8", false);
        active.store_ban(&ban).await.unwrap();
        assert!(
            PostgresStore::connect_exclusive(&url, &ConnectOptions::default(), false)
                .await
                .unwrap()
                .is_none()
        );
        let url_standby = url.clone();
        let waiting = tokio::spawn(async move {
            PostgresStore::connect_exclusive(&url_standby, &ConnectOptions::default(), true)
                .await
                .unwrap()
                .unwrap()
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!waiting.is_finished());

        // The server ends the holder's connection, as on a failover
        database
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
        let (standby, standby_lock) = tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("the standby takes over")
            .unwrap();
        assert!(!standby_lock.is_lost());

        // The old holder's writes are refused, the new holder's accepted
        assert!(matches!(
            active.apply(&key("c"), &[Write::ClearQueue]).await,
            Err(StorageError::Fenced)
        ));
        assert!(active.store_retained("t", None).await.is_err());
        standby
            .apply(&key("c"), &[Write::ClearQueue])
            .await
            .unwrap();

        // The standby finds the bans as it starts serving; the old holder
        // can't change them any more
        assert_eq!(standby.load_bans().await.unwrap(), vec![ban]);
        assert!(active
            .delete_ban("address", "10.0.0.0/8", false)
            .await
            .is_err());
        standby
            .delete_ban("address", "10.0.0.0/8", false)
            .await
            .unwrap();

        drop(standby_lock);
        let (_, lock) = PostgresStore::connect_exclusive(&url, &ConnectOptions::default(), false)
            .await
            .unwrap()
            .expect("dropping the lock frees the database");
        drop(lock);
    }

    /// A takeover the holder's lock connection hasn't noticed yet, as when
    /// that connection hangs: the first fenced write gives the lock up.
    #[tokio::test]
    #[cfg_attr(not(postgres_tests), ignore = "set MERCURIO_TEST_POSTGRES_URL to run")]
    async fn test_a_fenced_write_loses_the_lock() {
        let url = database_url();
        let _database = DATABASE.lock().await;
        let database = empty_store(&url).await;
        let (active, lock) =
            PostgresStore::connect_exclusive(&url, &ConnectOptions::default(), false)
                .await
                .unwrap()
                .unwrap();
        active.apply(&key("c"), &[Write::ClearQueue]).await.unwrap();
        assert!(!lock.is_lost());

        database
            .client()
            .await
            .unwrap()
            .batch_execute("UPDATE mercurio_lease SET epoch = epoch + 1")
            .await
            .unwrap();
        assert!(matches!(
            active.apply(&key("c"), &[Write::ClearQueue]).await,
            Err(StorageError::Fenced)
        ));
        assert!(lock.is_lost());
    }

    #[tokio::test]
    #[cfg_attr(not(postgres_tests), ignore = "set MERCURIO_TEST_POSTGRES_URL to run")]
    async fn test_errors_worth_retrying_are_transient() {
        let url = database_url();
        let _database = DATABASE.lock().await;
        let store = PostgresStore::connect(&url, &ConnectOptions::default())
            .await
            .unwrap();
        let client = store.client().await.unwrap();
        let raise = |code: &str| {
            format!("DO $$ BEGIN RAISE EXCEPTION 'test' USING ERRCODE = '{code}'; END $$")
        };
        for code in ["40001", "40P01", "57P01", "53300", "08006"] {
            let error = client.batch_execute(&raise(code)).await.db().unwrap_err();
            assert!(
                matches!(error, StorageError::Transient(_)),
                "{code}: {error}"
            );
        }
        let error = client
            .batch_execute(&raise("23505"))
            .await
            .db()
            .unwrap_err();
        assert!(matches!(error, StorageError::Database(_)), "{error}");

        // The server ends the connection, as on a failover
        let pid: i32 = client
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get(0);
        store
            .pool
            .get()
            .await
            .unwrap()
            .execute("SELECT pg_terminate_backend($1)", &[&pid])
            .await
            .unwrap();
        let error = client.batch_execute("SELECT 1").await.db().unwrap_err();
        assert!(matches!(error, StorageError::Transient(_)), "{error}");
    }

    /// A broker waiting for a free connection retries, rather than give
    /// its changes up.
    #[tokio::test]
    #[cfg_attr(not(postgres_tests), ignore = "set MERCURIO_TEST_POSTGRES_URL to run")]
    async fn test_the_pool_is_sized_and_its_wait_is_transient() {
        let url = database_url();
        let _database = DATABASE.lock().await;
        let options = ConnectOptions {
            pool_size: 1,
            pool_timeout: Duration::from_millis(100),
            ..Default::default()
        };
        let store = PostgresStore::connect(&url, &options).await.unwrap();
        assert_eq!(store.pool.status().max_size, 1);
        let _held = store.client().await.unwrap();
        let error = store.client().await.err().unwrap();
        assert!(matches!(error, StorageError::Transient(_)), "{error}");
    }

    #[test]
    fn test_remote_or_verified_connections_require_tls() {
        use tokio_postgres::config::SslMode;
        let mode = |url: &str, ca: bool| base_config(url, ca).unwrap().get_ssl_mode();
        for local in [
            "postgres://u@localhost/m",
            "postgres://u@127.0.0.1/m",
            "postgres://u@[::1]/m",
            "host=/var/run/postgresql user=u",
            "postgres://u@%2Fvar%2Frun%2Fpostgresql/m",
        ] {
            assert_eq!(mode(local, false), SslMode::Prefer, "{local}");
            assert_eq!(mode(local, true), SslMode::Require, "{local}");
        }
        assert_eq!(
            mode("postgres://u@db.example.com/m", false),
            SslMode::Require
        );
        assert_eq!(
            mode("host=localhost,db.example.com user=u", false),
            SslMode::Require
        );

        // libpq's verifying modes, which rustls's require already does
        for url in [
            "postgres://u@db.example.com/m?sslmode=verify-full",
            "postgres://u@localhost/m?connect_timeout=3&sslmode=verify-ca",
            "host=db.example.com sslmode=verify-full user=u",
        ] {
            assert_eq!(mode(url, false), SslMode::Require, "{url}");
        }

        // The operator's explicit choice stands
        assert_eq!(
            mode("postgres://u@db.example.com/m?sslmode=disable", true),
            SslMode::Disable
        );
        assert_eq!(
            mode("postgres://u@db.example.com/m?sslmode=prefer", false),
            SslMode::Prefer
        );
    }

    /// The server in the tests has TLS off, as PostgreSQL's images do.
    #[tokio::test]
    #[cfg_attr(not(postgres_tests), ignore = "set MERCURIO_TEST_POSTGRES_URL to run")]
    async fn test_a_ca_path_refuses_plaintext() {
        let url = database_url();
        let _database = DATABASE.lock().await;
        if url.contains("sslmode=") {
            return;
        }
        let options = ConnectOptions {
            ca_path: Some(
                concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../mercurio-server/tests/fixtures/cert.pem"
                )
                .into(),
            ),
            ..Default::default()
        };
        let store = PostgresStore::connect(&url, &ConnectOptions::default())
            .await
            .unwrap();
        let ssl: bool = store
            .client()
            .await
            .unwrap()
            .query_one("SELECT current_setting('ssl')::bool", &[])
            .await
            .unwrap()
            .get(0);
        if ssl {
            return;
        }
        assert!(PostgresStore::connect(&url, &options).await.is_err());
    }

    #[test]
    fn test_default_options() {
        let options = ConnectOptions::default();
        assert_eq!(options.pool_size, 16);
        assert_eq!(options.pool_timeout, Duration::from_secs(5));
        assert_eq!(options.ca_path, None);
    }

    #[tokio::test]
    async fn test_an_unreachable_database_is_transient() {
        let error = PostgresStore::connect(
            "postgres://mercurio@127.0.0.1:1/mercurio",
            &ConnectOptions::default(),
        )
        .await
        .err()
        .unwrap();
        assert!(matches!(error, StorageError::Transient(_)), "{error}");
    }
}
