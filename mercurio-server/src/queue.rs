use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

use tokio::sync::Notify;
use tracing::warn;

use mercurio_core::{
    message::{unix_millis, Message},
    qos::QoS,
};
use mercurio_storage::Write;

use crate::writer::Writer;

/// How many messages one session may have waiting to be sent.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct QueueLimits {
    /// QoS 0 messages are dropped once this many messages are queued.
    pub max_qos0: usize,
    /// Any message is dropped once this many are queued, so a persistent
    /// session whose client never returns can't grow without bound.
    pub max_messages: usize,
    /// Any message is dropped once the queued payloads take this many
    /// bytes, when set. By default 100 MiB, as `max_messages` alone lets a
    /// session hold that many of the largest packets.
    pub max_bytes: Option<usize>,
    /// Send messages at the QoS granted to the subscription even when they
    /// were published with a lower one, as mosquitto's
    /// `upgrade_outgoing_qos`. Outside the MQTT specification.
    pub upgrade_qos: bool,
}

impl Default for QueueLimits {
    fn default() -> Self {
        Self {
            max_qos0: 1000,
            max_messages: 10_000,
            max_bytes: Some(100 * 1024 * 1024),
            upgrade_qos: false,
        }
    }
}

/// Messages waiting to be sent to one session's client.
///
/// Each session has its own queue, so a slow or offline client never makes
/// others lose messages. QoS 0 messages, which the sender accepted could be
/// lost, are dropped first; QoS 1 and 2 messages only once the queue is at
/// its hard limit.
pub(crate) struct MessageQueue {
    state: Mutex<QueueState>,
    notify: Notify,
    limits: QueueLimits,
    /// Whether a client is connected to take the messages
    connected: AtomicBool,
}

#[derive(Default)]
struct QueueState {
    /// Messages with their sequence in this queue
    messages: VecDeque<(u64, Message)>,
    /// Payload bytes of `messages`
    bytes: usize,
    next_sequence: u64,
    /// Messages dropped since the queue last had room, so a full queue is
    /// logged once rather than for every message
    dropped: u64,
    /// Where QoS 1 and 2 messages are recorded while the client is away
    journal: Option<Writer>,
}

impl Default for MessageQueue {
    fn default() -> Self {
        Self::new(QueueLimits::default())
    }
}

impl MessageQueue {
    pub(crate) fn new(limits: QueueLimits) -> Self {
        Self {
            state: Mutex::default(),
            notify: Notify::new(),
            limits,
            connected: AtomicBool::new(false),
        }
    }

    pub(crate) fn set_connected(&self, connected: bool) {
        self.connected.store(connected, Ordering::Relaxed);
    }

    pub(crate) fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// How many messages wait to be sent.
    pub(crate) fn len(&self) -> usize {
        self.state.lock().unwrap().messages.len()
    }

    /// Queues `message` unless the queue is full, in which case the message
    /// is lost. Returns whether it was queued.
    pub(crate) fn push(&self, message: Message) -> bool {
        self.insert(message, true)
    }

    /// Queues `message` unless the queue is full. Returns whether it did;
    /// unlike `push`, a refusal is left for the caller to retry.
    pub(crate) fn try_push(&self, message: Message) -> bool {
        self.insert(message, false)
    }

    fn insert(&self, message: Message, lost_if_full: bool) -> bool {
        let mut state = self.state.lock().unwrap();
        let limit = if message.qos == QoS::AtMostOnce {
            self.limits.max_qos0
        } else {
            self.limits.max_messages
        };
        let size = payload_size(&message);
        let full = |state: &QueueState| {
            state.messages.len() >= limit
                || self
                    .limits
                    .max_bytes
                    .is_some_and(|max| state.bytes + size > max)
        };
        if full(&state) {
            state.evict_expired();
        }
        if full(&state) {
            if !lost_if_full {
                return false;
            }
            if state.dropped == 0 {
                warn!(
                    "Message queue full, dropping messages, starting with a QoS {} one on `{}`",
                    message.qos as u8, message.topic
                );
            }
            state.dropped += 1;
            return false;
        }
        if state.dropped > 0 {
            warn!(
                "Message queue has room again after dropping {} messages",
                state.dropped
            );
            state.dropped = 0;
        }

        let sequence = state.next_sequence;
        state.next_sequence += 1;
        // QoS 0 messages may be lost, so only QoS 1 and 2 are kept
        if let Some(journal) = state
            .journal
            .as_ref()
            .filter(|_| message.qos != QoS::AtMostOnce)
        {
            journal.write(Write::Enqueue(sequence, message.clone()));
        }
        state.bytes += size;
        state.messages.push_back((sequence, message));
        drop(state);
        self.notify.notify_one();
        true
    }

    /// Starts recording QoS 1 and 2 messages in `journal`, beginning with
    /// those already queued. Used while the client is disconnected.
    pub(crate) fn start_journal(&self, journal: Writer) {
        let mut state = self.state.lock().unwrap();
        state.record(&journal);
        state.journal = Some(journal);
    }

    /// Stops recording and discards the recorded copy, once the client is
    /// back to receive the queue or the session is gone.
    pub(crate) fn stop_journal(&self) {
        if let Some(journal) = self.state.lock().unwrap().journal.take() {
            journal.write(Write::ClearQueue);
        }
    }

    /// Puts back messages recorded by a previous run, keeping their order.
    pub(crate) fn restore(&self, messages: Vec<(u64, Message)>) {
        let mut state = self.state.lock().unwrap();
        for (sequence, message) in messages {
            state.next_sequence = state.next_sequence.max(sequence + 1);
            state.bytes += payload_size(&message);
            state.messages.push_back((sequence, message));
        }
        drop(state);
        self.notify.notify_one();
    }

    /// Waits for the next message, or with `qos0_only` for the next QoS 0
    /// one, which may overtake QoS 1 and 2 messages: ordering is only
    /// guaranteed within a QoS level [MQTT-4.6.0-6]. Cancel safe: a message
    /// is only removed from the queue when it is returned.
    pub(crate) async fn pop(&self, qos0_only: bool) -> Message {
        loop {
            {
                let mut state = self.state.lock().unwrap();
                let position = if qos0_only {
                    state
                        .messages
                        .iter()
                        .position(|(_, m)| m.qos == QoS::AtMostOnce)
                } else if state.messages.is_empty() {
                    None
                } else {
                    Some(0)
                };
                if let Some((_, message)) = position.and_then(|p| state.messages.remove(p)) {
                    state.bytes -= payload_size(&message);
                    return message;
                }
            }
            self.notify.notified().await;
        }
    }
}

impl QueueState {
    /// Records the queued QoS 1 and 2 messages in `journal`.
    fn record(&self, journal: &Writer) {
        for (sequence, message) in &self.messages {
            if message.qos != QoS::AtMostOnce {
                journal.write(Write::Enqueue(*sequence, message.clone()));
            }
        }
    }

    /// Drops the messages past their expiry, which are never sent
    /// [MQTT-3.3.2-5]. The journal can't drop single messages, so it is
    /// rewritten with those left.
    fn evict_expired(&mut self) {
        let now = unix_millis();
        let before = self.messages.len();
        let mut freed = 0;
        self.messages.retain(|(_, message)| {
            let live = message.expires_at.is_none_or(|deadline| deadline > now);
            if !live {
                freed += payload_size(message);
            }
            live
        });
        if self.messages.len() == before {
            return;
        }
        self.bytes -= freed;
        if let Some(journal) = &self.journal {
            journal.write(Write::ClearQueue);
            self.record(journal);
        }
    }
}

fn payload_size(message: &Message) -> usize {
    message.payload.as_ref().map_or(0, |p| p.len())
}

/// A session's subscription to one topic filter.
#[derive(Clone)]
pub(crate) struct Subscriber {
    pub queue: Arc<MessageQueue>,
    /// The session the queue belongs to
    pub session_id: u64,
    pub granted_qos: QoS,
    pub no_local: bool,
    pub retain_as_published: bool,
    pub subscription_identifier: Option<u32>,
}

impl Subscriber {
    /// Queues one copy of a newly published message for `subscriptions`,
    /// all of them one session's and matching the message, `origin` being
    /// the session that published it. The copy has the highest QoS granted
    /// [MQTT-3.3.4-2] and the identifiers of every subscription
    /// [MQTT-3.3.4-4]. Returns whether the session's queue took it.
    pub(crate) fn deliver(
        subscriptions: &[Subscriber],
        mut message: Message,
        origin: Option<u64>,
    ) -> bool {
        // [MQTT-3.8.3-3]
        let matching: Vec<&Subscriber> = subscriptions
            .iter()
            .filter(|s| !(s.no_local && origin == Some(s.session_id)))
            .collect();
        let Some(first) = matching.first() else {
            return false;
        };
        // Forwarded messages carry RETAIN only when asked to keep it as
        // published [MQTT-3.3.1-12]
        if !matching.iter().any(|s| s.retain_as_published) {
            message.retain = false;
        }
        let granted_qos = matching
            .iter()
            .map(|s| s.granted_qos)
            .max_by_key(|qos| *qos as u8)
            .unwrap_or(QoS::AtMostOnce);
        let mut identifiers: Vec<u32> = matching
            .iter()
            .filter_map(|s| s.subscription_identifier)
            .collect();
        identifiers.sort_unstable();
        identifiers.dedup();
        first.queue_message(message, granted_qos, identifiers)
    }

    /// Queues a retained message sent because of a new subscription.
    /// Returns whether the queue took it; if not, it can be offered again
    /// once the queue drains.
    pub(crate) fn deliver_retained(&self, message: Message) -> bool {
        let identifiers = self.subscription_identifier.into_iter().collect();
        let message = self.prepare(message, self.granted_qos, identifiers);
        self.queue.try_push(message)
    }

    fn queue_message(&self, message: Message, granted_qos: QoS, identifiers: Vec<u32>) -> bool {
        let message = self.prepare(message, granted_qos, identifiers);
        self.queue.push(message)
    }

    /// The copy of `message` to queue, downgraded to the QoS granted
    /// [MQTT-3.8.4-8].
    fn prepare(&self, mut message: Message, granted_qos: QoS, identifiers: Vec<u32>) -> Message {
        message.subscription_identifiers = identifiers;
        if self.queue.limits.upgrade_qos {
            message.qos = granted_qos;
        } else if (message.qos as u8) > (granted_qos as u8) {
            message.qos = granted_qos;
            if granted_qos == QoS::AtMostOnce {
                message.packet_id = None;
            }
        }
        message
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bytes::Bytes;
    use tokio::time::timeout;

    use crate::writer::Op;

    use super::*;

    fn message(qos: QoS) -> Message {
        Message {
            packet_id: None,
            topic: Arc::from("t"),
            dup: false,
            qos,
            retain: false,
            payload: Some(Bytes::from("payload")),
            properties: Default::default(),
            expires_at: None,
            subscription_identifiers: Vec::new(),
            origin: None,
        }
    }

    #[tokio::test]
    async fn test_full_queue_drops_qos0_first() {
        let queue = MessageQueue::new(QueueLimits {
            max_qos0: 2,
            max_messages: 3,
            ..Default::default()
        });
        for _ in 0..2 {
            queue.push(message(QoS::AtMostOnce));
        }

        // Past the QoS 0 limit only QoS 1 and 2 are queued, up to the hard limit
        queue.push(message(QoS::AtMostOnce));
        queue.push(message(QoS::AtLeastOnce));
        queue.push(message(QoS::ExactlyOnce));

        let state = queue.state.lock().unwrap();
        let qos: Vec<QoS> = state.messages.iter().map(|(_, m)| m.qos).collect();
        assert_eq!(qos, [QoS::AtMostOnce, QoS::AtMostOnce, QoS::AtLeastOnce]);
        // The QoS 1 message fitting ended the first episode of drops
        assert_eq!(state.dropped, 1);
    }

    #[tokio::test]
    async fn test_byte_limit_counts_queued_payloads() {
        // Room for two 7 byte payloads
        let queue = MessageQueue::new(QueueLimits {
            max_bytes: Some(15),
            ..Default::default()
        });
        for _ in 0..3 {
            queue.push(message(QoS::AtLeastOnce));
        }
        assert_eq!(queue.state.lock().unwrap().messages.len(), 2);

        // Sending one frees its bytes
        queue.pop(false).await;
        queue.push(message(QoS::AtLeastOnce));
        assert_eq!(queue.state.lock().unwrap().messages.len(), 2);
        assert_eq!(queue.state.lock().unwrap().bytes, 14);
    }

    #[tokio::test]
    async fn test_upgrade_sends_at_the_granted_qos() {
        for (upgrade, expected) in [(false, QoS::AtMostOnce), (true, QoS::ExactlyOnce)] {
            let queue = Arc::new(MessageQueue::new(QueueLimits {
                upgrade_qos: upgrade,
                ..Default::default()
            }));
            let subscriber = Subscriber {
                queue: Arc::clone(&queue),
                session_id: 1,
                granted_qos: QoS::ExactlyOnce,
                no_local: false,
                retain_as_published: false,
                subscription_identifier: None,
            };
            Subscriber::deliver(&[subscriber], message(QoS::AtMostOnce), None);
            assert_eq!(queue.pop(false).await.qos, expected);
        }
    }

    #[tokio::test]
    async fn test_qos0_can_overtake_held_back_messages() {
        let queue = MessageQueue::default();
        queue.push(message(QoS::AtLeastOnce));
        queue.push(message(QoS::AtMostOnce));

        assert_eq!(queue.pop(true).await.qos, QoS::AtMostOnce);
        assert!(timeout(Duration::from_millis(20), queue.pop(true))
            .await
            .is_err());
        assert_eq!(queue.pop(false).await.qos, QoS::AtLeastOnce);
    }

    #[tokio::test]
    async fn test_journal_records_qos12_while_active() {
        let queue = MessageQueue::default();
        queue.push(message(QoS::AtLeastOnce));
        queue.push(message(QoS::AtMostOnce));

        let (journal, mut events) = Writer::channel();
        queue.start_journal(journal);
        queue.push(message(QoS::ExactlyOnce));
        queue.stop_journal();
        queue.push(message(QoS::AtLeastOnce));

        let mut recorded = Vec::new();
        while let Ok(event) = events.try_recv() {
            let Op::Write(write, _) = event else {
                continue;
            };
            recorded.push(match *write {
                Write::Enqueue(sequence, m) => format!("{sequence}:{}", m.qos as u8),
                Write::ClearQueue => "clear".to_string(),
                _ => "other".to_string(),
            });
        }
        // The queued QoS 1 message, then the new QoS 2 one; QoS 0 is skipped
        assert_eq!(recorded, ["0:1", "2:2", "clear"]);
    }

    fn expiring(qos: QoS, expires_at: u64) -> Message {
        Message {
            expires_at: Some(expires_at),
            ..message(qos)
        }
    }

    #[tokio::test]
    async fn test_full_queue_evicts_expired_messages() {
        let queue = MessageQueue::new(QueueLimits {
            max_messages: 3,
            ..Default::default()
        });
        let now = unix_millis();
        queue.push(expiring(QoS::AtLeastOnce, now - 1));
        queue.push(expiring(QoS::AtLeastOnce, now + 60_000));
        queue.push(expiring(QoS::AtLeastOnce, now - 1));

        assert!(queue.push(message(QoS::ExactlyOnce)));
        let state = queue.state.lock().unwrap();
        let sequences: Vec<u64> = state.messages.iter().map(|(s, _)| *s).collect();
        assert_eq!(sequences, [1, 3]);
        assert_eq!(state.bytes, 14);
        assert_eq!(state.dropped, 0);
    }

    #[tokio::test]
    async fn test_evicting_expired_messages_rewrites_the_journal() {
        let queue = MessageQueue::new(QueueLimits {
            max_messages: 2,
            ..Default::default()
        });
        let now = unix_millis();
        queue.push(expiring(QoS::AtLeastOnce, now - 1));
        queue.push(message(QoS::AtLeastOnce));

        let (journal, mut events) = Writer::channel();
        queue.start_journal(journal);
        queue.push(message(QoS::ExactlyOnce));

        let mut recorded = Vec::new();
        while let Ok(event) = events.try_recv() {
            let Op::Write(write, _) = event else {
                continue;
            };
            recorded.push(match *write {
                Write::Enqueue(sequence, _) => sequence.to_string(),
                Write::ClearQueue => "clear".to_string(),
                _ => "other".to_string(),
            });
        }
        // Both journaled, then the expired one dropped from the record
        assert_eq!(recorded, ["0", "1", "clear", "1", "2"]);
    }

    #[tokio::test]
    async fn test_restore_keeps_order_and_sequences() {
        let queue = MessageQueue::default();
        queue.restore(vec![
            (5, message(QoS::AtLeastOnce)),
            (9, message(QoS::ExactlyOnce)),
        ]);
        queue.push(message(QoS::AtMostOnce));

        let state = queue.state.lock().unwrap();
        let sequences: Vec<u64> = state.messages.iter().map(|(s, _)| *s).collect();
        assert_eq!(sequences, [5, 9, 10]);
    }

    #[tokio::test]
    async fn test_pop_waits_for_push() {
        let queue = Arc::new(MessageQueue::default());

        let waiter = tokio::spawn({
            let queue = Arc::clone(&queue);
            async move { queue.pop(false).await }
        });
        tokio::task::yield_now().await;
        queue.push(message(QoS::AtLeastOnce));

        let popped = timeout(Duration::from_secs(1), waiter)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(popped.qos, QoS::AtLeastOnce);
    }
}
