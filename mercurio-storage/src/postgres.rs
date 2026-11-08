//! PostgreSQL storage backend.
//!
//! Keeps the broker's state in a PostgreSQL database, 12 or later, for
//! deployments that already run one or want the state off the broker's host.

use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use bytes::Bytes;
use deadpool_postgres::{Manager, Object, Pool, Runtime};
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
use tokio_postgres::{types::ToSql, Row};
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::{
    encoding::{
        decode_subscriptions, encode_properties, encode_subscriptions, int_to_qos, qos_to_int,
    },
    BatchStore, InflightMessage, InflightStore, QueueStore, Result, RetainedMessageStore,
    SessionState, SessionStore, StorageError, StoredWillMessage, WillStore, Write,
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
/// Released entries never change: a new schema change is a new entry.
const MIGRATIONS: &[&str] = &["
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
"];

/// Brings the schema up to date, one broker at a time.
async fn migrate(client: &tokio_postgres::Client) -> Result<()> {
    client
        .batch_execute(&format!(
            "SET client_min_messages = warning;
             BEGIN;
             SELECT pg_advisory_xact_lock({SCHEMA_LOCK_KEY});
             CREATE TABLE IF NOT EXISTS mercurio_schema (version INTEGER NOT NULL);"
        ))
        .await?;
    let result = async {
        let version: i32 = match client
            .query_opt("SELECT version FROM mercurio_schema", &[])
            .await?
        {
            Some(row) => row.try_get(0)?,
            None => {
                client
                    .execute("INSERT INTO mercurio_schema VALUES (0)", &[])
                    .await?;
                0
            }
        };
        let pending = MIGRATIONS.get(version as usize..).ok_or_else(|| {
            StorageError::OperationFailed(format!(
                "the database has schema version {version}, newer than this broker's {}",
                MIGRATIONS.len()
            ))
        })?;
        for migration in pending {
            client.batch_execute(migration).await?;
        }
        client
            .execute(
                "UPDATE mercurio_schema SET version = $1",
                &[&(MIGRATIONS.len() as i32)],
            )
            .await?;
        Ok(())
    }
    .await;
    client
        .batch_execute(if result.is_ok() { "COMMIT" } else { "ROLLBACK" })
        .await?;
    result
}

/// PostgreSQL storage backend implementing all storage traits, over a pool
/// of connections that are reopened when the server drops them.
#[derive(Clone)]
pub struct PostgresStore {
    pool: Pool,
}

/// The database, held by one broker at a time; see
/// [`PostgresStore::connect_exclusive`]. Dropping it releases the database.
pub struct DatabaseLock {
    heartbeat: JoinHandle<()>,
    lost: watch::Receiver<bool>,
}

impl DatabaseLock {
    /// Completes once the database may have been taken over by another
    /// broker, as the connection holding it closed or stopped answering.
    /// The store refuses writes from then on; the broker should stop.
    pub async fn lost(&self) {
        let _ = self.lost.clone().wait_for(|lost| *lost).await;
    }

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
    /// The connection is encrypted when the server supports it, or always
    /// with `?sslmode=require` in the URL, and never with `sslmode=disable`.
    /// The server's certificate is verified against `ca_path`, a PEM file,
    /// or else the Mozilla root certificates.
    ///
    /// Nothing stops another broker from using the database meanwhile; a
    /// broker serving clients should use [`Self::connect_exclusive`].
    pub async fn connect(url: &str, ca_path: Option<&Path>) -> Result<Self> {
        let config = base_config(url)?;
        let tls = tls(ca_path)?;
        let store = Self::with_pool(config, tls, None)?;
        let client = store.client().await?;
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
        ca_path: Option<&Path>,
        wait: bool,
    ) -> Result<Option<(Self, DatabaseLock)>> {
        let config = base_config(url)?;
        let tls = tls(ca_path)?;

        // The lock belongs to a connection of its own, outside the pool, so
        // it lasts as long as that connection
        let (client, connection) = config.connect(tls.clone()).await?;
        let (lost_tx, lost) = watch::channel(false);
        let closed = lost_tx.clone();
        tokio::spawn(async move {
            let _ = connection.await;
            let _ = closed.send(true);
        });
        // The database releases the lock within seconds of this host
        // vanishing, so a standby can take over
        client
            .batch_execute(
                "SET tcp_keepalives_idle = 5;
                 SET tcp_keepalives_interval = 2;
                 SET tcp_keepalives_count = 3;
                 SET tcp_user_timeout = 10000;",
            )
            .await?;
        migrate(&client).await?;

        let sql = if wait {
            "SELECT true FROM pg_advisory_lock($1)"
        } else {
            "SELECT pg_try_advisory_lock($1)"
        };
        if !client.query_one(sql, &[&LOCK_KEY]).await?.try_get(0)? {
            return Ok(None);
        }
        // Waits for writes of the previous holder already past their check
        let epoch: i64 = client
            .query_one(
                "WITH current AS (SELECT epoch FROM mercurio_lease FOR UPDATE)
                 UPDATE mercurio_lease SET epoch = (SELECT epoch FROM current) + 1
                 RETURNING epoch",
                &[],
            )
            .await?
            .try_get(0)?;

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
        let store = Self::with_pool(config, tls, Some(epoch))?;
        Ok(Some((store, DatabaseLock { heartbeat, lost })))
    }

    fn with_pool(
        mut config: tokio_postgres::Config,
        tls: MakeRustlsConnect,
        epoch: Option<i64>,
    ) -> Result<Self> {
        let mut options = format!(
            "{} -c statement_timeout={}",
            config.get_options().unwrap_or_default(),
            TIMEOUT.as_millis()
        );
        if let Some(epoch) = epoch {
            options.push_str(&format!(" -c mercurio.epoch={epoch}"));
        }
        config.options(options.trim());
        let pool = Pool::builder(Manager::new(config, tls))
            .runtime(Runtime::Tokio1)
            .wait_timeout(Some(TIMEOUT))
            .create_timeout(Some(TIMEOUT))
            .recycle_timeout(Some(TIMEOUT))
            .build()
            .map_err(|e| StorageError::OperationFailed(e.to_string()))?;
        Ok(PostgresStore { pool })
    }

    async fn client(&self) -> Result<Object> {
        self.pool
            .get()
            .await
            .map_err(|e| StorageError::OperationFailed(crate::error_chain(&e)))
    }

    async fn query(&self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<Vec<Row>> {
        let client = self.client().await?;
        let statement = client.prepare_cached(sql).await?;
        Ok(client.query(&statement, params).await?)
    }
}

/// The connection settings in `url`, with the timeouts and keepalives the
/// broker needs where the URL sets none.
fn base_config(url: &str) -> Result<tokio_postgres::Config> {
    let mut config: tokio_postgres::Config = url.parse()?;
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

/// The statements that apply `write` for `client_id`, other than those
/// applied in bulk.
fn statements(client_id: &str, write: &Write) -> Vec<(&'static str, Params)> {
    let id = || -> Box<dyn ToSql + Sync + Send> { Box::new(client_id.to_string()) };
    match write {
        Write::SaveSession(session) => vec![(
            "INSERT INTO sessions
             (client_id, subscriptions, clean_start, expiry_interval, expires_at)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (client_id) DO UPDATE SET
             subscriptions = EXCLUDED.subscriptions, clean_start = EXCLUDED.clean_start,
             expiry_interval = EXCLUDED.expiry_interval, expires_at = EXCLUDED.expires_at",
            vec![
                id(),
                Box::new(encode_subscriptions(&session.subscriptions).into_bytes()),
                Box::new(session.clean_start),
                Box::new(session.expiry_interval.map(i64::from)),
                Box::new(session.expires_at.map(|t| t as i64)),
            ],
        )],
        Write::SaveSubscriptions(subscriptions) => vec![(
            "INSERT INTO sessions (client_id, subscriptions, clean_start)
             VALUES ($1, $2, FALSE)
             ON CONFLICT (client_id) DO UPDATE SET subscriptions = EXCLUDED.subscriptions",
            vec![
                id(),
                Box::new(encode_subscriptions(subscriptions).into_bytes()),
            ],
        )],
        Write::SetExpiry {
            interval,
            expires_at,
        } => vec![(
            "UPDATE sessions SET expiry_interval = $2, expires_at = $3 WHERE client_id = $1",
            vec![
                id(),
                Box::new(i64::from(*interval)),
                Box::new(*expires_at as i64),
            ],
        )],
        Write::StoreWill(will) => vec![(
            "INSERT INTO will_messages
             (client_id, topic, payload, qos, retain, delay_secs, properties)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (client_id) DO UPDATE SET
             topic = EXCLUDED.topic, payload = EXCLUDED.payload, qos = EXCLUDED.qos,
             retain = EXCLUDED.retain, delay_secs = EXCLUDED.delay_secs,
             properties = EXCLUDED.properties",
            vec![
                id(),
                Box::new(will.topic.clone()),
                Box::new(will.payload.to_vec()),
                Box::new(qos_to_int(will.qos)),
                Box::new(will.retain),
                Box::new(i64::from(will.delay_secs)),
                Box::new(encode_properties(&will.properties)),
            ],
        )],
        Write::DeleteWill => vec![("DELETE FROM will_messages WHERE client_id = $1", vec![id()])],
        Write::StoreInflight(_)
        | Write::ReleaseInflight(_)
        | Write::RemoveInflight(_)
        | Write::Enqueue(..) => {
            unreachable!("applied in bulk by bulk_statements")
        }
        Write::ClearQueue => vec![(
            "DELETE FROM queued_messages WHERE client_id = $1",
            vec![id()],
        )],
        Write::DeleteSession => vec![
            ("DELETE FROM sessions WHERE client_id = $1", vec![id()]),
            ("DELETE FROM will_messages WHERE client_id = $1", vec![id()]),
            (
                "DELETE FROM queued_messages WHERE client_id = $1",
                vec![id()],
            ),
            (
                "DELETE FROM inflight_messages WHERE client_id = $1",
                vec![id()],
            ),
        ],
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
fn bulk_statements(client_id: &str, run: &[&Write]) -> Vec<(&'static str, Params)> {
    let mut fates: HashMap<u16, Fate> = HashMap::new();
    let mut queued: BTreeMap<u64, &Message> = BTreeMap::new();
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
                queued.insert(*sequence, message);
            }
            _ => unreachable!("only in-flight and queue changes run in bulk"),
        }
    }

    let id = || -> Box<dyn ToSql + Sync + Send> { Box::new(client_id.to_string()) };
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
            "DELETE FROM inflight_messages WHERE client_id = $1 AND packet_id = ANY($2)",
            vec![id(), Box::new(removed)],
        ));
    }
    let released = ids_of(|fate| matches!(fate, Fate::Released));
    if !released.is_empty() {
        statements.push((
            "UPDATE inflight_messages SET released = TRUE, payload = NULL
             WHERE client_id = $1 AND packet_id = ANY($2)",
            vec![id(), Box::new(released)],
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
             (client_id, packet_id, topic, payload, qos, dup, sequence, released, properties,
              subscription_identifiers)
             SELECT $1, * FROM unnest($2::INTEGER[], $3::TEXT[], $4::BYTEA[], $5::INTEGER[],
                                      $6::BOOLEAN[], $7::BIGINT[], $8::BOOLEAN[], $9::BYTEA[],
                                      $10::TEXT[])
             ON CONFLICT (client_id, packet_id) DO UPDATE SET
             topic = EXCLUDED.topic, payload = EXCLUDED.payload, qos = EXCLUDED.qos,
             dup = EXCLUDED.dup, sequence = EXCLUDED.sequence, released = EXCLUDED.released,
             properties = EXCLUDED.properties,
             subscription_identifiers = EXCLUDED.subscription_identifiers",
            vec![
                id(),
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
            ],
        ));
    }
    let queued: Vec<(&u64, &&Message)> = queued.iter().collect();
    if !queued.is_empty() {
        statements.push((
            "INSERT INTO queued_messages
             (client_id, sequence, topic, payload, qos, retain, properties, expires_at,
              subscription_identifiers)
             SELECT $1, * FROM unnest($2::BIGINT[], $3::TEXT[], $4::BYTEA[], $5::INTEGER[],
                                      $6::BOOLEAN[], $7::BYTEA[], $8::BIGINT[], $9::TEXT[])
             ON CONFLICT (client_id, sequence) DO UPDATE SET
             topic = EXCLUDED.topic, payload = EXCLUDED.payload, qos = EXCLUDED.qos,
             retain = EXCLUDED.retain, properties = EXCLUDED.properties,
             expires_at = EXCLUDED.expires_at,
             subscription_identifiers = EXCLUDED.subscription_identifiers",
            vec![
                id(),
                column(&queued, |(s, _)| **s as i64),
                column(&queued, |(_, m)| m.topic.to_string()),
                column(&queued, |(_, m)| m.payload.as_ref().map(|p| p.to_vec())),
                column(&queued, |(_, m)| qos_to_int(m.qos)),
                column(&queued, |(_, m)| m.retain),
                column(&queued, |(_, m)| encode_properties(&m.properties)),
                column(&queued, |(_, m)| m.expires_at.map(|t| t as i64)),
                column(&queued, |(_, m)| identifiers(&m.subscription_identifiers)),
            ],
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
            | Write::Enqueue(..)
    )
}

#[async_trait]
impl BatchStore for PostgresStore {
    /// Applies the batch in one transaction, its statements pipelined, so
    /// it costs about one round trip.
    async fn apply(&self, client_id: &str, writes: &[Write]) -> Result<()> {
        let mut statements = Vec::new();
        let mut run = Vec::new();
        for write in writes {
            if in_bulk(write) {
                run.push(write);
                continue;
            }
            statements.extend(bulk_statements(client_id, &run));
            run.clear();
            statements.extend(self::statements(client_id, write));
        }
        statements.extend(bulk_statements(client_id, &run));
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
            prepared.push((client.prepare_cached(sql).await?, params));
        }

        if let [(statement, params)] = prepared.as_slice() {
            client.execute(statement, params).await?;
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
        begun?;
        for result in executed {
            result?;
        }
        committed?;
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

/// A LIKE pattern for the topics a filter can match: those starting with
/// its part before the first wildcard. `a/#` also matches `a`, so a
/// trailing `/` is left out.
fn topic_prefix_pattern(filter: &str) -> String {
    let prefix = filter
        .split(['+', '#'])
        .next()
        .unwrap_or_default()
        .trim_end_matches('/');
    let mut pattern = String::with_capacity(prefix.len() + 1);
    for c in prefix.chars() {
        if matches!(c, '\\' | '%' | '_') {
            pattern.push('\\');
        }
        pattern.push(c);
    }
    pattern.push('%');
    pattern
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
        subscription_identifiers: decode_identifiers(row.try_get("subscription_identifiers")?)?,
    })
}

#[async_trait]
impl SessionStore for PostgresStore {
    async fn save_session(&self, client_id: &str, session: &SessionState) -> Result<()> {
        self.apply(client_id, &[Write::SaveSession(session.clone())])
            .await
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
        let client = self.client().await?;
        let statement = client
            .prepare_cached("DELETE FROM sessions WHERE client_id = $1")
            .await?;
        client.execute(&statement, &[&client_id]).await?;
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
        let client = self.client().await?;
        let Some(msg) = message else {
            let statement = client
                .prepare_cached("DELETE FROM retained_messages WHERE topic = $1")
                .await?;
            client.execute(&statement, &[&topic]).await?;
            return Ok(());
        };
        let origin = msg.origin.as_deref();
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
                 origin_username = EXCLUDED.origin_username",
            )
            .await?;
        client
            .execute(
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
            .await?;
        Ok(())
    }

    /// Reads only the topics sharing the filter's literal prefix, and drops
    /// the expired ones among them for good, in one round trip.
    async fn get_retained(&self, topic_filter: &str) -> Result<Vec<Message>> {
        let rows = self
            .query(
                "WITH expired AS (
                     DELETE FROM retained_messages WHERE topic LIKE $1 AND expires_at <= $2
                 )
                 SELECT topic, payload, qos, dup, properties, expires_at,
                        origin_client_id, origin_username
                 FROM retained_messages
                 WHERE topic LIKE $1 AND (expires_at IS NULL OR expires_at > $2)",
                &[&topic_prefix_pattern(topic_filter), &(unix_millis() as i64)],
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
        self.client()
            .await?
            .execute("DELETE FROM retained_messages", &[])
            .await?;
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
        self.apply(client_id, &[Write::StoreWill(will.clone())])
            .await
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
        self.apply(client_id, &[Write::DeleteWill]).await
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
        self.apply(client_id, &[Write::RemoveInflight(packet_id)])
            .await
    }

    async fn release_inflight(&self, client_id: &str, packet_id: u16) -> Result<()> {
        self.apply(client_id, &[Write::ReleaseInflight(packet_id)])
            .await
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
        self.apply(client_id, &[Write::Enqueue(sequence, message.clone())])
            .await
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
                )?,
                origin: None,
            };
            Ok((row.try_get::<_, i64>("sequence")? as u64, message))
        })
        .collect()
    }

    async fn clear_queue(&self, client_id: &str) -> Result<()> {
        self.apply(client_id, &[Write::ClearQueue]).await
    }
}

#[cfg(test)]
mod tests {
    use mercurio_core::{properties::ResponseTopic, qos::QoS};
    use tokio::sync::Mutex;

    use super::*;
    use crate::StoredSubscription;

    /// The tests share one database, and some take it over or empty it.
    static DATABASE: Mutex<()> = Mutex::const_new(());

    fn database_url() -> Option<String> {
        let url = std::env::var("MERCURIO_TEST_POSTGRES_URL").ok();
        if url.is_none() {
            eprintln!("MERCURIO_TEST_POSTGRES_URL not set, skipping");
        }
        url
    }

    async fn empty_store(url: &str) -> PostgresStore {
        let store = PostgresStore::connect(url, None).await.unwrap();
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
        store
    }

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

    fn inflight(packet_id: u16) -> InflightMessage {
        InflightMessage {
            packet_id,
            topic: "t".into(),
            payload: Some(Bytes::from_static(b"p")),
            qos: QoS::ExactlyOnce,
            dup: false,
            sequence: u64::MAX,
            released: false,
            properties: Default::default(),
            subscription_identifiers: vec![1, u32::MAX],
        }
    }

    #[test]
    fn test_topic_prefix_pattern() {
        assert_eq!(topic_prefix_pattern("#"), "%");
        assert_eq!(topic_prefix_pattern("a/#"), "a%");
        assert_eq!(topic_prefix_pattern("a/+/c"), "a%");
        assert_eq!(topic_prefix_pattern("a/b"), "a/b%");
        assert_eq!(topic_prefix_pattern("50%_off\\/+"), "50\\%\\_off\\\\%");
    }

    #[tokio::test]
    async fn test_round_trips_every_store() {
        let Some(url) = database_url() else { return };
        let _database = DATABASE.lock().await;
        let store = empty_store(&url).await;

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

        store
            .store_inflight("c", u16::MAX, &inflight(u16::MAX))
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

    #[tokio::test]
    async fn test_batches_apply_in_order_and_atomically() {
        let Some(url) = database_url() else { return };
        let _database = DATABASE.lock().await;
        let store = empty_store(&url).await;

        store
            .apply(
                "c",
                &[
                    Write::SaveSubscriptions(vec![StoredSubscription::new("a", QoS::AtMostOnce)]),
                    Write::SetExpiry {
                        interval: 60,
                        expires_at: 5,
                    },
                    Write::StoreInflight(inflight(1)),
                    Write::StoreInflight(inflight(2)),
                    Write::ReleaseInflight(2),
                    Write::RemoveInflight(1),
                    Write::Enqueue(1, message("q", "1")),
                ],
            )
            .await
            .unwrap();
        let session = store.load_session("c").await.unwrap().unwrap();
        assert_eq!(session.subscriptions.len(), 1);
        assert_eq!(
            (session.expiry_interval, session.expires_at),
            (Some(60), Some(5))
        );
        let inflight_left = store.get_all_inflight("c").await.unwrap();
        assert_eq!(inflight_left.len(), 1);
        assert!(inflight_left[0].released);

        // Rows stored by an earlier batch, then a run that reuses packet id
        // 1 and clears the queue between two enqueues
        store
            .apply(
                "c",
                &[
                    Write::StoreInflight(inflight(4)),
                    Write::StoreInflight(inflight(5)),
                ],
            )
            .await
            .unwrap();
        let mut reused = inflight(1);
        reused.sequence = 7;
        store
            .apply(
                "c",
                &[
                    Write::ReleaseInflight(4),
                    Write::RemoveInflight(5),
                    Write::StoreInflight(inflight(1)),
                    Write::RemoveInflight(1),
                    Write::StoreInflight(reused),
                    Write::ReleaseInflight(1),
                    Write::Enqueue(5, message("q", "dropped")),
                    Write::ClearQueue,
                    Write::Enqueue(6, message("q", "kept")),
                    Write::Enqueue(6, message("q", "kept, again")),
                ],
            )
            .await
            .unwrap();
        let mut left = store.get_all_inflight("c").await.unwrap();
        left.sort_by_key(|m| m.packet_id);
        let summary: Vec<_> = left
            .iter()
            .map(|m| (m.packet_id, m.sequence, m.released, m.payload.is_some()))
            .collect();
        assert_eq!(
            summary,
            [
                (1, 7, true, false),
                (2, u64::MAX, true, false),
                (4, u64::MAX, true, false)
            ]
        );
        let queue = store.load_queue("c").await.unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].0, 6);
        assert_eq!(queue[0].1.payload.as_deref(), Some(&b"kept, again"[..]));

        // A failing statement undoes the whole batch: text can't hold NUL
        let mut bad = inflight(3);
        bad.topic = "\0".into();
        assert!(store
            .apply("c", &[Write::DeleteSession, Write::StoreInflight(bad)])
            .await
            .is_err());
        assert!(store.load_session("c").await.unwrap().is_some());

        store.apply("c", &[Write::DeleteSession]).await.unwrap();
        assert!(store.load_session("c").await.unwrap().is_none());
        assert!(store.load_queue("c").await.unwrap().is_empty());
        assert!(store.get_all_inflight("c").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_retained_lookup_by_prefix() {
        let Some(url) = database_url() else { return };
        let _database = DATABASE.lock().await;
        let store = empty_store(&url).await;
        for topic in ["a", "a/b", "ab", "a%b/c", "axb/c", "$SYS/x"] {
            store
                .store_retained(topic, Some(message(topic, "")))
                .await
                .unwrap();
        }
        let topics = |filter: &'static str| {
            let store = store.clone();
            async move {
                let mut topics: Vec<_> = store
                    .get_retained(filter)
                    .await
                    .unwrap()
                    .iter()
                    .map(|m| m.topic.to_string())
                    .collect();
                topics.sort();
                topics
            }
        };
        assert_eq!(topics("a/#").await, ["a", "a/b"]);
        assert_eq!(topics("a%b/+").await, ["a%b/c"]);
        assert_eq!(topics("+/c").await, ["a%b/c", "axb/c"]);
        assert_eq!(topics("#").await.len(), 5);
    }

    #[tokio::test]
    async fn test_one_broker_holds_the_database() {
        let Some(url) = database_url() else { return };
        let _database = DATABASE.lock().await;

        let (active, lock) = PostgresStore::connect_exclusive(&url, None, false)
            .await
            .unwrap()
            .unwrap();
        assert!(PostgresStore::connect_exclusive(&url, None, false)
            .await
            .unwrap()
            .is_none());
        let url_standby = url.clone();
        let waiting = tokio::spawn(async move {
            PostgresStore::connect_exclusive(&url_standby, None, true)
                .await
                .unwrap()
                .unwrap()
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!waiting.is_finished());

        // The server ends the holder's connection, as on a failover
        empty_store(&url)
            .await
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
        assert!(active
            .apply("c", &[Write::ClearQueue])
            .await
            .unwrap_err()
            .to_string()
            .contains("another broker took over"));
        assert!(active.store_retained("t", None).await.is_err());
        standby.apply("c", &[Write::ClearQueue]).await.unwrap();

        drop(standby_lock);
        let (_, lock) = PostgresStore::connect_exclusive(&url, None, false)
            .await
            .unwrap()
            .expect("dropping the lock frees the database");
        drop(lock);
    }
}
