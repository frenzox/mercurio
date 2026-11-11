use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Weak},
};

use crate::error::Error;
use crate::CONNECTIONS;
use tokio::{
    sync::{watch, Mutex, OwnedMutexGuard},
    task::AbortHandle,
    time::{self, Duration},
};
use tracing::info;
use uuid::Uuid;

use mercurio_core::{
    message::{unix_millis, Message},
    protocol::ProtocolVersion,
    reason::ReasonCode,
};
use mercurio_packets::connect::ConnectPacket;
use mercurio_packets::connect::ConnectPayload;
use mercurio_storage::{
    BatchStore, InflightMessage, LoadedClient, RetainedMessageStore, SessionKey, SessionOwner,
    SessionState, StoredClient, StoredSubscription, StoredWillMessage, Write,
};
use tracing::{error, warn};

use crate::{
    broker::{Broker, Reoffer},
    plugin::{BoxFuture, Chain, Event},
    queue::QueueLimits,
    session::{Held, Session, SessionDropGuard, WillMessage},
    shutdown::Tasks,
    sys::Stats,
    writer::{Snapshots, StorageHealth, Writers},
};

pub(crate) struct SessionManager<S: BatchStore> {
    shared: Arc<Shared>,
    storage: Arc<S>,
    /// Every change to what is stored for a client goes through its writer
    writers: Writers<S>,
    queue_limits: QueueLimits,
    /// Where the sessions' queues count the messages they lose
    stats: Arc<Stats>,
    /// How long a new connection waits for the one it takes over to let go
    /// of the client id; forever when unset
    takeover_wait: Option<Duration>,
    /// Sessions outliving their connection the broker may hold
    max_sessions: Option<usize>,
    /// Where the manager's tasks run, such as expiry timers
    tasks: Tasks,
}

impl<S: BatchStore> Clone for SessionManager<S> {
    fn clone(&self) -> Self {
        SessionManager {
            shared: Arc::clone(&self.shared),
            storage: Arc::clone(&self.storage),
            writers: self.writers.clone(),
            queue_limits: self.queue_limits,
            stats: Arc::clone(&self.stats),
            takeover_wait: self.takeover_wait,
            max_sessions: self.max_sessions,
            tasks: self.tasks.clone(),
        }
    }
}

struct Shared {
    state: Mutex<State>,
    /// Clients whose session outlives their connection. Only theirs keep
    /// subscriptions and in-flight messages in storage, as others' are
    /// discarded when the connection ends.
    persistent: std::sync::Mutex<HashSet<SessionKey>>,
    /// Client ids in-process clients are connected with
    local: std::sync::Mutex<HashSet<SessionKey>>,
    /// Sessions ended by their expiry
    expired: std::sync::atomic::AtomicU64,
    /// Who hears about expired sessions, which belong to no connection
    expiry_chain: std::sync::OnceLock<Chain>,
    /// Takes the messages of a departing member of a share group
    share_groups: std::sync::OnceLock<Arc<dyn Reoffer>>,
}

struct State {
    sessions: HashMap<SessionKey, SessionDropGuard>,
    connections: HashMap<SessionKey, ConnectionSlot>,
    /// Source of ConnectionSlot::generation, unique across slots
    next_generation: u64,
    /// Clients whose will, left over from a previous run, is still due,
    /// with that will
    restored_wills: HashMap<SessionKey, StoredWillMessage>,
    /// The expiry timer that may end each session. Deleting or resuming a
    /// session, or scheduling a new timer for it, stops the timer.
    expiry_timers: HashMap<SessionKey, ExpiryTimer>,
    /// Held by a new connection while it checks whom its client id belongs
    /// to and claims it; present while one does
    admissions: HashMap<SessionKey, Arc<Mutex<()>>>,
    /// Clients whose stored session couldn't be read at restore: their
    /// rows stay in storage untouched, and their connections are refused
    /// until the session is deleted
    set_aside: HashSet<SessionKey>,
}

struct ExpiryTimer {
    token: u64,
    task: AbortHandle,
}

impl State {
    fn stop_expiry_timer(&mut self, key: &SessionKey) {
        if let Some(timer) = self.expiry_timers.remove(key) {
            timer.task.abort();
        }
    }

    fn entry(&self, key: &SessionKey, guard: &SessionDropGuard) -> ClientEntry {
        ClientEntry {
            key: key.clone(),
            session: guard.session(),
            connected: self.connections.contains_key(key),
            generation: self.connections.get(key).map(|slot| slot.holder.generation),
            restored_will: self.restored_wills.contains_key(key),
        }
    }

    /// Lets go of the claim of `generation` on `key`, wherever it
    /// stands. The latest claim hands the id back to the one it replaced,
    /// and the id is free once no claim is left.
    fn let_go(&mut self, key: &SessionKey, generation: u64) {
        let Some(slot) = self.connections.get_mut(key) else {
            return;
        };
        if slot.holder.generation != generation {
            slot.replaced
                .retain(|holder| holder.generation != generation);
        } else if let Some(previous) = slot.replaced.pop() {
            slot.holder = previous;
        } else {
            self.connections.remove(key);
        }
    }
}

/// Why a connection is closed from outside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Close {
    /// A new connection with the same client id took over
    TakenOver,
    /// The admin API closes it with `reason`; without `send_will` the will
    /// is dropped
    Admin { reason: ReasonCode, send_will: bool },
}

/// Records `reason` as the pending close. An administrator's pending close
/// isn't undone by a takeover, and one that drops the will always wins.
fn merge_close(pending: &mut Option<Close>, reason: Close) {
    let drops_will = matches!(
        reason,
        Close::Admin {
            send_will: false,
            ..
        }
    );
    if drops_will || !matches!(pending, Some(Close::Admin { .. })) {
        *pending = Some(reason);
    }
}

/// Why `delete_session` refused.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DeleteRefused {
    NotFound,
    Connected,
}

/// Tracks the network connection currently using a client id.
struct ConnectionSlot {
    /// Held by the active connection until it has fully shut down
    lease: Arc<Mutex<()>>,
    /// The latest connection to claim this client id
    holder: Holder,
    /// The claims `holder` replaced that are still around, oldest first:
    /// the connection being taken over and claims waiting for it. The id
    /// goes back to the latest of them when `holder` lets go of it.
    replaced: Vec<Holder>,
}

/// A connection's claim on a client id.
struct Holder {
    /// Tells the connection to close, and why
    close: watch::Sender<Option<Close>>,
    /// Identifies the connection
    generation: u64,
    /// Whom the connection starts its session for; None for the admin API
    owner: Option<SessionOwner>,
}

/// How many client ids new connections may be admitted with at once. Each
/// holds an entry in `State::admissions` until its owner is checked, so
/// connections cycling through fresh client ids while storage is slow
/// can't grow the map without bound.
const MAX_ADMISSIONS: usize = 65_536;

/// A new connection's place in `State::admissions`, removed once no other
/// connection with the same client id waits for it, also when the
/// connection is abandoned midway.
struct Admission<S: BatchStore + 'static> {
    manager: SessionManager<S>,
    key: SessionKey,
    /// Taken once removed
    lock: Option<Arc<Mutex<()>>>,
}

impl<S: BatchStore + 'static> Admission<S> {
    fn lock(&self) -> &Mutex<()> {
        self.lock
            .as_ref()
            .expect("an admission is held until it leaves")
    }

    async fn leave(mut self) {
        if let Some(lock) = self.lock.take() {
            self.manager.leave_admission(&self.key, lock).await;
        }
    }
}

impl<S: BatchStore + 'static> Drop for Admission<S> {
    fn drop(&mut self) {
        let Some(lock) = self.lock.take() else {
            return;
        };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let manager = self.manager.clone();
            let key = std::mem::take(&mut self.key);
            self.manager.tasks.spawn_on(
                async move { manager.leave_admission(&key, lock).await },
                &runtime,
            );
        }
    }
}

/// A place among the sessions `max_sessions` counts, given back if the
/// session doesn't start after all.
struct Reservation {
    shared: Arc<Shared>,
    key: SessionKey,
    armed: bool,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if self.armed {
            self.shared.persistent.lock().unwrap().remove(&self.key);
        }
    }
}

/// How `acquire_client_id` ended.
enum Acquired<S: BatchStore + 'static> {
    Lease(Claim<S>),
    /// The id belongs to the session of this other owner
    OwnedBy(SessionOwner),
    /// The connection using the id didn't let go of it in time
    Busy,
    /// Too many connections were checking their client id already
    Crowded,
}

/// Why `start_session` started no session.
#[derive(Debug)]
pub(crate) enum StartError {
    /// The client id belongs to the session of another owner, which is left
    /// as it was. Hands the CONNECT back to refuse it.
    OwnedByAnother(Box<ConnectPacket>),
    /// The connection using the client id didn't let go of it in time
    Busy,
    /// Too many connections were checking their client id at once
    Crowded,
    /// The session would be one more than `max_sessions`
    TooManySessions,
    /// The client's stored session couldn't be read, and was set aside
    SetAside,
    Storage(Error),
}

fn describe(owner: &SessionOwner) -> String {
    match &owner.username {
        Some(username) => format!("user `{username}`"),
        None => "an anonymous client".to_string(),
    }
}

/// A session as the admin API lists it.
pub(crate) struct ClientEntry {
    pub key: SessionKey,
    pub session: Session,
    pub connected: bool,
    /// Identifies the connection `connected` speaks of
    pub generation: Option<u64>,
    /// A will from before a restart waits for its delay
    pub restored_will: bool,
}

/// Ownership of a client id, under its mount point, for the lifetime of
/// one connection. Dropped without `end_connection`, as when the
/// connection's task panics, it ends the connection as its CONNECT asked,
/// so the client id isn't left claimed by a connection that is gone.
pub(crate) struct ConnectionLease {
    /// Taken once the client id is being given back
    guard: Option<OwnedMutexGuard<()>>,
    key: SessionKey,
    pub generation: u64,
    /// Changes when the connection is to close, for a takeover or the
    /// admin API
    pub close: watch::Receiver<Option<Close>>,
    on_drop: OnDrop,
    manager: Box<dyn Abandon>,
}

impl ConnectionLease {
    /// The session the connection uses.
    pub(crate) fn key(&self) -> &SessionKey {
        &self.key
    }
}

impl Drop for ConnectionLease {
    fn drop(&mut self) {
        let on_drop = std::mem::replace(&mut self.on_drop, OnDrop::Nothing);
        self.manager.abandon(
            self.guard.take(),
            std::mem::take(&mut self.key),
            self.generation,
            on_drop,
        );
    }
}

/// What is left to do for a claim or lease dropped midway.
#[derive(Clone, Copy)]
enum OnDrop {
    Nothing,
    /// Give the client id back
    LetGo,
    /// End the connection with this session expiry, then give the client
    /// id back
    End(Option<Duration>),
}

/// Finishes, in a task of its own, what a dropped claim or lease left
/// undone. Drop can't wait for the manager's lock.
trait Abandon: Send + Sync {
    fn abandon(
        &self,
        guard: Option<OwnedMutexGuard<()>>,
        key: SessionKey,
        generation: u64,
        on_drop: OnDrop,
    );
}

impl<S: BatchStore + 'static> Abandon for SessionManager<S> {
    fn abandon(
        &self,
        guard: Option<OwnedMutexGuard<()>>,
        key: SessionKey,
        generation: u64,
        on_drop: OnDrop,
    ) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let manager = self.clone();
        match on_drop {
            OnDrop::Nothing => {}
            OnDrop::LetGo => {
                drop(guard);
                self.tasks.spawn_on(
                    async move { manager.let_go(&key, generation).await },
                    &runtime,
                );
            }
            // The lease stays held until the session is dealt with, so a
            // connection waiting to take over finds it as it is left
            OnDrop::End(expiry) => {
                let end = async move {
                    manager.end_own_session(&key, generation, expiry).await;
                    drop(guard);
                    manager.let_go(&key, generation).await;
                };
                self.tasks.spawn_on(end, &runtime);
            }
        }
    }
}

/// A connection's claim on a client id until its session has started.
/// Let go of unless kept, also when the connection gives up or is
/// abandoned midway, so it leaves no slot behind that refuses other owners.
struct Claim<S: BatchStore + 'static> {
    manager: SessionManager<S>,
    key: SessionKey,
    generation: u64,
    close: watch::Receiver<Option<Close>>,
    /// Held once the connection that had the id before has finished
    guard: Option<OwnedMutexGuard<()>>,
    on_drop: OnDrop,
}

impl<S: BatchStore + 'static> Claim<S> {
    /// Turns the claim into the lease of a connection, which does
    /// `on_drop` if dropped before it is ended.
    fn keep(mut self, on_drop: OnDrop) -> ConnectionLease {
        self.on_drop = OnDrop::Nothing;
        ConnectionLease {
            guard: Some(self.guard.take().expect("a claim is kept once leased")),
            key: std::mem::take(&mut self.key),
            generation: self.generation,
            close: self.close.clone(),
            on_drop,
            manager: Box::new(self.manager.clone()),
        }
    }

    async fn let_go(mut self) {
        self.on_drop = OnDrop::Nothing;
        drop(self.guard.take());
        self.manager.let_go(&self.key, self.generation).await;
    }
}

impl<S: BatchStore + 'static> Drop for Claim<S> {
    fn drop(&mut self) {
        let on_drop = std::mem::replace(&mut self.on_drop, OnDrop::Nothing);
        self.manager.abandon(
            self.guard.take(),
            std::mem::take(&mut self.key),
            self.generation,
            on_drop,
        );
    }
}

impl<S: BatchStore + 'static> SessionManager<S> {
    /// The broker's manager: it waits `takeover_wait` for a connection to
    /// let go of a client id and holds up to `max_sessions` sessions that
    /// outlive their connection.
    pub(crate) fn for_broker(
        storage: Arc<S>,
        queue_limits: QueueLimits,
        takeover_wait: Duration,
        max_sessions: Option<usize>,
        stats: Arc<Stats>,
        tasks: Tasks,
    ) -> SessionManager<S> {
        let mut session_manager = SessionManager::with_queue_limits(storage, queue_limits, tasks);
        session_manager.takeover_wait = Some(takeover_wait);
        session_manager.max_sessions = max_sessions;
        session_manager.stats = stats;
        session_manager
    }
}

/// A session read from storage: its subscriptions, queue, in-flight
/// messages and the packet ids of incoming QoS 2 messages awaiting PUBREL.
type StoredSession = (
    Vec<StoredSubscription>,
    Vec<(u64, Message)>,
    Vec<InflightMessage>,
    Vec<u16>,
);

/// How many stored sessions restore reads at a time.
const RESTORE_PAGE: usize = 1000;

/// Result of starting a session, includes subscriptions, will, and inflight to restore if resuming.
pub(crate) struct SessionStartResult {
    pub session: Session,
    /// Whether an existing session was resumed
    pub resume: bool,
    /// Whether the server generated the client id
    pub assigned_client_id: bool,
    /// Topic filters to restore (re-subscribe to broker) if resuming a persisted session.
    pub subscriptions_to_restore: Vec<StoredSubscription>,
    /// Must be held until the connection has finished, will message included.
    pub lease: ConnectionLease,
    /// Inflight messages to retry (QoS 1/2 messages awaiting acknowledgment).
    pub inflight_to_retry: Vec<InflightMessage>,
}

impl<S: BatchStore + 'static> SessionManager<S> {
    #[cfg(test)]
    pub(crate) fn new(storage: Arc<S>) -> SessionManager<S> {
        Self::with_queue_limits(storage, QueueLimits::default(), Tasks::default())
    }

    /// A manager whose sessions' queues hold up to `queue_limits`, running
    /// its tasks in `tasks`.
    pub(crate) fn with_queue_limits(
        storage: Arc<S>,
        queue_limits: QueueLimits,
        tasks: Tasks,
    ) -> SessionManager<S> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                sessions: HashMap::new(),
                connections: HashMap::new(),
                next_generation: 0,
                restored_wills: HashMap::new(),
                expiry_timers: HashMap::new(),
                admissions: HashMap::new(),
                set_aside: HashSet::new(),
            }),
            persistent: Default::default(),
            local: Default::default(),
            expired: Default::default(),
            expiry_chain: Default::default(),
            share_groups: Default::default(),
        });

        let writers = Writers::new(Arc::clone(&storage), tasks.clone());
        writers.set_snapshots(Box::new(StoredState {
            shared: Arc::downgrade(&shared),
            durable: storage.is_durable(),
        }));
        SessionManager {
            shared,
            writers,
            storage,
            queue_limits,
            stats: Default::default(),
            takeover_wait: None,
            max_sessions: None,
            tasks,
        }
    }

    /// Batches of changes retried and given up since the broker started,
    /// and clients whose stored state is dirty until it is stored whole
    /// again.
    pub(crate) fn storage_health(&self) -> StorageHealth {
        self.writers.health()
    }

    /// Starts or resumes the session of the client in `connect_packet`,
    /// authenticated as `owner`, under the mount point of its listener,
    /// `owner.mount_point`: the same client id under another mount point is
    /// another session. A client id belongs to the user of its session for
    /// as long as that session exists: a client of another user is refused,
    /// Clean Start or not, and the session and any connection using it are
    /// left alone.
    /// Fails as busy when the connection using the client id doesn't let go
    /// of it within `takeover_wait`; that connection is still asked to
    /// close, and nothing else changes.
    /// The session outlives its connection by `expiry`, forever if None:
    /// what the server applies, which for MQTT 3.x clients comes from its
    /// configuration, so storage holds that from the start.
    pub(crate) async fn start_session(
        &mut self,
        mut connect_packet: ConnectPacket,
        owner: SessionOwner,
        expiry: Option<Duration>,
    ) -> std::result::Result<SessionStartResult, StartError> {
        // Clients without an identifier each get a unique one, so they never
        // share a session entry
        let assigned_client_id = connect_packet.payload.client_id.is_empty();
        if assigned_client_id {
            connect_packet.payload.client_id = Uuid::new_v4().hyphenated().to_string();
        }

        let expiry_interval = expiry.map(|e| e.as_secs().try_into().unwrap_or(u32::MAX));
        let persistent = expiry_interval != Some(0);

        let storage_error =
            |e: mercurio_storage::StorageError| StartError::Storage(Error::Storage(e));
        let key = SessionKey::new(
            owner.mount_point.clone(),
            connect_packet.payload.client_id.clone(),
        );
        if self.shared.state.lock().await.set_aside.contains(&key) {
            warn!(
                "Refusing client {}: its stored session couldn't be read and was set aside \
                 until it is deleted",
                key
            );
            return Err(StartError::SetAside);
        }
        let mut claim = match self
            .acquire_client_id(&key, &owner)
            .await
            .map_err(storage_error)?
        {
            Acquired::Lease(claim) => claim,
            Acquired::Busy => return Err(StartError::Busy),
            Acquired::Crowded => return Err(StartError::Crowded),
            Acquired::OwnedBy(holder) => {
                warn!(
                    "Client {} belongs to {}, refusing a connection from {}",
                    key,
                    describe(&holder),
                    describe(&owner)
                );
                return Err(StartError::OwnedByAnother(Box::new(connect_packet)));
            }
        };

        // The claim's lease keeps other connections with this client id and
        // the admin API away from its session until this returns, so the
        // manager's lock is only taken to look the session up and to put it
        // in place
        let generation = claim.generation;
        let Some(mut reservation) = self.reserve_session(&key, persistent) else {
            warn!("Too many sessions, refusing a new one for client {}", key);
            claim.let_go().await;
            return Err(StartError::TooManySessions);
        };
        let clean_start = connect_packet.clean_start;
        let existing = self
            .shared
            .state
            .lock()
            .await
            .sessions
            .get(&key)
            .map(SessionDropGuard::session);

        // Marks the previous connection's pending will as superseded, whether
        // this connection resumes the session or replaces it
        if let Some(existing) = &existing {
            // The session's expiry timer stops seeing it as the previous
            // connection's from here on, so abandoned, the start ends the
            // session as this connection would have
            claim.on_drop = OnDrop::End(expiry);
            existing.hand_over(generation).await;
        }

        let superseded = existing.clone();
        let resumed = existing.filter(|_| !clean_start);
        let mut stored = None;
        if !clean_start && resumed.is_none() {
            // Failing to read storage fails the connection: resuming
            // without it would lose messages
            match self.load_stored(&key).await {
                Ok(found) => stored = found,
                Err(e) => {
                    claim.let_go().await;
                    return Err(storage_error(e));
                }
            }
        }
        let resume = resumed.is_some() || stored.is_some();
        let (subscriptions_to_restore, queue_to_restore, inflight_to_retry, incoming_qos2) =
            stored.unwrap_or_default();

        let from_memory = resumed.is_some();
        let session = match resumed {
            Some(mut session) => {
                session.set_connect_packet(connect_packet).await;
                session
            }
            None => SessionDropGuard::new(
                connect_packet,
                self.queue_limits,
                Arc::clone(&self.stats),
                self.tasks.clone(),
            )
            .session(),
        };
        session.hand_over(generation).await;
        session.set_owner(owner.clone()).await;
        session.set_expiry(expiry).await;
        session.restore_incoming_qos2(incoming_qos2).await;

        claim.on_drop = OnDrop::End(expiry);
        let discarded = {
            let mut manager = self.shared.state.lock().await;
            // Reconnecting cancels a will left over from before a restart
            manager.restored_wills.remove(&key);
            // The previous connection scheduled its timer before letting go
            // of the client id, so this stops it
            manager.stop_expiry_timer(&key);
            match from_memory {
                true => None,
                false => manager.sessions.insert(
                    key.clone(),
                    SessionDropGuard::guarding(session.clone(), self.tasks.clone()),
                ),
            }
        };
        if let Some(discarded) = discarded {
            discarded.session().stop_queue_journal();
            discarded.session().unsubscribe_all().await;
            self.hand_back(&key, &discarded.session(), true).await;
        }
        if clean_start {
            self.writers.write(&key, Write::DeleteSession);
        }

        // The queue of a session outliving its connection is kept in
        // storage, connected or not, so a crash loses none of it; a stored
        // one restored here is there already
        let journaled = persistent && self.storage.is_durable();
        let restored = !queue_to_restore.is_empty();
        session.restore_queue(queue_to_restore);
        if journaled {
            session.start_queue_journal(self.writers.get(&key), restored);
        } else {
            session.stop_queue_journal();
            if restored {
                self.writers.write(&key, Write::ClearQueue);
            }
        }

        {
            let mut persistent_clients = self.shared.persistent.lock().unwrap();
            if persistent {
                persistent_clients.insert(key.clone());
            } else {
                persistent_clients.remove(&key);
            }
            reservation.armed = false;
        }
        // A session resumed from memory already holds its subscriptions; one
        // loaded from storage gets them restored after this returns
        let mut subscriptions = session.get_subscriptions().await;
        subscriptions.extend(subscriptions_to_restore.iter().cloned());
        let session_state = SessionState {
            client_id: key.client_id.clone(),
            subscriptions,
            expiry_interval,
            expires_at: None,
            owner,
        };
        self.writers.write(&key, Write::SaveSession(session_state));

        // The will of this CONNECT replaces any earlier one
        match session.get_will().await {
            Some(will) => {
                let stored_will: StoredWillMessage = will.into();
                self.writers.write(&key, Write::StoreWill(stored_will));
            }
            None => self.writers.write(&key, Write::DeleteWill),
        }

        // The client is back: the will its previous connection left
        // waiting is no longer due [MQTT-3.1.3-9]
        if let Some(superseded) = &superseded {
            superseded.drop_pending_will().await;
        }

        Ok(SessionStartResult {
            session,
            resume,
            assigned_client_id,
            subscriptions_to_restore,
            lease: claim.keep(OnDrop::End(expiry)),
            inflight_to_retry,
        })
    }

    /// `start_session` with the expiry the CONNECT asks for.
    #[cfg(test)]
    pub(crate) async fn start_session_as_asked(
        &mut self,
        connect_packet: ConnectPacket,
        owner: SessionOwner,
    ) -> std::result::Result<SessionStartResult, StartError> {
        let expiry = session_expiry(&connect_packet);
        self.start_session(connect_packet, owner, expiry).await
    }

    /// Counts the session of `key` among those outliving their
    /// connection, if it is one and isn't counted already. None when that
    /// would make one more than `max_sessions`. Sessions ending with their
    /// connection aren't counted: `max_connections` bounds them.
    fn reserve_session(&self, key: &SessionKey, persistent: bool) -> Option<Reservation> {
        let mut reservation = Reservation {
            shared: Arc::clone(&self.shared),
            key: key.clone(),
            armed: false,
        };
        let mut sessions = self.shared.persistent.lock().unwrap();
        if !persistent || sessions.contains(key) {
            return Some(reservation);
        }
        if self.max_sessions.is_some_and(|max| sessions.len() >= max) {
            return None;
        }
        sessions.insert(key.clone());
        reservation.armed = true;
        Some(reservation)
    }

    /// The session of `key` in storage, with its queue and in-flight
    /// messages, once what an earlier session wrote there is in place.
    async fn load_stored(
        &self,
        key: &SessionKey,
    ) -> mercurio_storage::Result<Option<StoredSession>> {
        self.writers.settle(key).await;
        let Some(stored_session) = self.storage.load_session(key).await? else {
            return Ok(None);
        };
        info!(
            "Restoring session for client {} with {} subscriptions",
            key,
            stored_session.subscriptions.len()
        );
        let queue = self.storage.load_queue(key).await?;
        let inflight = self.storage.load_inflight(key).await?;
        if !inflight.is_empty() {
            info!(
                "Restoring {} inflight messages for client {}",
                inflight.len(),
                key
            );
        }
        let incoming_qos2 = self.storage.load_incoming_qos2(key).await?;
        Ok(Some((
            stored_session.subscriptions,
            queue,
            inflight,
            incoming_qos2,
        )))
    }

    /// Takes over the client id of `key`, under its mount point, for a
    /// connection of `owner`: asks any connection already using it to close
    /// [MQTT-3.1.4-3] and waits until it has, so the two never share session
    /// state. Gives up with the owner the id belongs to, touching nothing,
    /// when that is another user, and as busy when the connection didn't
    /// close within `takeover_wait`. The mount point is part of the key, so
    /// only the user names are compared.
    ///
    /// New connections with one key check its owner and claim it one at a
    /// time, holding the key's admission lock, while the manager's lock is
    /// free for other clients during the storage read. Only an admitted
    /// claim gives a key an owner where it had none; every other change
    /// keeps the owner or removes it. The owner of a pending claim counts
    /// before the session it is about to take. Of two clients of different
    /// users connecting at once, the second is then refused even when the
    /// first has yet to start its session.
    async fn acquire_client_id(
        &self,
        key: &SessionKey,
        owner: &SessionOwner,
    ) -> mercurio_storage::Result<Acquired<S>> {
        let Some(admission) = self.admit(key).await else {
            warn!(
                "Too many connections checking their client id at once, refusing {}",
                key
            );
            return Ok(Acquired::Crowded);
        };
        let admitted = admission.lock().lock().await;
        let checked = async {
            if let Some(holder) = self.holder(key).await? {
                if holder.username != owner.username {
                    return Ok(Err(holder));
                }
            }
            let mut manager = self.shared.state.lock().await;
            let claim = self.claim(&mut manager, key, Close::TakenOver, Some(owner.clone()));
            mercurio_storage::Result::Ok(Ok(claim))
        }
        .await;
        drop(admitted);
        admission.leave().await;
        let (claim, lease, _) = match checked? {
            Ok(claimed) => claimed,
            Err(holder) => return Ok(Acquired::OwnedBy(holder)),
        };
        Ok(
            match self.wait_for_lease(claim, lease, self.takeover_wait).await {
                Some(claim) => Acquired::Lease(claim),
                None => Acquired::Busy,
            },
        )
    }

    /// The admission lock a new connection with `key` holds while it
    /// checks whom the id belongs to and claims it. None when too many
    /// connections are being admitted already.
    async fn admit(&self, key: &SessionKey) -> Option<Admission<S>> {
        let mut manager = self.shared.state.lock().await;
        let lock = match manager.admissions.get(key) {
            Some(lock) => Arc::clone(lock),
            None if manager.admissions.len() >= MAX_ADMISSIONS => return None,
            None => Arc::clone(manager.admissions.entry(key.clone()).or_default()),
        };
        Some(Admission {
            manager: self.clone(),
            key: key.clone(),
            lock: Some(lock),
        })
    }

    /// Removes the admission lock of `key` once no other connection
    /// holds or waits for it.
    async fn leave_admission(&self, key: &SessionKey, lock: Arc<Mutex<()>>) {
        // Only taken from the map under the manager's lock, so no other
        // connection waits for it when the map and this one hold the last
        // references
        let mut manager = self.shared.state.lock().await;
        if Arc::strong_count(&lock) == 2 {
            manager.admissions.remove(key);
        }
    }

    /// Whom `key` belongs to: the owner of the latest connection to
    /// claim it, else of its session, in memory or in storage.
    async fn holder(&self, key: &SessionKey) -> mercurio_storage::Result<Option<SessionOwner>> {
        let session = {
            let state = self.shared.state.lock().await;
            if let Some(owner) = state
                .connections
                .get(key)
                .and_then(|slot| slot.holder.owner.clone())
            {
                return Ok(Some(owner));
            }
            state.sessions.get(key).map(SessionDropGuard::session)
        };
        if let Some(session) = session {
            return Ok(Some(session.owner().await));
        }
        // What an earlier session wrote, its removal included, is in place
        // before the stored one is read
        self.writers.settle(key).await;
        Ok(self
            .storage
            .load_session(key)
            .await?
            .map(|stored| stored.owner))
    }

    /// Like `acquire_client_id` for the admin API, closing a connection for
    /// `reason` whoever it belongs to. With `only_if_free` gives up, touching
    /// nothing, when a connection holds the client id. The check and the
    /// claim happen under one lock, so no connection is closed by a claim it
    /// wasn't meant to close. Also tells whether a connection held the id.
    /// With a `wait`, gives up too once the connection holding the id hasn't
    /// let go of it in that time; it is still asked to close.
    async fn claim_client_id(
        &self,
        key: &SessionKey,
        reason: Close,
        only_if_free: bool,
        wait: Option<Duration>,
    ) -> Option<(ConnectionLease, bool)> {
        let (claim, lease, held) = {
            let mut manager = self.shared.state.lock().await;
            if only_if_free && manager.connections.contains_key(key) {
                return None;
            }
            self.claim(&mut manager, key, reason, None)
        };
        let claim = self.wait_for_lease(claim, lease, wait).await?;
        Some((claim.keep(OnDrop::LetGo), held))
    }

    /// Waits until the connection that held the id before `claim` has
    /// finished with `lease`. With a `wait`, gives up once it hasn't in that
    /// time and lets go of the claim, handing the id back to the claim it
    /// replaced if it is still the latest.
    async fn wait_for_lease(
        &self,
        mut claim: Claim<S>,
        lease: Arc<Mutex<()>>,
        wait: Option<Duration>,
    ) -> Option<Claim<S>> {
        let guard = match wait {
            Some(wait) => match time::timeout(wait, lease.lock_owned()).await {
                Ok(guard) => guard,
                Err(_) => {
                    claim.let_go().await;
                    return None;
                }
            },
            None => lease.lock_owned().await,
        };
        claim.guard = Some(guard);
        Some(claim)
    }

    /// Makes a new connection of `owner` the one holding `key` and
    /// asks the one holding it until now to close for `reason`. The claim
    /// lets go of the id if dropped from here on; it gets the id once the
    /// returned lease is free. Also tells whether a connection held the id.
    fn claim(
        &self,
        state: &mut State,
        key: &SessionKey,
        reason: Close,
        owner: Option<SessionOwner>,
    ) -> (Claim<S>, Arc<Mutex<()>>, bool) {
        let (close_tx, close) = watch::channel(None);
        state.next_generation += 1;
        let generation = state.next_generation;
        let holder = Holder {
            close: close_tx,
            generation,
            owner,
        };
        let (lease, held) = match state.connections.entry(key.clone()) {
            std::collections::hash_map::Entry::Occupied(mut e) => {
                let slot = e.get_mut();
                slot.holder
                    .close
                    .send_modify(|pending| merge_close(pending, reason));
                let previous = std::mem::replace(&mut slot.holder, holder);
                slot.replaced.push(previous);
                (Arc::clone(&slot.lease), true)
            }
            std::collections::hash_map::Entry::Vacant(e) => {
                let lease = Arc::new(Mutex::new(()));
                e.insert(ConnectionSlot {
                    lease: Arc::clone(&lease),
                    holder,
                    replaced: Vec::new(),
                });
                (lease, false)
            }
        };
        let claim = Claim {
            manager: self.clone(),
            key: key.clone(),
            generation,
            close,
            guard: None,
            on_drop: OnDrop::LetGo,
        };
        (claim, lease, held)
    }

    /// Releases the client id once its connection has finished and ends the
    /// session after `expiry`, unless a new connection resumes it first.
    /// `None` keeps the session forever.
    pub(crate) async fn end_connection(
        &self,
        mut lease: ConnectionLease,
        expiry: Option<Duration>,
    ) {
        // Dropped midway, the lease starts over in a task of its own
        lease.on_drop = OnDrop::End(expiry);
        self.end_own_session(&lease.key, lease.generation, expiry)
            .await;
        self.release(&mut lease).await;
    }

    /// What ending its connection does to the session of `key` while
    /// the connection of `generation` still holds the client id. Done
    /// before the id is released, so all of it comes before anything a
    /// connection taking over does, in memory and in storage.
    async fn end_own_session(&self, key: &SessionKey, generation: u64, expiry: Option<Duration>) {
        // Ending before releasing the id means a connection waiting to take
        // over can never observe a session that is about to vanish
        if expiry == Some(Duration::ZERO) {
            self.end_session(key, generation, None).await;
        } else if self.storage.is_durable() {
            // Messages for the client now wait in its queue until it returns;
            // keep them on disk meanwhile, as they are already unless the
            // session was to end with the connection, and unless another
            // connection took the session over
            let manager = self.shared.state.lock().await;
            if let Some(session) = manager.sessions.get(key).map(|s| s.session()) {
                if session.generation() == generation {
                    session.start_queue_journal(self.writers.get(key), false);
                }
            }
        }
        if let Some(expiry) = expiry.filter(|e| !e.is_zero()) {
            if self.schedule_end(key.clone(), generation, expiry).await {
                // Recorded so a restarted broker still expires the session
                self.writers.write(
                    key,
                    Write::SetExpiry {
                        interval: expiry.as_secs() as u32,
                        expires_at: unix_millis() + expiry.as_millis() as u64,
                    },
                );
            }
        }
    }

    /// Gives the client id back once the connection holding `lease` has
    /// finished with it.
    async fn release(&self, lease: &mut ConnectionLease) {
        lease.on_drop = OnDrop::LetGo;
        drop(lease.guard.take());
        self.let_go(&lease.key, lease.generation).await;
        lease.on_drop = OnDrop::Nothing;
    }

    async fn let_go(&self, key: &SessionKey, generation: u64) {
        self.shared.state.lock().await.let_go(key, generation);
    }

    /// Asks the connection using `key` to close. False if none does.
    pub(crate) async fn close_connection(&self, key: &SessionKey, close: Close) -> bool {
        let manager = self.shared.state.lock().await;
        let Some(slot) = manager.connections.get(key) else {
            return false;
        };
        slot.holder
            .close
            .send_modify(|pending| merge_close(pending, close));
        true
    }

    /// Like `close_connection`, but only if the connection using
    /// `key` is still the one of `generation`.
    pub(crate) async fn close_generation(
        &self,
        key: &SessionKey,
        generation: u64,
        close: Close,
    ) -> bool {
        let manager = self.shared.state.lock().await;
        let Some(slot) = manager
            .connections
            .get(key)
            .filter(|slot| slot.holder.generation == generation)
        else {
            return false;
        };
        slot.holder
            .close
            .send_modify(|pending| merge_close(pending, close));
        true
    }

    /// Asks the connection holding `lease` to close, unless a newer one took
    /// its client id since.
    pub(crate) async fn close_own(&self, lease: &ConnectionLease, close: Close) {
        self.close_generation(&lease.key, lease.generation, close)
            .await;
    }

    /// Removes the session of `key` with everything stored for it,
    /// its will included. A connected client is refused unless
    /// `disconnect`, which closes it first with Administrative Action and
    /// without its will.
    pub(crate) async fn delete_session(
        &self,
        key: &SessionKey,
        disconnect: bool,
    ) -> std::result::Result<(), DeleteRefused> {
        // Holding the client id like a new connection would: the current
        // one closes first, and one arriving meanwhile waits for the removal.
        // Without `disconnect` the claim fails instead of closing a client.
        let close = Close::Admin {
            reason: ReasonCode::AdministrativeAction,
            send_will: false,
        };
        let Some((mut lease, was_connected)) =
            self.claim_client_id(key, close, !disconnect, None).await
        else {
            return Err(DeleteRefused::Connected);
        };
        let generation = lease.generation;
        let (removed, was_restored) = {
            let mut manager = self.shared.state.lock().await;
            manager.stop_expiry_timer(key);
            // A session set aside is only in storage
            let was_restored =
                manager.restored_wills.remove(key).is_some() | manager.set_aside.remove(key);
            (manager.sessions.remove(key), was_restored)
        };
        if let Some(removed) = &removed {
            let session = removed.session();
            // A will waiting for its delay sees a newer generation and
            // stays unpublished
            session.set_generation(generation).await;
            session.stop_queue_journal();
            session.unsubscribe_all().await;
            self.hand_back(key, &session, true).await;
        }
        let removed = removed.is_some() || was_restored;
        if removed {
            self.shared.persistent.lock().unwrap().remove(key);
            self.writers.write(key, Write::DeleteSession);
            self.writers.close(key);
        }
        self.release(&mut lease).await;
        // A clean session is gone by the time the closed connection lets go
        if !removed && !was_connected {
            return Err(DeleteRefused::NotFound);
        }
        if removed {
            info!(target: CONNECTIONS, "Session for client {} deleted", key);
        }
        Ok(())
    }

    pub(crate) fn set_expiry_chain(&self, chain: Chain) {
        let _ = self.shared.expiry_chain.set(chain);
    }

    /// Ends the session of `key` after `delay`, unless a connection
    /// newer than `generation` uses it in the meantime. False if the
    /// session was gone or taken over already.
    async fn schedule_end(&self, key: SessionKey, generation: u64, delay: Duration) -> bool {
        let mut manager = self.shared.state.lock().await;
        // The session may be gone already, deleted while this connection
        // was closing, and a deleted session never expires. Generation 0
        // is a session restored from storage, which may have no
        // in-memory session left to find.
        if generation != 0 {
            let current = match manager.sessions.get(&key) {
                Some(guard) => guard.session().generation() == generation,
                None => false,
            };
            if !current {
                return false;
            }
        }
        manager.next_generation += 1;
        let token = manager.next_generation;
        let ender = self.clone();
        let ended = key.clone();
        let task = self.tasks.spawn(async move {
            tokio::time::sleep(delay).await;
            if ender.end_session(&ended, generation, Some(token)).await {
                ender
                    .shared
                    .expired
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if let Some(chain) = ender.shared.expiry_chain.get() {
                    chain.emit(Event::SessionExpired {
                        client_id: ended.client_id,
                        mount_point: ended.mount_point,
                    });
                }
            }
        });
        // Once the server has stopped, sessions expire when a restarted
        // broker says so
        if let Some(task) = task {
            let timer = ExpiryTimer { token, task };
            if let Some(replaced) = manager.expiry_timers.insert(key, timer) {
                replaced.task.abort();
            }
        }
        true
    }

    /// Schedules the expiry of sessions persisted by a previous run, and
    /// discards those that expired while the broker was down. Returns the
    /// wills those sessions left behind, with when they are due.
    pub(crate) async fn restore_from_storage<B: RetainedMessageStore>(
        &self,
        broker: &Broker<B>,
    ) -> mercurio_storage::Result<Vec<(SessionKey, WillMessage, Duration)>> {
        let now = unix_millis();
        let mut wills = Vec::new();
        let mut after: Option<SessionKey> = None;
        loop {
            let page = self
                .storage
                .load_clients(after.as_ref(), RESTORE_PAGE)
                .await?;
            let last_page = page.len() < RESTORE_PAGE;
            after = page.last().map(|loaded| loaded.key.clone());
            for LoadedClient { key, stored } in page {
                match stored {
                    Ok(stored) => wills.extend(self.restore_client(key, stored, now, broker).await),
                    Err(e) => self.set_aside(&key, &e.to_string()).await,
                }
            }
            if last_page {
                return Ok(wills);
            }
        }
    }

    /// Restores a session a previous run stored, if it hasn't expired, and
    /// schedules its end. Returns its will, with when it is due.
    async fn restore_client<B: RetainedMessageStore>(
        &self,
        key: SessionKey,
        stored: StoredClient,
        now: u64,
        broker: &Broker<B>,
    ) -> Option<(SessionKey, WillMessage, Duration)> {
        let StoredClient {
            session: state,
            will,
            queue,
            inflight,
            incoming_qos2,
        } = stored;
        // Sessions connected when the broker stopped count as disconnected
        // from now on
        let deadline = state
            .expires_at
            .or_else(|| state.expiry_interval.map(|i| now + u64::from(i) * 1000));
        let until_expiry = deadline.map(|d| Duration::from_millis(d.saturating_sub(now)));

        // Live sessions go back in memory, subscribed, so messages keep
        // being queued for their clients until they reconnect
        if deadline.is_none_or(|d| d > now) {
            self.restore_session(&key, state, (queue, inflight, incoming_qos2), broker)
                .await;
        }

        // The clients were disconnected by the shutdown, so their wills are
        // due after their delay, or when the session ends if sooner
        let due_will = will.map(|stored| {
            let will = WillMessage::from(stored.clone());
            let due = until_expiry.map_or(will.delay, |e| e.min(will.delay));
            (stored, will, due)
        });
        if let Some((stored, ..)) = &due_will {
            self.shared
                .state
                .lock()
                .await
                .restored_wills
                .insert(key.clone(), stored.clone());
        }

        if let Some(delay) = until_expiry {
            // Generation 0 predates every connection of this run
            self.schedule_end(key.clone(), 0, delay).await;
        }
        due_will.map(|(_, will, due)| (key, will, due))
    }

    /// Leaves the stored session of `key` as it is, unrestored, and
    /// refuses its client until an operator deletes the session.
    async fn set_aside(&self, key: &SessionKey, reason: &str) {
        error!(
            "The stored session of client {} can't be read, setting it aside: {}. Its \
             client is refused until the session is deleted with the admin API, such as \
             `mercurioctl client delete-session`",
            key, reason
        );
        self.shared.state.lock().await.set_aside.insert(key.clone());
    }

    /// How many stored sessions were set aside as they couldn't be read.
    pub(crate) async fn set_aside_count(&self) -> usize {
        self.shared.state.lock().await.set_aside.len()
    }

    /// Recreates the in-memory state of a session persisted by a previous
    /// run, whose client has not reconnected yet.
    async fn restore_session<B: RetainedMessageStore>(
        &self,
        key: &SessionKey,
        state: SessionState,
        (queue, inflight, incoming_qos2): (Vec<(u64, Message)>, Vec<InflightMessage>, Vec<u16>),
        broker: &Broker<B>,
    ) {
        // Stands in for the client's CONNECT until it sends a new one
        let connect_packet = ConnectPacket {
            protocol_version: ProtocolVersion::V5,
            clean_start: false,
            keepalive: 0,
            properties: None,
            payload: ConnectPayload {
                client_id: key.client_id.clone(),
                ..Default::default()
            },
        };
        let mut session = SessionDropGuard::new(
            connect_packet,
            self.queue_limits,
            Arc::clone(&self.stats),
            self.tasks.clone(),
        )
        .session();
        // The session is found under the mount point it was stored with
        let owner = SessionOwner {
            mount_point: key.mount_point.clone(),
            ..state.owner
        };
        session.set_owner(owner).await;
        let expires_at = state.expires_at.or_else(|| {
            state
                .expiry_interval
                .map(|i| unix_millis() + u64::from(i) * 1000)
        });
        let expiry = state.expiry_interval.map(|i| Duration::from_secs(i.into()));
        session.mark_restored(expiry, expires_at).await;

        if let Err(e) = session
            .restore_subscriptions(state.subscriptions, broker)
            .await
        {
            error!("Failed to restore subscriptions of {}: {}", key, e);
        }
        session.restore_queue(queue);
        session.restore_inflight(inflight).await;
        session.restore_incoming_qos2(incoming_qos2).await;
        session.start_queue_journal(self.writers.get(key), true);
        self.shared.persistent.lock().unwrap().insert(key.clone());

        self.shared.state.lock().await.sessions.insert(
            key.clone(),
            SessionDropGuard::guarding(session, self.tasks.clone()),
        );
    }

    /// Waits until every change made so far is in storage, as on shutdown.
    pub(crate) async fn flush(&self) {
        self.writers.flush_all().await;
    }

    /// How many sessions ended by their expiry.
    pub(crate) fn expired_count(&self) -> u64 {
        self.shared
            .expired
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// How many sessions the broker holds, connected or not.
    pub(crate) async fn session_count(&self) -> usize {
        self.shared.state.lock().await.sessions.len()
    }

    /// Every session, connected or not.
    pub(crate) async fn entries(&self) -> Vec<ClientEntry> {
        let manager = self.shared.state.lock().await;
        manager
            .sessions
            .iter()
            .map(|(key, guard)| manager.entry(key, guard))
            .collect()
    }

    /// The keys `client_id` has a session, set aside or not, or a
    /// connection under, sorted: one per mount point.
    pub(crate) async fn keys_of(&self, client_id: &str) -> Vec<SessionKey> {
        let manager = self.shared.state.lock().await;
        let mut keys: Vec<SessionKey> = manager
            .sessions
            .keys()
            .chain(manager.connections.keys())
            .chain(manager.set_aside.iter())
            .filter(|key| key.client_id == client_id)
            .cloned()
            .collect();
        keys.sort();
        keys.dedup();
        keys
    }

    pub(crate) async fn entry(&self, key: &SessionKey) -> Option<ClientEntry> {
        let manager = self.shared.state.lock().await;
        let guard = manager.sessions.get(key)?;
        Some(manager.entry(key, guard))
    }

    /// Claims a will returned by `restore_from_storage` for publishing.
    /// False if its client reconnected in the meantime.
    pub(crate) async fn take_restored_will(&self, key: &SessionKey) -> bool {
        self.shared
            .state
            .lock()
            .await
            .restored_wills
            .remove(key)
            .is_some()
    }

    /// Removes from storage a will `take_restored_will` claimed, now that
    /// it is published, with its session if that ended meanwhile.
    pub(crate) async fn restored_will_published(&self, key: &SessionKey) {
        let manager = self.shared.state.lock().await;
        if manager.sessions.contains_key(key) || manager.connections.contains_key(key) {
            self.writers.write(key, Write::DeleteWill);
        } else {
            self.writers.write(key, Write::DeleteSession);
            self.writers.close(key);
        }
    }

    /// Discards the session of `key` unless a connection newer than
    /// `generation` has used it since. An expiry timer passes its `timer`
    /// token, and does nothing once deletion or a newer timer retired it.
    /// Without a timer, the connection of `generation` ends its own session,
    /// even when a new one waits to take it over [MQTT-3.1.2-6].
    async fn end_session(&self, key: &SessionKey, generation: u64, timer: Option<u64>) -> bool {
        let mut manager = self.shared.state.lock().await;
        if let Some(token) = timer {
            if manager.expiry_timers.get(key).map(|t| t.token) != Some(token) {
                return false;
            }
            manager.expiry_timers.remove(key);
        }
        let connected_since = timer.is_some()
            && manager
                .connections
                .get(key)
                .is_some_and(|slot| slot.holder.generation != generation);
        let resumed_since = match manager.sessions.get(key) {
            Some(session) => session.session().generation() != generation,
            None => false,
        };
        if connected_since || resumed_since {
            return false;
        }

        info!(target: CONNECTIONS, "Session for client {} ended", key);
        let ended = manager.sessions.remove(key);
        self.shared.persistent.lock().unwrap().remove(key);
        // A will left by a previous run stays stored with its session until
        // it is published, so stopping before then doesn't lose it
        if !manager.restored_wills.contains_key(key) {
            // Queued while the session is still out of reach, so a
            // connection reading storage for it finds it removed
            self.writers.write(key, Write::DeleteSession);
            self.writers.close(key);
        }
        drop(manager);
        if let Some(ended) = ended {
            ended.session().stop_queue_journal();
            ended.session().unsubscribe_all().await;
            self.hand_back(key, &ended.session(), true).await;
        }
        true
    }

    /// Where the messages of a departing member of a share group go.
    pub(crate) fn set_share_groups(&self, groups: Arc<dyn Reoffer>) {
        let _ = self.shared.share_groups.set(groups);
    }

    /// Gives what reached `session`, of `key`, through shared subscriptions
    /// and may go to another member back to the groups. Once the session
    /// has `ended`, any other member may take it, and what none takes is
    /// lost. Otherwise, as the client has only lost its connection, only
    /// a connected member may, and what one takes leaves the session, in
    /// storage too; the rest waits for the client.
    async fn hand_back(&self, key: &SessionKey, session: &Session, ended: bool) {
        let Some(groups) = self.shared.share_groups.get() else {
            return;
        };
        for handback in session.handbacks().await {
            let except = session.subscriber_id();
            if !groups.reoffer(&handback.group, handback.message, except, ended) || ended {
                continue;
            }
            if session.give_up(handback.held).await {
                if let Held::Unacknowledged(packet_id) = handback.held {
                    self.save_changes(key, [Write::RemoveInflight(packet_id)])
                        .await;
                }
            }
        }
    }

    /// Gives the messages a client that lost its connection holds through
    /// shared subscriptions to the members connected; see `hand_back`.
    pub(crate) async fn hand_back_on_disconnect(&self, key: &SessionKey, session: &Session) {
        self.hand_back(key, session, false).await;
    }

    pub(crate) fn hold_locally(&self, key: &SessionKey) {
        self.shared.local.lock().unwrap().insert(key.clone());
    }

    pub(crate) fn release_locally(&self, key: &SessionKey) {
        self.shared.local.lock().unwrap().remove(key);
    }

    /// Whether an in-process client is connected with `key`.
    pub(crate) fn is_held_locally(&self, key: &SessionKey) -> bool {
        self.shared.local.lock().unwrap().contains(key)
    }

    fn is_persistent(&self, key: &SessionKey) -> bool {
        self.storage.is_durable() && self.shared.persistent.lock().unwrap().contains(key)
    }

    /// Stores what the client's packets changed, one write each, for a
    /// session that outlives its connection.
    pub(crate) async fn save_changes(
        &self,
        key: &SessionKey,
        changes: impl IntoIterator<Item = Write>,
    ) {
        if self.is_persistent(key) {
            for change in changes {
                self.writers.write_paced(key, Box::new([change])).await;
            }
        }
    }

    /// Delete persisted will message for a client.
    /// Called when will is cleared (clean disconnect) or published (abnormal disconnect).
    pub(crate) fn delete_will(&self, key: &SessionKey) {
        self.writers.write(key, Write::DeleteWill);
    }

    /// Stores a QoS 1 or 2 message sent and awaiting acknowledgment,
    /// removing it from the stored queue, if it was at `dequeued` there, in
    /// the same batch: it is in one place or the other, whenever the broker
    /// stops.
    pub(crate) async fn store_inflight(
        &self,
        key: &SessionKey,
        message: &InflightMessage,
        dequeued: Option<u64>,
    ) {
        let writes: Box<[Write]> = match (self.is_persistent(key), dequeued) {
            (true, Some(sequence)) => Box::new([
                Write::StoreInflight(message.clone()),
                Write::Dequeue(sequence),
            ]),
            (true, None) => Box::new([Write::StoreInflight(message.clone())]),
            (false, Some(sequence)) => Box::new([Write::Dequeue(sequence)]),
            (false, None) => return,
        };
        self.writers.write_paced(key, writes).await;
    }

    /// Removes a message dropped from the queue, rather than sent, from
    /// the stored queue.
    pub(crate) async fn dequeue(&self, key: &SessionKey, sequence: u64) {
        self.writers
            .write_paced(key, Box::new([Write::Dequeue(sequence)]))
            .await;
    }
}

/// Takes what the broker holds for a client, for its writer to store it
/// whole.
struct StoredState {
    shared: Weak<Shared>,
    durable: bool,
}

impl Snapshots for StoredState {
    fn snapshot<'a>(&'a self, key: &'a SessionKey) -> BoxFuture<'a, Option<Vec<Write>>> {
        Box::pin(async move {
            let shared = self.shared.upgrade()?;
            let (session, restored_will) = {
                let state = shared.state.lock().await;
                (
                    state.sessions.get(key).map(SessionDropGuard::session),
                    state.restored_wills.get(key).cloned(),
                )
            };
            let persistent = self.durable && shared.persistent.lock().unwrap().contains(key);
            let mut writes = vec![Write::DeleteSession];
            if let Some(session) = session {
                writes.extend(session.stored_state(persistent).await);
            }
            // A will from before a restart, still due
            if let Some(will) = restored_will {
                if !writes.iter().any(|w| matches!(w, Write::StoreWill(_))) {
                    writes.push(Write::StoreWill(will));
                }
            }
            Some(writes)
        })
    }
}

/// How long a session outlives its connection; `None` means forever.
pub(crate) fn session_expiry(connect_packet: &ConnectPacket) -> Option<Duration> {
    if !connect_packet.protocol_version.supports_properties() {
        // MQTT 3.x: a clean session ends with the connection, others persist
        return connect_packet.clean_start.then_some(Duration::ZERO);
    }

    let interval = connect_packet
        .properties
        .as_ref()
        .and_then(|p| p.session_expiry_interval.as_ref())
        .map_or(0, |p| p.value);

    (interval != u32::MAX).then(|| Duration::from_secs(interval.into()))
}

#[cfg(test)]
mod tests;
