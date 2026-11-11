//! Writes each client's state to storage in order, off the paths that
//! serve the client.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::Duration,
};

use mercurio_storage::{BatchStore, StorageError, Write};
use tokio::{
    sync::{mpsc, oneshot, watch, Semaphore},
    time::Instant,
};
use tracing::{error, info, warn};

use crate::plugin::BoxFuture;

/// Most changes applied in one batch.
const MAX_BATCH: usize = 256;

/// How long a writer waits for more changes before stopping. Writers
/// exist while there is something to write, so idle clients, most of them,
/// keep no task, channel or buffer.
const LINGER: Duration = Duration::from_secs(1);

/// Attempts at a batch failing for good before its changes are given up.
const ATTEMPTS: u32 = 5;

/// Changes a client's connection may have waiting for storage before it
/// waits too, so a broker never runs ahead of its storage.
const PENDING_LIMIT: usize = 10_000;

/// Wait before the first attempt to store a dirty client's state whole,
/// unless a change for it comes sooner; doubled after each failure, up to
/// `MAX_RECONCILE_DELAY`.
const RECONCILE_DELAY: Duration = Duration::from_secs(1);

const MAX_RECONCILE_DELAY: Duration = Duration::from_secs(60);

/// What the broker holds for each client, for storing it whole.
pub(crate) trait Snapshots: Send + Sync {
    /// The writes that replace everything stored for `client_id` with what
    /// the broker holds for it, the first of them `Write::DeleteSession`;
    /// None once the broker holds nothing to take them from.
    fn snapshot<'a>(&'a self, client_id: &'a str) -> BoxFuture<'a, Option<Vec<Write>>>;
}

/// How the writers of one broker fared with storage.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StorageHealth {
    /// Batches attempted again after a failure
    pub retried: u64,
    /// Batches given up on, the client's state then stored whole instead
    pub dropped: u64,
    /// Clients whose stored state differs from the broker's until it is
    /// stored whole again
    pub dirty: usize,
}

/// How storage has been failing the writers.
#[derive(Default)]
struct Health {
    /// Clients whose stored state is dirty: a batch of their changes was
    /// given up, so storage holds something else than the broker until
    /// their state is stored whole again
    dirty: Mutex<HashSet<String>>,
    /// Batches attempted again after a failure since the broker started
    retried: AtomicU64,
    /// Batches given up since the broker started
    failed: AtomicU64,
    snapshots: OnceLock<Box<dyn Snapshots>>,
}
pub(crate) enum Op {
    /// Changes that go to storage in the same batch, with whether they hold
    /// a pending-change permit. Boxed: the channel allocates its slots
    /// ahead, and every client has one.
    Write(Box<[Write]>, bool),
    /// Answered once every change sent before it is applied
    Flush(oneshot::Sender<()>),
}

/// Sends a client's changes to its writer.
#[derive(Clone)]
pub(crate) struct Writer {
    ops: mpsc::UnboundedSender<Op>,
    pending: Arc<Semaphore>,
}

impl Writer {
    fn new(ops: mpsc::UnboundedSender<Op>) -> Self {
        Writer {
            ops,
            pending: Arc::new(Semaphore::new(PENDING_LIMIT)),
        }
    }

    /// Sends a change without waiting, for those bounded by other limits,
    /// such as the queue's.
    pub(crate) fn write(&self, write: Write) {
        let _ = self.ops.send(Op::Write(Box::new([write]), false));
    }

    /// Sends changes, applied in one batch, once fewer than `PENDING_LIMIT`
    /// are waiting for storage. Boxed already, so the wait doesn't carry
    /// them whole.
    pub(crate) async fn write_paced(&self, writes: Box<[Write]>) {
        let Ok(permit) = self.pending.acquire().await else {
            return;
        };
        permit.forget();
        if self.ops.send(Op::Write(writes, true)).is_err() {
            self.pending.add_permits(1);
        }
    }

    pub(crate) async fn flush(&self) {
        let (flushed, done) = oneshot::channel();
        if self.ops.send(Op::Flush(flushed)).is_ok() {
            let _ = done.await;
        }
    }

    #[cfg(test)]
    pub(crate) fn channel() -> (Self, mpsc::UnboundedReceiver<Op>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (Writer::new(sender), receiver)
    }
}

struct Entry {
    /// None once closed: the writer finishes what it was sent, then stops
    writer: Option<Writer>,
    /// True once the writer has stopped
    stopped: watch::Receiver<bool>,
    id: u64,
}

/// One writer per client, so a client's changes reach storage in the
/// order they were made. A client's new writer starts once its previous
/// one has stopped.
pub(crate) struct Writers<S> {
    storage: Arc<S>,
    entries: Arc<Mutex<HashMap<String, Entry>>>,
    next_id: Arc<AtomicU64>,
    health: Arc<Health>,
}

impl<S> Clone for Writers<S> {
    fn clone(&self) -> Self {
        Writers {
            storage: Arc::clone(&self.storage),
            entries: Arc::clone(&self.entries),
            next_id: Arc::clone(&self.next_id),
            health: Arc::clone(&self.health),
        }
    }
}

impl<S: BatchStore + 'static> Writers<S> {
    pub(crate) fn new(storage: Arc<S>) -> Self {
        Writers {
            storage,
            entries: Default::default(),
            next_id: Default::default(),
            health: Default::default(),
        }
    }

    /// Where the state of a client whose changes were given up is taken
    /// from, to store it whole. Until set, such a client stays dirty.
    pub(crate) fn set_snapshots(&self, snapshots: Box<dyn Snapshots>) {
        let _ = self.health.snapshots.set(snapshots);
    }

    /// Batches retried and given up since the broker started, and clients
    /// whose stored state is dirty because of them.
    pub(crate) fn health(&self) -> StorageHealth {
        StorageHealth {
            retried: self.health.retried.load(Ordering::Relaxed),
            dropped: self.health.failed.load(Ordering::Relaxed),
            dirty: self.health.dirty.lock().unwrap().len(),
        }
    }

    /// The client's writer, started if it has none.
    pub(crate) fn get(&self, client_id: &str) -> Writer {
        let mut entries = self.entries.lock().unwrap();
        if let Some(writer) = entries.get(client_id).and_then(|e| e.writer.clone()) {
            return writer;
        }
        let previous = entries.get(client_id).map(|e| e.stopped.clone());
        let (sender, receiver) = mpsc::unbounded_channel();
        let (stopped_tx, stopped) = watch::channel(false);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let writer = Writer::new(sender);
        entries.insert(
            client_id.to_string(),
            Entry {
                writer: Some(writer.clone()),
                stopped,
                id,
            },
        );
        tokio::spawn(run(
            Arc::clone(&writer.pending),
            Arc::clone(&self.storage),
            client_id.to_string(),
            receiver,
            previous,
            stopped_tx,
            Arc::clone(&self.entries),
            id,
            Arc::clone(&self.health),
        ));
        writer
    }

    pub(crate) fn write(&self, client_id: &str, write: Write) {
        self.get(client_id).write(write);
    }

    pub(crate) async fn write_paced(&self, client_id: &str, writes: Box<[Write]>) {
        self.get(client_id).write_paced(writes).await;
    }

    /// Lets the client's writer stop once it has applied what it was sent.
    pub(crate) fn close(&self, client_id: &str) {
        if let Some(entry) = self.entries.lock().unwrap().get_mut(client_id) {
            entry.writer = None;
        }
    }

    /// Waits until the client's earlier changes are in storage.
    pub(crate) async fn settle(&self, client_id: &str) {
        let (writer, stopped) = match self.entries.lock().unwrap().get(client_id) {
            Some(entry) => (entry.writer.clone(), entry.stopped.clone()),
            None => return,
        };
        match writer {
            Some(writer) => writer.flush().await,
            None => {
                let _ = stopped.clone().wait_for(|stopped| *stopped).await;
            }
        }
    }

    /// Waits until every change made so far is in storage, and makes one
    /// more attempt to store the state of dirty clients whole.
    pub(crate) async fn flush_all(&self) {
        let client_ids: Vec<String> = self.entries.lock().unwrap().keys().cloned().collect();
        for client_id in client_ids {
            self.settle(&client_id).await;
        }
        let dirty: Vec<String> = self.health.dirty.lock().unwrap().iter().cloned().collect();
        for client_id in dirty {
            self.get(&client_id).flush().await;
        }
    }
}

/// Applies a client's changes in order. Once a batch is given up, the
/// client is dirty: its changes are dropped as they come, as storing its
/// state whole supersedes them, which is tried at the next change or flush
/// and then again after a growing delay, until it succeeds.
#[allow(clippy::too_many_arguments)]
async fn run<S: BatchStore>(
    pending: Arc<Semaphore>,
    storage: Arc<S>,
    client_id: String,
    mut ops: mpsc::UnboundedReceiver<Op>,
    previous: Option<watch::Receiver<bool>>,
    stopped: watch::Sender<bool>,
    entries: Arc<Mutex<HashMap<String, Entry>>>,
    id: u64,
    health: Arc<Health>,
) {
    if let Some(mut previous) = previous {
        let _ = previous.wait_for(|stopped| *stopped).await;
    }
    let mut batch = Vec::new();
    let mut flushed = Vec::new();
    let mut dirty = health.dirty.lock().unwrap().contains(&client_id);
    let mut reconcile_delay = RECONCILE_DELAY;
    let mut next_reconcile = Instant::now();
    // Held by something besides the map, such as a queue's journal, when
    // last idle
    let mut held = false;
    loop {
        let received = match (dirty, held) {
            (true, _) => {
                let wait = next_reconcile.saturating_duration_since(Instant::now());
                tokio::time::timeout(wait, ops.recv()).await
            }
            // What holds it sends a last change as it lets go, such as the
            // journal's ClearQueue, so there is no need to look again before
            (false, true) => Ok(ops.recv().await),
            (false, false) => tokio::time::timeout(LINGER, ops.recv()).await,
        };
        held = false;
        let op = match received {
            Ok(Some(op)) => Some(op),
            Ok(None) => {
                if dirty {
                    reconcile(storage.as_ref(), &client_id, &health).await;
                }
                break;
            }
            Err(_) if dirty => None,
            Err(_) => {
                // Idle: stop, unless something besides the map, such as a
                // queue's journal, still holds this writer. A later change
                // starts a new one.
                let mut entries = entries.lock().unwrap();
                let only_the_map = entries
                    .get(&client_id)
                    .filter(|entry| entry.id == id)
                    .and_then(|entry| entry.writer.as_ref())
                    .is_some_and(|writer| writer.ops.strong_count() == 1);
                if only_the_map && ops.is_empty() {
                    entries.remove(&client_id);
                    break;
                }
                held = true;
                continue;
            }
        };
        let mut next = op;
        let mut permits = 0;
        while let Some(op) = next {
            match op {
                Op::Write(writes, paced) => {
                    // A dirty client's changes are in the state stored
                    // whole, which is taken after they were made
                    if !dirty {
                        batch.extend(writes.into_vec());
                    }
                    permits += usize::from(paced);
                }
                Op::Flush(done) => flushed.push(done),
            }
            next = (dirty || batch.len() < MAX_BATCH)
                .then(|| ops.try_recv().ok())
                .flatten();
        }
        if dirty {
            if !flushed.is_empty() || Instant::now() >= next_reconcile {
                if reconcile(storage.as_ref(), &client_id, &health).await {
                    dirty = false;
                    reconcile_delay = RECONCILE_DELAY;
                } else {
                    next_reconcile = Instant::now() + reconcile_delay;
                    reconcile_delay = (reconcile_delay * 2).min(MAX_RECONCILE_DELAY);
                }
            }
        } else {
            compact(&mut batch);
            if !batch.is_empty() {
                if apply(storage.as_ref(), &client_id, &batch, &health).await == Applied::GaveUp {
                    health.failed.fetch_add(1, Ordering::Relaxed);
                    health.dirty.lock().unwrap().insert(client_id.clone());
                    dirty = true;
                    next_reconcile = Instant::now() + RECONCILE_DELAY;
                }
                batch.clear();
            }
        }
        pending.add_permits(permits);
        for done in flushed.drain(..) {
            let _ = done.send(());
        }
    }
    let _ = stopped.send(true);
    let mut entries = entries.lock().unwrap();
    if entries.get(&client_id).is_some_and(|entry| entry.id == id) {
        entries.remove(&client_id);
    }
}

/// Drops changes that a later one in the batch undoes, such as a message
/// stored in flight and then removed once acknowledged, or queued and then
/// sent, which under load is most of them.
fn compact(batch: &mut Vec<Write>) {
    let (mut removed, mut released, mut dequeued) =
        (HashSet::new(), HashSet::new(), HashSet::new());
    let (mut queue_cleared, mut session_deleted) = (false, false);
    let mut keep = vec![true; batch.len()];
    for (i, write) in batch.iter().enumerate().rev() {
        match write {
            Write::RemoveInflight(packet_id) => {
                removed.insert(*packet_id);
            }
            Write::StoreInflight(message) => {
                keep[i] = !session_deleted && !removed.contains(&message.packet_id);
            }
            Write::ReleaseInflight(packet_id) => {
                keep[i] = !session_deleted && !removed.contains(packet_id);
            }
            Write::RemoveIncomingQos2(packet_id) => {
                released.insert(*packet_id);
            }
            Write::StoreIncomingQos2(packet_id) => {
                keep[i] = !session_deleted && !released.contains(packet_id);
            }
            Write::Dequeue(sequence) => {
                dequeued.insert(*sequence);
                keep[i] = !session_deleted && !queue_cleared;
            }
            // The removal stays, as harmless as it is cheap
            Write::Enqueue(sequence, _) => {
                keep[i] = !session_deleted && !queue_cleared && !dequeued.contains(sequence);
            }
            Write::ClearQueue => queue_cleared = true,
            Write::DeleteSession => session_deleted = true,
            _ => {}
        }
    }
    let mut keep = keep.into_iter();
    batch.retain(|_| keep.next().unwrap_or(true));
}

/// How `apply` ended.
#[derive(Debug, PartialEq, Eq)]
enum Applied {
    Stored,
    GaveUp,
    /// Another broker holds the database, and this one is stopping
    Fenced,
}

/// Applies a batch, retrying with a growing delay, as storage on another
/// host fails now and then. Changes are applied again whole, which they
/// allow.
async fn apply<S: BatchStore>(
    storage: &S,
    client_id: &str,
    batch: &[Write],
    health: &Health,
) -> Applied {
    let mut retry = Retry::default();
    loop {
        let Err(e) = storage.apply(client_id, batch).await else {
            return Applied::Stored;
        };
        match retry.after(&e) {
            Some(delay) => {
                health.retried.fetch_add(1, Ordering::Relaxed);
                warn!(
                    "Failed to store changes for `{}`, retrying: {}",
                    client_id, e
                );
                tokio::time::sleep(delay).await;
            }
            None if matches!(e, StorageError::Fenced) => return Applied::Fenced,
            None => {
                error!(
                    "Gave up storing {} changes for `{}`, storing its whole state instead \
                     once storage takes it: {}",
                    batch.len(),
                    client_id,
                    e
                );
                return Applied::GaveUp;
            }
        }
    }
}

/// Stores the client's state whole, replacing what storage holds for it.
/// Returns whether it did, and the client is clean again.
async fn reconcile<S: BatchStore>(storage: &S, client_id: &str, health: &Health) -> bool {
    let Some(snapshots) = health.snapshots.get() else {
        return false;
    };
    let Some(writes) = snapshots.snapshot(client_id).await else {
        return false;
    };
    match storage.apply(client_id, &writes).await {
        Ok(()) => {
            health.dirty.lock().unwrap().remove(client_id);
            info!("Stored the whole state of `{}` again", client_id);
            true
        }
        Err(e) => {
            warn!(
                "Failed to store the whole state of `{}`, will try again: {}",
                client_id, e
            );
            false
        }
    }
}

/// Longest wait between attempts at a batch.
const MAX_DELAY: Duration = Duration::from_secs(4);

/// Whether a failed batch is worth another attempt. Transient failures,
/// such as a lost connection, are retried until storage recovers; others
/// give the batch up after `ATTEMPTS`.
#[derive(Default)]
struct Retry {
    failures: u32,
    permanent: u32,
}

impl Retry {
    /// How long to wait before the next attempt after `error`, or None to
    /// give the batch up.
    fn after(&mut self, error: &StorageError) -> Option<Duration> {
        match error {
            // Another broker holds the database; this one is stopping
            StorageError::Fenced => return None,
            StorageError::Transient(_) => {}
            _ => {
                self.permanent += 1;
                if self.permanent == ATTEMPTS {
                    return None;
                }
            }
        }
        self.failures = self.failures.saturating_add(1);
        Some(Duration::from_millis(250 << self.failures.min(4)).min(MAX_DELAY))
    }
}

#[cfg(test)]
mod tests {
    use mercurio_storage::memory::MemoryStore;

    use super::*;

    fn inflight(packet_id: u16) -> Write {
        Write::StoreInflight(mercurio_storage::InflightMessage {
            packet_id,
            topic: "t".into(),
            payload: None,
            qos: mercurio_core::qos::QoS::AtLeastOnce,
            dup: false,
            retain: false,
            expires_at: None,
            sequence: 0,
            released: false,
            properties: Default::default(),
            subscription_identifiers: Vec::new(),
        })
    }

    #[test]
    fn test_compact_drops_what_a_later_change_undoes() {
        let describe = |batch: &[Write]| -> Vec<String> {
            batch
                .iter()
                .map(|w| match w {
                    Write::StoreInflight(m) => format!("store {}", m.packet_id),
                    Write::ReleaseInflight(p) => format!("release {p}"),
                    Write::RemoveInflight(p) => format!("remove {p}"),
                    Write::StoreIncomingQos2(p) => format!("received {p}"),
                    Write::Enqueue(s, _) => format!("queue {s}"),
                    Write::Dequeue(s) => format!("dequeue {s}"),
                    Write::ClearQueue => "clear".into(),
                    Write::RemoveIncomingQos2(p) => format!("pubrel {p}"),
                    Write::DeleteSession => "delete".into(),
                    _ => "other".into(),
                })
                .collect()
        };
        let mut batch = vec![
            inflight(1),
            inflight(2),
            Write::ReleaseInflight(2),
            Write::RemoveInflight(1),
            inflight(1),
        ];
        compact(&mut batch);
        // The second store of 1 comes after its removal, so it stays
        assert_eq!(
            describe(&batch),
            ["store 2", "release 2", "remove 1", "store 1"]
        );

        let mut batch = vec![inflight(3), Write::DeleteSession, inflight(4)];
        compact(&mut batch);
        assert_eq!(describe(&batch), ["delete", "store 4"]);

        let mut batch = vec![
            Write::StoreIncomingQos2(1),
            Write::StoreIncomingQos2(2),
            Write::RemoveIncomingQos2(1),
            Write::StoreIncomingQos2(1),
        ];
        compact(&mut batch);
        assert_eq!(describe(&batch), ["received 2", "pubrel 1", "received 1"]);

        let queued = |sequence| {
            Write::Enqueue(
                sequence,
                mercurio_core::message::Message {
                    packet_id: None,
                    topic: "q".into(),
                    dup: false,
                    qos: mercurio_core::qos::QoS::AtLeastOnce,
                    retain: false,
                    payload: None,
                    properties: Default::default(),
                    expires_at: None,
                    subscription_identifiers: Vec::new(),
                    origin: None,
                },
            )
        };
        // A message queued and sent within the batch is only removed
        let mut batch = vec![queued(1), queued(2), Write::Dequeue(1), inflight(1)];
        compact(&mut batch);
        assert_eq!(describe(&batch), ["queue 2", "dequeue 1", "store 1"]);

        let mut batch = vec![queued(4), Write::Dequeue(3), Write::ClearQueue, queued(5)];
        compact(&mut batch);
        assert_eq!(describe(&batch), ["clear", "queue 5"]);
    }

    #[test]
    fn test_transient_failures_are_retried_until_they_pass() {
        let transient = || StorageError::Transient("pool wait timed out".into());
        let permanent = || StorageError::OperationFailed("constraint".into());
        let mut retry = Retry::default();
        let delays: Vec<_> = (0..20).map(|_| retry.after(&transient())).collect();
        assert!(delays
            .iter()
            .all(|delay| delay.is_some_and(|d| d <= MAX_DELAY)));
        assert_eq!(delays[19], Some(MAX_DELAY));

        // Permanent failures still give the batch up after a few attempts,
        // however many transient ones came before
        for _ in 1..ATTEMPTS {
            assert!(retry.after(&permanent()).is_some());
            assert!(retry.after(&transient()).is_some());
        }
        assert_eq!(retry.after(&permanent()), None);

        // Another broker holds the database: retrying can't help
        assert_eq!(Retry::default().after(&StorageError::Fenced), None);
    }

    #[tokio::test]
    async fn test_a_new_writer_waits_for_the_previous_one() {
        let storage = Arc::new(MemoryStore::new());
        let writers = Writers::new(Arc::clone(&storage));
        writers.write("c", Write::RemoveSubscription("t".into()));
        writers.close("c");
        writers.write("c", Write::DeleteSession);
        writers.settle("c").await;
        assert!(storage.load_session("c").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_idle_writers_stop_and_start_again() {
        let storage = Arc::new(MemoryStore::new());
        let writers = Writers::new(Arc::clone(&storage));
        writers.write("c", Write::RemoveSubscription("t".into()));
        writers.settle("c").await;
        tokio::time::sleep(LINGER + Duration::from_millis(300)).await;
        assert!(writers.entries.lock().unwrap().is_empty());

        // A later change starts a new writer
        writers.write("c", Write::DeleteSession);
        writers.settle("c").await;
        assert!(storage.load_session("c").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_writers_held_elsewhere_stay() {
        let storage = Arc::new(MemoryStore::new());
        let writers = Writers::new(Arc::clone(&storage));
        // As a disconnected session's queue journal holds it
        let held = writers.get("c");
        tokio::time::sleep(LINGER + Duration::from_millis(300)).await;
        assert_eq!(writers.entries.lock().unwrap().len(), 1);
        held.write(Write::DeleteWill);
        held.flush().await;
    }

    #[tokio::test]
    async fn test_writers_stop_once_closed() {
        let storage = Arc::new(MemoryStore::new());
        let writers = Writers::new(Arc::clone(&storage));
        writers.write("c", Write::DeleteWill);
        writers.close("c");
        writers.settle("c").await;
        tokio::task::yield_now().await;
        assert!(writers.entries.lock().unwrap().is_empty());
        assert!(storage.load_will("c").await.unwrap().is_none());
    }
}
