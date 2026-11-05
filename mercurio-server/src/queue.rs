use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use tokio::sync::Notify;
use tracing::warn;

use mercurio_core::{message::Message, qos::QoS};

/// QoS 0 messages beyond this many queued for one session are dropped.
// ponytail: fixed limit, make it configurable when deployments need tuning
pub(crate) const QOS0_QUEUE_LIMIT: usize = 1000;

/// Messages waiting to be sent to one session's client.
///
/// Each session has its own queue, so a slow or offline client never makes
/// others lose messages. QoS 0 messages are dropped once the queue is full;
/// QoS 1 and 2 messages are always kept, as the spec requires delivering
/// them.
#[derive(Default)]
pub(crate) struct MessageQueue {
    messages: Mutex<VecDeque<Message>>,
    notify: Notify,
}

impl MessageQueue {
    pub(crate) fn push(&self, message: Message) {
        let mut messages = self.messages.lock().unwrap();
        if message.qos == QoS::AtMostOnce && messages.len() >= QOS0_QUEUE_LIMIT {
            warn!(
                "Message queue full, dropping QoS 0 message on `{}`",
                message.topic
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
        }
    }

    #[tokio::test]
    async fn test_full_queue_drops_only_qos0() {
        let queue = MessageQueue::default();
        for _ in 0..QOS0_QUEUE_LIMIT {
            queue.push(message(QoS::AtMostOnce));
        }

        queue.push(message(QoS::AtMostOnce));
        queue.push(message(QoS::AtLeastOnce));

        let messages = queue.messages.lock().unwrap();
        assert_eq!(messages.len(), QOS0_QUEUE_LIMIT + 1);
        assert_eq!(messages.back().unwrap().qos, QoS::AtLeastOnce);
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
