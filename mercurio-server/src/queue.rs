use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use tokio::sync::{mpsc::UnboundedSender, Notify};
use tracing::warn;

use mercurio_core::{message::Message, qos::QoS};

/// How many messages one session may have waiting to be sent.
#[derive(Debug, Clone, Copy)]
pub struct QueueLimits {
    /// QoS 0 messages are dropped once this many messages are queued.
    pub max_qos0: usize,
    /// Any message is dropped once this many are queued, so a persistent
    /// session whose client never returns can't grow without bound.
    pub max_messages: usize,
}

impl Default for QueueLimits {
    fn default() -> Self {
        Self {
            max_qos0: 1000,
            max_messages: 10_000,
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
}

#[derive(Default)]
struct QueueState {
    /// Messages with their sequence in this queue
    messages: VecDeque<(u64, Message)>,
    next_sequence: u64,
    /// Where QoS 1 and 2 messages are recorded while the client is away
    journal: Option<UnboundedSender<JournalEvent>>,
}

/// A change to a queue to apply to its stored copy.
#[derive(Debug)]
pub(crate) enum JournalEvent {
    Enqueued(u64, Box<Message>),
    /// The stored copy is no longer needed
    Clear,
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
        }
    }

    pub(crate) fn push(&self, message: Message) {
        let mut state = self.state.lock().unwrap();
        let limit = if message.qos == QoS::AtMostOnce {
            self.limits.max_qos0
        } else {
            self.limits.max_messages
        };
        if state.messages.len() >= limit {
            warn!(
                "Message queue full, dropping QoS {} message on `{}`",
                message.qos as u8, message.topic
            );
            return;
        }

        let sequence = state.next_sequence;
        state.next_sequence += 1;
        // QoS 0 messages may be lost, so only QoS 1 and 2 are kept
        if let Some(journal) = state
            .journal
            .as_ref()
            .filter(|_| message.qos != QoS::AtMostOnce)
        {
            let _ = journal.send(JournalEvent::Enqueued(sequence, Box::new(message.clone())));
        }
        state.messages.push_back((sequence, message));
        drop(state);
        self.notify.notify_one();
    }

    /// Starts recording QoS 1 and 2 messages in `journal`, beginning with
    /// those already queued. Used while the client is disconnected.
    pub(crate) fn start_journal(&self, journal: UnboundedSender<JournalEvent>) {
        let mut state = self.state.lock().unwrap();
        for (sequence, message) in &state.messages {
            if message.qos != QoS::AtMostOnce {
                let _ = journal.send(JournalEvent::Enqueued(*sequence, Box::new(message.clone())));
            }
        }
        state.journal = Some(journal);
    }

    /// Stops recording and discards the recorded copy, once the client is
    /// back to receive the queue or the session is gone.
    pub(crate) fn stop_journal(&self) {
        if let Some(journal) = self.state.lock().unwrap().journal.take() {
            let _ = journal.send(JournalEvent::Clear);
        }
    }

    /// Puts back messages recorded by a previous run, keeping their order.
    pub(crate) fn restore(&self, messages: Vec<(u64, Message)>) {
        let mut state = self.state.lock().unwrap();
        for (sequence, message) in messages {
            state.next_sequence = state.next_sequence.max(sequence + 1);
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
                    return message;
                }
            }
            self.notify.notified().await;
        }
    }
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
    /// Queues a newly published message, `origin` being the session that
    /// published it.
    pub(crate) fn deliver(&self, mut message: Message, origin: Option<u64>) {
        // [MQTT-3.8.3-3]
        if self.no_local && origin == Some(self.session_id) {
            return;
        }
        // Forwarded messages carry RETAIN only when asked to keep it as
        // published [MQTT-3.3.1-12]
        if !self.retain_as_published {
            message.retain = false;
        }
        self.queue_message(message);
    }

    /// Queues a retained message sent because of a new subscription.
    pub(crate) fn deliver_retained(&self, message: Message) {
        self.queue_message(message);
    }

    /// Queues a message, downgraded to the QoS granted for the subscription
    /// [MQTT-3.8.4-8].
    fn queue_message(&self, mut message: Message) {
        message.subscription_identifier = self.subscription_identifier;
        if (message.qos as u8) > (self.granted_qos as u8) {
            message.qos = self.granted_qos;
            if self.granted_qos == QoS::AtMostOnce {
                message.packet_id = None;
            }
        }
        self.queue.push(message);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bytes::Bytes;
    use tokio::time::timeout;

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
            subscription_identifier: None,
        }
    }

    #[tokio::test]
    async fn test_full_queue_drops_qos0_first() {
        let queue = MessageQueue::new(QueueLimits {
            max_qos0: 2,
            max_messages: 3,
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

        let (journal, mut events) = tokio::sync::mpsc::unbounded_channel();
        queue.start_journal(journal);
        queue.push(message(QoS::ExactlyOnce));
        queue.stop_journal();
        queue.push(message(QoS::AtLeastOnce));

        let mut recorded = Vec::new();
        while let Ok(event) = events.try_recv() {
            recorded.push(match event {
                JournalEvent::Enqueued(sequence, m) => format!("{sequence}:{}", m.qos as u8),
                JournalEvent::Clear => "clear".to_string(),
            });
        }
        // The queued QoS 1 message, then the new QoS 2 one; QoS 0 is skipped
        assert_eq!(recorded, ["0:1", "2:2", "clear"]);
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
