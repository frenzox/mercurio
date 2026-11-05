use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use tokio::sync::Notify;
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
    messages: Mutex<VecDeque<Message>>,
    notify: Notify,
    limits: QueueLimits,
}

impl Default for MessageQueue {
    fn default() -> Self {
        Self::new(QueueLimits::default())
    }
}

impl MessageQueue {
    pub(crate) fn new(limits: QueueLimits) -> Self {
        Self {
            messages: Mutex::default(),
            notify: Notify::new(),
            limits,
        }
    }

    pub(crate) fn push(&self, message: Message) {
        let mut messages = self.messages.lock().unwrap();
        let limit = if message.qos == QoS::AtMostOnce {
            self.limits.max_qos0
        } else {
            self.limits.max_messages
        };
        if messages.len() >= limit {
            warn!(
                "Message queue full, dropping QoS {} message on `{}`",
                message.qos as u8, message.topic
            );
            return;
        }
        messages.push_back(message);
        drop(messages);
        self.notify.notify_one();
    }

    /// Waits for the next message. Cancel safe: a message is only removed
    /// from the queue when it is returned.
    pub(crate) async fn pop(&self) -> Message {
        loop {
            if let Some(message) = self.messages.lock().unwrap().pop_front() {
                return message;
            }
            self.notify.notified().await;
        }
    }
}

/// A session's subscription to one topic filter.
#[derive(Clone)]
pub(crate) struct Subscriber {
    pub queue: Arc<MessageQueue>,
    pub granted_qos: QoS,
}

impl Subscriber {
    /// Queues a message, downgraded to the QoS granted for the subscription
    /// [MQTT-3.8.4-8].
    pub(crate) fn deliver(&self, mut message: Message) {
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

        let messages = queue.messages.lock().unwrap();
        let qos: Vec<QoS> = messages.iter().map(|m| m.qos).collect();
        assert_eq!(qos, [QoS::AtMostOnce, QoS::AtMostOnce, QoS::AtLeastOnce]);
    }

    #[tokio::test]
    async fn test_pop_waits_for_push() {
        let queue = Arc::new(MessageQueue::default());

        let waiter = tokio::spawn({
            let queue = Arc::clone(&queue);
            async move { queue.pop().await }
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
