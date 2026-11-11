use super::retained::RETAINED_PAGE;
use super::*;
use crate::error::Error;
use mercurio_core::{
    message::unix_millis, properties::SubscriptionIdentifier, protocol::ProtocolVersion,
};
use mercurio_packets::{
    connect::{self, ConnectPayload, ConnectProperties},
    publish::PublishPacket,
    pubrec::PubRecPacket,
    pubrel::PubRelPacket,
    subscribe::{RetainHandling, SubscribePayload, SubscriptionOptions},
    unsubscribe::{UnsubscribePacket, UnsubscribePayload},
    Packet,
};
use mercurio_storage::memory::MemoryStore;
use tokio::time::{timeout, Duration};

fn no_plugins() -> Chain {
    Chain::new(Vec::new(), Duration::from_secs(5))
}

fn create_test_session() -> Session {
    let connect_packet = ConnectPacket {
        protocol_version: ProtocolVersion::V5,
        clean_start: false,
        keepalive: 60,
        properties: None,
        payload: ConnectPayload {
            client_id: "test-client".to_string(),
            will: None,
            user_name: None,
            password: None,
        },
    };
    Session::new(connect_packet)
}

fn default_subscription_options() -> SubscriptionOptions {
    SubscriptionOptions::default()
}

fn create_connect_packet_with_will() -> ConnectPacket {
    ConnectPacket {
        protocol_version: ProtocolVersion::V5,
        clean_start: true,
        keepalive: 60,
        properties: None,
        payload: ConnectPayload {
            client_id: "test-client".to_string(),
            will: Some(connect::Will {
                qos: QoS::AtLeastOnce,
                retain: true,
                properties: None,
                topic: "will/topic".to_string(),
                payload: Bytes::from("goodbye"),
            }),
            ..Default::default()
        },
    }
}

fn create_connect_packet_without_will() -> ConnectPacket {
    ConnectPacket {
        protocol_version: ProtocolVersion::V5,
        clean_start: true,
        keepalive: 60,
        properties: None,
        payload: ConnectPayload {
            client_id: "test-client".to_string(),
            ..Default::default()
        },
    }
}

#[tokio::test]
async fn test_unsubscribe_nonexistent_topic() {
    let mut session = create_test_session();
    let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));

    let packet = UnsubscribePacket {
        packet_id: 1,
        properties: None,
        payload: vec![UnsubscribePayload {
            topic_filter: "nonexistent/topic".to_string(),
        }],
    };

    let result = session
        .handle_unsubscribe(packet, &broker, &no_plugins())
        .await
        .unwrap();

    match result {
        Some(ControlPacket::UnsubAck(ack)) => {
            assert_eq!(ack.packet_id, 1);
            assert_eq!(ack.payload.len(), 1);
            assert_eq!(
                ack.payload[0].reason_code,
                ReasonCode::NoSubscriptionExisted
            );
        }
        _ => panic!("Expected UnsubAck packet"),
    }
}

#[tokio::test]
async fn test_unsubscribe_existing_topic() {
    let mut session = create_test_session();
    let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));

    // First subscribe to a topic
    let subscribe_packet = mercurio_packets::subscribe::SubscribePacket {
        packet_id: 1,
        properties: None,
        payload: vec![SubscribePayload {
            topic_filter: "test/topic".to_string(),
            subs_opt: default_subscription_options(),
        }],
    };
    session
        .handle_subscribe(subscribe_packet, &broker, &no_plugins())
        .await
        .unwrap();

    // Now unsubscribe
    let unsub_packet = UnsubscribePacket {
        packet_id: 2,
        properties: None,
        payload: vec![UnsubscribePayload {
            topic_filter: "test/topic".to_string(),
        }],
    };

    let result = session
        .handle_unsubscribe(unsub_packet, &broker, &no_plugins())
        .await
        .unwrap();

    match result {
        Some(ControlPacket::UnsubAck(ack)) => {
            assert_eq!(ack.packet_id, 2);
            assert_eq!(ack.payload.len(), 1);
            assert_eq!(ack.payload[0].reason_code, ReasonCode::Success);
        }
        _ => panic!("Expected UnsubAck packet"),
    }
}

/// Each SUBSCRIBE and UNSUBSCRIBE is stored as what it changed, not
/// as every subscription the session holds.
#[tokio::test]
async fn test_subscription_changes_are_recorded_one_by_one() {
    let mut session = create_test_session();
    let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));
    for filter in ["a", "b"] {
        session
            .handle_subscribe(
                subscribe_packet(filter, QoS::AtLeastOnce),
                &broker,
                &no_plugins(),
            )
            .await
            .unwrap();
    }
    let described = |changes: Vec<Write>| -> Vec<String> {
        changes
            .into_iter()
            .map(|change| match change {
                Write::AddSubscription(s) => format!("+{}", s.topic_filter),
                Write::RemoveSubscription(filter) => format!("-{filter}"),
                _ => "other".into(),
            })
            .collect()
    };
    assert_eq!(
        described(session.take_subscription_changes().await),
        ["+a", "+b"]
    );

    let unsubscribe = |filter: &str| UnsubscribePacket {
        packet_id: 2,
        properties: None,
        payload: vec![UnsubscribePayload {
            topic_filter: filter.to_string(),
        }],
    };
    for filter in ["a", "never"] {
        session
            .handle_unsubscribe(unsubscribe(filter), &broker, &no_plugins())
            .await
            .unwrap();
    }
    assert_eq!(described(session.take_subscription_changes().await), ["-a"]);
    assert!(session.take_subscription_changes().await.is_empty());
}

#[tokio::test]
async fn test_unsubscribe_multiple_topics() {
    let mut session = create_test_session();
    let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));

    // Subscribe to two topics
    let subscribe_packet = mercurio_packets::subscribe::SubscribePacket {
        packet_id: 1,
        properties: None,
        payload: vec![
            SubscribePayload {
                topic_filter: "topic/one".to_string(),
                subs_opt: default_subscription_options(),
            },
            SubscribePayload {
                topic_filter: "topic/two".to_string(),
                subs_opt: default_subscription_options(),
            },
        ],
    };
    session
        .handle_subscribe(subscribe_packet, &broker, &no_plugins())
        .await
        .unwrap();

    // Unsubscribe from one existing, one non-existing
    let unsub_packet = UnsubscribePacket {
        packet_id: 3,
        properties: None,
        payload: vec![
            UnsubscribePayload {
                topic_filter: "topic/one".to_string(),
            },
            UnsubscribePayload {
                topic_filter: "topic/nonexistent".to_string(),
            },
            UnsubscribePayload {
                topic_filter: "topic/two".to_string(),
            },
        ],
    };

    let result = session
        .handle_unsubscribe(unsub_packet, &broker, &no_plugins())
        .await
        .unwrap();

    match result {
        Some(ControlPacket::UnsubAck(ack)) => {
            assert_eq!(ack.packet_id, 3);
            assert_eq!(ack.payload.len(), 3);
            assert_eq!(ack.payload[0].reason_code, ReasonCode::Success);
            assert_eq!(
                ack.payload[1].reason_code,
                ReasonCode::NoSubscriptionExisted
            );
            assert_eq!(ack.payload[2].reason_code, ReasonCode::Success);
        }
        _ => panic!("Expected UnsubAck packet"),
    }
}

fn subscribe_packet(topic_filter: &str, qos: QoS) -> mercurio_packets::subscribe::SubscribePacket {
    mercurio_packets::subscribe::SubscribePacket {
        packet_id: 1,
        properties: None,
        payload: vec![SubscribePayload {
            topic_filter: topic_filter.to_string(),
            subs_opt: SubscriptionOptions {
                qos,
                ..Default::default()
            },
        }],
    }
}

fn message(topic: &str, qos: QoS, packet_id: Option<u16>, retain: bool) -> Message {
    Message {
        packet_id,
        topic: Arc::from(topic),
        dup: false,
        qos,
        retain,
        payload: Some(Bytes::from("payload")),
        properties: Default::default(),
        expires_at: None,
        subscription_identifiers: Vec::new(),
        origin: None,
        share_group: None,
    }
}

async fn next_publish(session: &mut Session) -> PublishPacket {
    let (message, _) = session.next_outgoing(true).await;
    match session.prepare_publish(message).await.map(|o| o.packet) {
        Some(ControlPacket::Publish(p)) => p,
        other => panic!("Expected Publish packet, got {:?}", other),
    }
}

#[tokio::test]
async fn test_outgoing_publish_uses_published_topic_name() {
    let mut session = create_test_session();
    let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));

    session
        .handle_subscribe(
            subscribe_packet("a/+", QoS::AtMostOnce),
            &broker,
            &no_plugins(),
        )
        .await
        .unwrap();
    broker
        .publish("a/b", message("a/b", QoS::AtMostOnce, None, false))
        .await
        .unwrap();

    assert_eq!(next_publish(&mut session).await.topic_name, "a/b");
}

#[tokio::test]
async fn test_subscription_grants_requested_qos_and_downgrades_delivery() {
    let mut session = create_test_session();
    let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));

    let ack = session
        .handle_subscribe(
            subscribe_packet("q/0", QoS::AtMostOnce),
            &broker,
            &no_plugins(),
        )
        .await
        .unwrap();
    assert!(matches!(
        ack,
        Some(ControlPacket::SubAck(ref a)) if a.payload[0].reason_code == ReasonCode::GrantedQoS0
    ));

    let ack = session
        .handle_subscribe(
            subscribe_packet("q/1", QoS::AtLeastOnce),
            &broker,
            &no_plugins(),
        )
        .await
        .unwrap();
    assert!(matches!(
        ack,
        Some(ControlPacket::SubAck(ref a)) if a.payload[0].reason_code == ReasonCode::GrantedQoS1
    ));

    broker
        .publish("q/0", message("q/0", QoS::ExactlyOnce, Some(7), false))
        .await
        .unwrap();
    let publish = next_publish(&mut session).await;
    assert_eq!(publish.qos_level, QoS::AtMostOnce);
    assert_eq!(publish.packet_id, None);

    broker
        .publish("q/1", message("q/1", QoS::ExactlyOnce, Some(8), false))
        .await
        .unwrap();
    assert_eq!(next_publish(&mut session).await.qos_level, QoS::AtLeastOnce);

    broker
        .publish("q/1", message("q/1", QoS::AtMostOnce, None, false))
        .await
        .unwrap();
    assert_eq!(next_publish(&mut session).await.qos_level, QoS::AtMostOnce);
}

#[tokio::test]
async fn test_outgoing_packet_ids_are_allocated_per_session() {
    let mut session = create_test_session();
    let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));

    session
        .handle_subscribe(
            subscribe_packet("p", QoS::ExactlyOnce),
            &broker,
            &no_plugins(),
        )
        .await
        .unwrap();

    // Two publishers reusing the same packet identifier
    for _ in 0..2 {
        broker
            .publish("p", message("p", QoS::AtLeastOnce, Some(42), false))
            .await
            .unwrap();
    }

    let first = next_publish(&mut session).await.packet_id.unwrap();
    let second = next_publish(&mut session).await.packet_id.unwrap();
    assert_ne!(first, second);
    assert_ne!(first, 0);
    assert_ne!(second, 0);
}

fn qos2_publish(topic: &str, packet_id: u16, dup: bool) -> ControlPacket {
    ControlPacket::Publish(PublishPacket {
        dup,
        qos_level: QoS::ExactlyOnce,
        retain: false,
        topic_name: topic.to_string(),
        packet_id: Some(packet_id),
        properties: None,
        payload: Some(Bytes::from("payload")),
    })
}

#[tokio::test]
async fn test_incoming_qos2_retransmission_is_delivered_once() {
    let mut publisher = create_test_session();
    let mut subscriber = create_test_session();
    let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));

    subscriber
        .handle_subscribe(
            subscribe_packet("t", QoS::ExactlyOnce),
            &broker,
            &no_plugins(),
        )
        .await
        .unwrap();

    for dup in [false, true] {
        let response = publisher
            .process_incoming(qos2_publish("t", 5, dup), &broker, &no_plugins())
            .await
            .unwrap();
        assert!(matches!(response, Some(ControlPacket::PubRec(ref r)) if r.packet_id == 5));
    }

    next_publish(&mut subscriber).await;
    assert!(
        timeout(Duration::from_millis(50), subscriber.next_outgoing(true))
            .await
            .is_err()
    );

    // After PUBREL the packet id is free again for a new message
    publisher
        .process_incoming(
            ControlPacket::PubRel(PubRelPacket {
                packet_id: 5,
                reason: ReasonCode::Success,
                properties: None,
            }),
            &broker,
            &no_plugins(),
        )
        .await
        .unwrap();
    publisher
        .process_incoming(qos2_publish("t", 5, false), &broker, &no_plugins())
        .await
        .unwrap();
    next_publish(&mut subscriber).await;
}

/// Retained messages, which fail to store until `working` is set.
#[derive(Default)]
struct FlakyRetained {
    inner: MemoryStore,
    working: AtomicBool,
}

#[async_trait::async_trait]
impl RetainedMessageStore for FlakyRetained {
    async fn store_retained(
        &self,
        topic: &str,
        message: Option<Message>,
    ) -> mercurio_storage::Result<bool> {
        if !self.working.load(Ordering::Relaxed) {
            return Err(mercurio_storage::StorageError::OperationFailed(
                "disk full".into(),
            ));
        }
        self.inner.store_retained(topic, message).await
    }

    async fn get_retained_page(
        &self,
        topic_filter: &str,
        after: Option<&str>,
        limit: usize,
    ) -> mercurio_storage::Result<Vec<Message>> {
        self.inner
            .get_retained_page(topic_filter, after, limit)
            .await
    }

    async fn clear_all_retained(&self) -> mercurio_storage::Result<()> {
        self.inner.clear_all_retained().await
    }

    async fn count_retained(&self) -> mercurio_storage::Result<usize> {
        self.inner.count_retained().await
    }
}

/// A QoS 2 message the broker failed to take isn't acknowledged, and
/// the copy the client sends again is forwarded.
#[tokio::test]
async fn test_incoming_qos2_that_failed_to_store_is_taken_when_resent() {
    let mut publisher = create_test_session();
    let mut subscriber = create_test_session();
    let storage = Arc::new(FlakyRetained::default());
    let broker = crate::broker::Broker::new(Arc::clone(&storage));
    subscriber
        .handle_subscribe(
            subscribe_packet("t", QoS::ExactlyOnce),
            &broker,
            &no_plugins(),
        )
        .await
        .unwrap();
    let retained = |dup| {
        let ControlPacket::Publish(mut publish) = qos2_publish("t", 5, dup) else {
            unreachable!()
        };
        publish.retain = true;
        ControlPacket::Publish(publish)
    };

    let failed = publisher
        .process_incoming(retained(false), &broker, &no_plugins())
        .await;
    assert!(matches!(
        failed,
        Err(Error::Reason(ReasonCode::ImplementationSpecificError))
    ));
    assert!(nothing_queued(&subscriber).await);

    storage.working.store(true, Ordering::Relaxed);
    let response = publisher
        .process_incoming(retained(true), &broker, &no_plugins())
        .await
        .unwrap();
    assert!(matches!(response, Some(ControlPacket::PubRec(ref r)) if r.packet_id == 5));
    timeout(Duration::from_secs(1), next_publish(&mut subscriber))
        .await
        .expect("the copy sent again is forwarded");
    assert_eq!(broker.retained("t").await.unwrap().len(), 1);
}

/// Packet ids of QoS 2 messages a restored session had taken still
/// mark copies sent again as already delivered.
#[tokio::test]
async fn test_restored_incoming_qos2_ids_are_not_forwarded_again() {
    let mut publisher = create_test_session();
    let mut subscriber = create_test_session();
    let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));
    subscriber
        .handle_subscribe(
            subscribe_packet("t", QoS::ExactlyOnce),
            &broker,
            &no_plugins(),
        )
        .await
        .unwrap();

    publisher.restore_incoming_qos2(vec![5]).await;
    let response = publisher
        .process_incoming(qos2_publish("t", 5, true), &broker, &no_plugins())
        .await
        .unwrap();
    assert!(matches!(response, Some(ControlPacket::PubRec(ref r)) if r.packet_id == 5));
    assert!(nothing_queued(&subscriber).await);
}

#[tokio::test]
async fn test_incoming_pubrel_leaves_outgoing_qos2_state_alone() {
    let mut session = create_test_session();
    let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));
    let packet_id = send_qos2_to(&mut session, &broker).await;

    let pubrel = session
        .process_incoming(
            pubrec(packet_id, ReasonCode::Success),
            &broker,
            &no_plugins(),
        )
        .await
        .unwrap();
    assert!(
        matches!(pubrel, Some(ControlPacket::PubRel(ref p)) if p.reason == ReasonCode::Success)
    );
    session
        .process_incoming(pubrel_packet(packet_id), &broker, &no_plugins())
        .await
        .unwrap();

    assert!(session
        .shared
        .state
        .lock()
        .await
        .inflight
        .pubrecs
        .contains_key(&packet_id));
}

/// Delivers one QoS 2 message to `session`, returning its packet id.
async fn send_qos2_to(session: &mut Session, broker: &crate::broker::Broker<MemoryStore>) -> u16 {
    session
        .handle_subscribe(
            subscribe_packet("q2", QoS::ExactlyOnce),
            broker,
            &no_plugins(),
        )
        .await
        .unwrap();
    broker
        .publish("q2", message("q2", QoS::ExactlyOnce, Some(1), false))
        .await
        .unwrap();
    next_publish(session).await.packet_id.unwrap()
}

fn pubrec(packet_id: u16, reason: ReasonCode) -> ControlPacket {
    ControlPacket::PubRec(PubRecPacket {
        packet_id,
        reason,
        properties: None,
    })
}

fn pubrel_packet(packet_id: u16) -> ControlPacket {
    ControlPacket::PubRel(PubRelPacket {
        packet_id,
        reason: ReasonCode::Success,
        properties: None,
    })
}

#[tokio::test]
async fn test_unknown_packet_ids_are_reported() {
    let mut session = create_test_session();
    let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));

    let pubcomp = session
        .process_incoming(pubrel_packet(77), &broker, &no_plugins())
        .await
        .unwrap();
    assert!(matches!(
        pubcomp,
        Some(ControlPacket::PubComp(ref p)) if p.reason == ReasonCode::PacketIdentifierNotFound
    ));

    let pubrel = session
        .process_incoming(pubrec(78, ReasonCode::Success), &broker, &no_plugins())
        .await
        .unwrap();
    assert!(matches!(
        pubrel,
        Some(ControlPacket::PubRel(ref p)) if p.reason == ReasonCode::PacketIdentifierNotFound
    ));
    assert!(session
        .shared
        .state
        .lock()
        .await
        .inflight
        .pubrecs
        .is_empty());
}

#[tokio::test]
async fn test_failed_pubrec_ends_the_exchange() {
    let mut session = create_test_session();
    let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));
    let packet_id = send_qos2_to(&mut session, &broker).await;

    let response = session
        .process_incoming(
            pubrec(packet_id, ReasonCode::UnspecifiedError),
            &broker,
            &no_plugins(),
        )
        .await
        .unwrap();

    assert!(response.is_none());
    let state = session.shared.state.lock().await;
    assert!(state.inflight.pubrecs.is_empty());
    assert!(state.inflight.unacknowledged_messages.is_empty());
}

#[tokio::test]
async fn test_subscriptions_record_granted_qos() {
    let mut session = create_test_session();
    let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));

    session
        .handle_subscribe(
            subscribe_packet("s", QoS::AtLeastOnce),
            &broker,
            &no_plugins(),
        )
        .await
        .unwrap();

    assert_eq!(
        session.get_subscriptions().await,
        vec![StoredSubscription::new("s", QoS::AtLeastOnce)]
    );
}

#[tokio::test]
async fn test_restored_subscription_keeps_granted_qos() {
    let mut session = create_test_session();
    let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));

    session
        .restore_subscriptions(vec![StoredSubscription::new("r", QoS::AtMostOnce)], &broker)
        .await
        .unwrap();
    broker
        .publish("r", message("r", QoS::ExactlyOnce, Some(3), false))
        .await
        .unwrap();

    assert_eq!(next_publish(&mut session).await.qos_level, QoS::AtMostOnce);
}

#[tokio::test]
async fn test_expired_messages_are_dropped_and_others_carry_remaining_time() {
    let mut session = create_test_session();

    let mut expired = message("t", QoS::AtMostOnce, None, false);
    expired.expires_at = Some(unix_millis() - 1);
    assert!(session.prepare_publish(expired).await.is_none());
    let counters = session.snapshot().await.counters;
    assert_eq!(counters.dropped.load(Ordering::Relaxed), 1);

    let mut fresh = message("t", QoS::AtMostOnce, None, false);
    fresh.expires_at = Some(unix_millis() + 5000);
    let Some(ControlPacket::Publish(publish)) =
        session.prepare_publish(fresh).await.map(|o| o.packet)
    else {
        panic!("Expected Publish");
    };
    let interval = publish.properties.unwrap().message_expiry_interval.unwrap();
    assert_eq!(interval.value, 5);
}

fn subscribe_with(
    topic_filter: &str,
    subs_opt: SubscriptionOptions,
) -> mercurio_packets::subscribe::SubscribePacket {
    mercurio_packets::subscribe::SubscribePacket {
        packet_id: 1,
        properties: None,
        payload: vec![SubscribePayload {
            topic_filter: topic_filter.to_string(),
            subs_opt,
        }],
    }
}

async fn nothing_queued(session: &Session) -> bool {
    timeout(Duration::from_millis(50), session.next_outgoing(true))
        .await
        .is_err()
}

#[tokio::test]
async fn test_no_local_skips_own_messages() {
    let mut session = create_test_session();
    let mut other = create_test_session();
    let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));

    let options = SubscriptionOptions {
        no_local: true,
        ..Default::default()
    };
    session
        .handle_subscribe(subscribe_with("n", options), &broker, &no_plugins())
        .await
        .unwrap();

    let publish = ControlPacket::Publish(PublishPacket {
        topic_name: "n".into(),
        payload: Some(Bytes::from("x")),
        ..Default::default()
    });
    session
        .process_incoming(publish, &broker, &no_plugins())
        .await
        .unwrap();
    assert!(nothing_queued(&session).await);

    let publish = ControlPacket::Publish(PublishPacket {
        topic_name: "n".into(),
        payload: Some(Bytes::from("x")),
        ..Default::default()
    });
    other
        .process_incoming(publish, &broker, &no_plugins())
        .await
        .unwrap();
    next_publish(&mut session).await;
}

#[tokio::test]
async fn test_retain_as_published_keeps_retain_flag() {
    let mut session = create_test_session();
    let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));

    let options = SubscriptionOptions {
        retain_as_pub: true,
        ..Default::default()
    };
    session
        .handle_subscribe(subscribe_with("r", options), &broker, &no_plugins())
        .await
        .unwrap();
    broker
        .publish("r", message("r", QoS::AtMostOnce, None, true))
        .await
        .unwrap();

    assert!(next_publish(&mut session).await.retain);
}

#[tokio::test]
async fn test_retain_handling_options() {
    let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));
    broker
        .publish("h", message("h", QoS::AtMostOnce, None, true))
        .await
        .unwrap();

    let if_new = SubscriptionOptions {
        retain_handling: RetainHandling::SendRetainedIfNonExisting,
        ..Default::default()
    };
    let mut session = create_test_session();
    session
        .handle_subscribe(subscribe_with("h", if_new), &broker, &no_plugins())
        .await
        .unwrap();
    session.feed_retained(&broker, &no_plugins()).await;
    assert!(next_publish(&mut session).await.retain);
    let if_new = SubscriptionOptions {
        retain_handling: RetainHandling::SendRetainedIfNonExisting,
        ..Default::default()
    };
    session
        .handle_subscribe(subscribe_with("h", if_new), &broker, &no_plugins())
        .await
        .unwrap();
    session.feed_retained(&broker, &no_plugins()).await;
    assert!(nothing_queued(&session).await);

    let never = SubscriptionOptions {
        retain_handling: RetainHandling::DoNotSendRetained,
        ..Default::default()
    };
    let mut session = create_test_session();
    session
        .handle_subscribe(subscribe_with("h", never), &broker, &no_plugins())
        .await
        .unwrap();
    session.feed_retained(&broker, &no_plugins()).await;
    assert!(nothing_queued(&session).await);
}

fn subscribe_with_identifier(
    topic_filter: &str,
    identifier: u32,
) -> mercurio_packets::subscribe::SubscribePacket {
    let mut packet = subscribe_packet(topic_filter, QoS::AtMostOnce);
    packet.properties = Some(mercurio_packets::subscribe::SubscribeProperties {
        subscription_id: SubscriptionIdentifier::new(identifier),
        user_property: None,
    });
    packet
}

#[tokio::test]
async fn test_overlapping_subscriptions_get_one_copy_with_every_identifier() {
    let mut session = create_test_session();
    let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));

    for (filter, identifier) in [("s/+", 7), ("s/#", 9)] {
        session
            .handle_subscribe(
                subscribe_with_identifier(filter, identifier),
                &broker,
                &no_plugins(),
            )
            .await
            .unwrap();
    }
    broker
        .publish("s/x", message("s/x", QoS::AtMostOnce, None, false))
        .await
        .unwrap();

    let properties = next_publish(&mut session).await.properties.unwrap();
    let mut identifiers: Vec<u32> = properties
        .subscription_identifiers
        .iter()
        .map(|id| id.value())
        .collect();
    identifiers.sort();
    assert_eq!(identifiers, [7, 9]);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), session.next_outgoing(true))
            .await
            .is_err(),
        "second copy delivered"
    );
}

#[tokio::test]
async fn test_overlapping_subscriptions_deliver_at_the_highest_qos() {
    let mut session = create_test_session();
    let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));

    for (filter, qos) in [("q/+", QoS::AtMostOnce), ("q/#", QoS::ExactlyOnce)] {
        session
            .handle_subscribe(subscribe_packet(filter, qos), &broker, &no_plugins())
            .await
            .unwrap();
    }
    broker
        .publish("q/x", message("q/x", QoS::ExactlyOnce, Some(1), false))
        .await
        .unwrap();

    assert_eq!(next_publish(&mut session).await.qos_level, QoS::ExactlyOnce);
}

#[tokio::test]
async fn test_invalid_subscription_identifiers_are_protocol_errors() {
    let mut session = create_test_session();
    let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));

    // An identifier of 0 can't be built; the decoder refuses it
    let from_client = ControlPacket::Publish(PublishPacket {
        topic_name: "t".into(),
        properties: Some(PublishProperties {
            subscription_identifiers: SubscriptionIdentifier::new(1).into_iter().collect(),
            ..Default::default()
        }),
        ..Default::default()
    });
    assert!(matches!(
        session
            .process_incoming(from_client, &broker, &no_plugins())
            .await,
        Err(Error::Reason(ReasonCode::ProtocolError))
    ));
}

fn aliased_publish(topic: &str, alias: u16) -> ControlPacket {
    ControlPacket::Publish(PublishPacket {
        topic_name: topic.into(),
        properties: Some(PublishProperties {
            topic_alias: Some(mercurio_core::properties::TopicAlias::new(alias)),
            ..Default::default()
        }),
        payload: Some(Bytes::from("x")),
        ..Default::default()
    })
}

#[tokio::test]
async fn test_topic_aliases_resolve_to_registered_topics() {
    let mut publisher = create_test_session();
    let mut subscriber = create_test_session();
    let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));
    publisher.shared.state.lock().await.topic_alias_maximum = 5;
    subscriber
        .handle_subscribe(
            subscribe_packet("a/b", QoS::AtMostOnce),
            &broker,
            &no_plugins(),
        )
        .await
        .unwrap();

    for topic in ["a/b", ""] {
        publisher
            .process_incoming(aliased_publish(topic, 1), &broker, &no_plugins())
            .await
            .unwrap();
        let publish = next_publish(&mut subscriber).await;
        assert_eq!(publish.topic_name, "a/b");
        // The alias is not forwarded
        assert!(publish.properties.unwrap().topic_alias.is_none());
    }

    for (topic, alias, reason) in [
        ("a/b", 6, ReasonCode::TopicAliasInvalid),
        ("a/b", 0, ReasonCode::TopicAliasInvalid),
        ("", 2, ReasonCode::ProtocolError),
    ] {
        let result = publisher
            .process_incoming(aliased_publish(topic, alias), &broker, &no_plugins())
            .await;
        assert!(
            matches!(result, Err(Error::Reason(r)) if r == reason),
            "alias {alias}"
        );
    }
}

#[tokio::test]
async fn test_messages_beyond_client_maximum_packet_size_are_dropped() {
    let mut connect = create_connect_packet_without_will();
    connect.properties = Some(mercurio_packets::connect::ConnectProperties {
        maximum_packet_size: Some(MaximumPacketSize::new(32)),
        ..Default::default()
    });
    let mut session = Session::new(connect);

    let small = message("t", QoS::AtLeastOnce, None, false);
    assert!(session.prepare_publish(small).await.is_some());

    let mut large = message("t", QoS::AtLeastOnce, None, false);
    large.payload = Some(Bytes::from(vec![0u8; 64]));
    assert!(session.prepare_publish(large).await.is_none());
    let counters = session.snapshot().await.counters;
    assert_eq!(counters.dropped.load(Ordering::Relaxed), 1);
    // Nothing is left awaiting an acknowledgement that will never come
    assert_eq!(
        session
            .shared
            .state
            .lock()
            .await
            .inflight
            .unacknowledged_messages
            .len(),
        1
    );
}

#[tokio::test]
async fn test_shared_subscription_splits_messages_without_retained() {
    let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));
    broker
        .publish("w", message("w", QoS::AtMostOnce, None, true))
        .await
        .unwrap();

    let mut members = [create_test_session(), create_test_session()];
    for member in &mut members {
        member
            .handle_subscribe(
                subscribe_packet("$share/g/w", QoS::AtMostOnce),
                &broker,
                &no_plugins(),
            )
            .await
            .unwrap();
        member.feed_retained(&broker, &no_plugins()).await;
        assert!(nothing_queued(member).await);
    }

    for _ in 0..4 {
        broker
            .publish("w", message("w", QoS::AtMostOnce, None, false))
            .await
            .unwrap();
    }
    for member in &mut members {
        for _ in 0..2 {
            assert_eq!(next_publish(member).await.topic_name, "w");
        }
        assert!(nothing_queued(member).await);
    }
}

#[tokio::test]
async fn test_no_local_on_shared_subscription_is_a_protocol_error() {
    let mut session = create_test_session();
    let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));

    let options = SubscriptionOptions {
        no_local: true,
        ..Default::default()
    };
    let result = session
        .handle_subscribe(
            subscribe_with("$share/g/w", options),
            &broker,
            &no_plugins(),
        )
        .await;
    assert!(matches!(
        result,
        Err(Error::Reason(ReasonCode::ProtocolError))
    ));
}

#[tokio::test]
async fn test_packet_identifier_zero_is_a_protocol_error() {
    let mut session = create_test_session();
    let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));

    let mut subscribe = subscribe_packet("t", QoS::AtMostOnce);
    subscribe.packet_id = 0;
    let packets = [
        qos2_publish("t", 0, false),
        ControlPacket::Subscribe(subscribe),
        ControlPacket::Unsubscribe(UnsubscribePacket {
            packet_id: 0,
            properties: None,
            payload: vec![UnsubscribePayload {
                topic_filter: "t".into(),
            }],
        }),
        pubrel_packet(0),
    ];

    for packet in packets {
        let result = session
            .process_incoming(packet, &broker, &no_plugins())
            .await;
        assert!(matches!(
            result,
            Err(Error::Reason(ReasonCode::ProtocolError))
        ));
    }
}

#[tokio::test]
async fn test_unsubscribe_reports_invalid_filters() {
    let mut session = create_test_session();
    let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));

    let packet = UnsubscribePacket {
        packet_id: 1,
        properties: None,
        payload: vec![UnsubscribePayload {
            topic_filter: "a/#/b".into(),
        }],
    };
    let Some(ControlPacket::UnsubAck(ack)) = session
        .handle_unsubscribe(packet, &broker, &no_plugins())
        .await
        .unwrap()
    else {
        panic!("Expected UNSUBACK");
    };
    assert_eq!(ack.payload[0].reason_code, ReasonCode::TopicFilterInvalid);
}

#[tokio::test]
async fn test_outgoing_topics_are_aliased_up_to_the_client_maximum() {
    let mut connect = create_connect_packet_without_will();
    connect.properties = Some(mercurio_packets::connect::ConnectProperties {
        topic_alias_maximum: Some(TopicAliasMaximum::new(1)),
        ..Default::default()
    });
    let mut session = Session::new(connect);

    // Off unless the operator enables it
    let outgoing = session
        .prepare_publish(message("a", QoS::AtMostOnce, None, false))
        .await
        .unwrap();
    let ControlPacket::Publish(publish) = outgoing.packet else {
        panic!("Expected Publish");
    };
    assert!(publish.properties.unwrap().topic_alias.is_none());

    session.shared.state.lock().await.send_topic_aliases = true;

    let mut sent = Vec::new();
    for topic in ["a", "a", "b"] {
        let outgoing = session
            .prepare_publish(message(topic, QoS::AtLeastOnce, None, false))
            .await
            .unwrap();
        let ControlPacket::Publish(publish) = outgoing.packet else {
            panic!("Expected Publish");
        };
        let alias = publish
            .properties
            .unwrap()
            .topic_alias
            .map(|alias| alias.value);
        sent.push((publish.topic_name, alias));
    }
    assert_eq!(
        sent,
        [
            ("a".to_string(), Some(1)),
            (String::new(), Some(1)),
            ("b".to_string(), None)
        ]
    );

    // What would be resent after a reconnect keeps the full topic
    let state = session.shared.state.lock().await;
    assert!(state
        .inflight
        .unacknowledged_messages
        .values()
        .all(|sent| !sent.publish.topic_name.is_empty()));
}

fn encoded_len(packet: &ControlPacket) -> usize {
    let ControlPacket::Publish(publish) = packet else {
        panic!("Expected Publish");
    };
    publish.encoded_size(ProtocolVersion::V5)
}

#[tokio::test]
async fn test_aliased_publish_stays_within_client_maximum_packet_size() {
    let full = message("topic", QoS::AtMostOnce, None, false);
    let unaliased = encoded_len(
        &Session::new(create_connect_packet_without_will())
            .prepare_publish(full.clone())
            .await
            .unwrap()
            .packet,
    );

    let mut connect = create_connect_packet_without_will();
    connect.properties = Some(mercurio_packets::connect::ConnectProperties {
        topic_alias_maximum: Some(TopicAliasMaximum::new(1)),
        maximum_packet_size: Some(MaximumPacketSize::new(unaliased as u32)),
        ..Default::default()
    });
    let mut session = Session::new(connect);
    session.shared.state.lock().await.send_topic_aliases = true;

    // Announcing an alias would take it over the limit, so it goes
    // without one, and the alias stays free
    let outgoing = session.prepare_publish(full.clone()).await.unwrap();
    assert!(encoded_len(&outgoing.packet) <= unaliased);
    assert!(session
        .shared
        .state
        .lock()
        .await
        .outgoing_aliases
        .is_empty());

    // A smaller message announces it; later ones fit using it alone
    let mut small = full.clone();
    small.payload = Some(Bytes::from("x"));
    session.prepare_publish(small).await.unwrap();
    let outgoing = session.prepare_publish(full).await.unwrap();
    assert!(encoded_len(&outgoing.packet) < unaliased);
}

fn qos1_publish(topic: &str, payload: &'static [u8], text: bool) -> ControlPacket {
    ControlPacket::Publish(PublishPacket {
        qos_level: QoS::AtLeastOnce,
        topic_name: topic.into(),
        packet_id: Some(1),
        properties: Some(PublishProperties {
            payload_format_indicator: text
                .then(|| mercurio_core::properties::PayloadFormatIndicator::new(1)),
            ..Default::default()
        }),
        payload: Some(Bytes::from_static(payload)),
        ..Default::default()
    })
}

async fn puback_reason(
    session: &mut Session,
    broker: &crate::broker::Broker<MemoryStore>,
    packet: ControlPacket,
) -> ReasonCode {
    match session
        .process_incoming(packet, broker, &no_plugins())
        .await
        .unwrap()
    {
        Some(ControlPacket::PubAck(puback)) => puback.reason,
        other => panic!("Expected PUBACK, got {:?}", other),
    }
}

#[tokio::test]
async fn test_publish_acknowledgement_reasons() {
    let mut publisher = create_test_session();
    let mut subscriber = create_test_session();
    let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));
    subscriber
        .handle_subscribe(
            subscribe_packet("watched", QoS::AtLeastOnce),
            &broker,
            &no_plugins(),
        )
        .await
        .unwrap();

    assert_eq!(
        puback_reason(
            &mut publisher,
            &broker,
            qos1_publish("watched", b"hi", true)
        )
        .await,
        ReasonCode::Success
    );
    assert_eq!(
        puback_reason(
            &mut publisher,
            &broker,
            qos1_publish("ignored", b"hi", false)
        )
        .await,
        ReasonCode::NoMatchingSubscribers
    );
    // Declared as text, but not valid UTF-8, so not forwarded
    assert_eq!(
        puback_reason(
            &mut publisher,
            &broker,
            qos1_publish("watched", b"\xff", true)
        )
        .await,
        ReasonCode::PayloadFormatInvalid
    );
    next_publish(&mut subscriber).await;
    assert!(nothing_queued(&subscriber).await);
}

#[tokio::test]
async fn test_retained_messages_are_queued_as_the_queue_drains() {
    let limits = QueueLimits {
        max_qos0: 2,
        ..Default::default()
    };
    let mut session = Session::with_queue_limits(
        create_connect_packet_without_will(),
        limits,
        Default::default(),
    );
    let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));
    for i in 0..5 {
        let topic = format!("r/{i}");
        broker
            .publish(&topic, message(&topic, QoS::AtLeastOnce, Some(1), true))
            .await
            .unwrap();
    }

    session
        .handle_subscribe(subscribe_with_identifier("r/+", 7), &broker, &no_plugins())
        .await
        .unwrap();
    assert!(session.owes_retained());
    assert!(nothing_queued(&session).await);

    let mut topics = Vec::new();
    while session.owes_retained() {
        session.feed_retained(&broker, &no_plugins()).await;
        while session.shared.queue.len() > 0 {
            let publish = next_publish(&mut session).await;
            // Granted QoS 0, so downgraded, and with the identifier
            assert_eq!(publish.qos_level, QoS::AtMostOnce);
            assert!(publish.retain);
            let identifiers = &publish.properties.unwrap().subscription_identifiers;
            assert_eq!(identifiers[0].value(), 7);
            topics.push(publish.topic_name);
        }
    }
    topics.sort();
    assert_eq!(topics, ["r/0", "r/1", "r/2", "r/3", "r/4"]);
}

#[tokio::test]
async fn test_retained_delivery_sets_retain_flag_only_for_retained() {
    let mut session = create_test_session();
    let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));

    broker
        .publish("r/1", message("r/1", QoS::AtMostOnce, None, true))
        .await
        .unwrap();
    session
        .handle_subscribe(
            subscribe_packet("r/1", QoS::AtMostOnce),
            &broker,
            &no_plugins(),
        )
        .await
        .unwrap();
    session.feed_retained(&broker, &no_plugins()).await;
    assert!(next_publish(&mut session).await.retain);

    broker
        .publish("r/1", message("r/1", QoS::AtMostOnce, None, true))
        .await
        .unwrap();
    assert!(!next_publish(&mut session).await.retain);
}

/// Retained messages, recording the largest page looked up.
#[derive(Default)]
struct PagedRetained {
    inner: MemoryStore,
    largest: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl RetainedMessageStore for PagedRetained {
    async fn store_retained(
        &self,
        topic: &str,
        message: Option<Message>,
    ) -> mercurio_storage::Result<bool> {
        self.inner.store_retained(topic, message).await
    }

    async fn get_retained_page(
        &self,
        topic_filter: &str,
        after: Option<&str>,
        limit: usize,
    ) -> mercurio_storage::Result<Vec<Message>> {
        let page = self
            .inner
            .get_retained_page(topic_filter, after, limit)
            .await?;
        self.largest.fetch_max(page.len(), Ordering::Relaxed);
        Ok(page)
    }

    async fn clear_all_retained(&self) -> mercurio_storage::Result<()> {
        self.inner.clear_all_retained().await
    }

    async fn count_retained(&self) -> mercurio_storage::Result<usize> {
        self.inner.count_retained().await
    }
}

/// A subscription matching many retained messages gets them all, in
/// topic order, looked up a page at a time.
#[tokio::test]
async fn test_retained_messages_are_looked_up_a_page_at_a_time() {
    let mut session = create_test_session();
    let storage = Arc::new(PagedRetained::default());
    let broker = crate::broker::Broker::new(Arc::clone(&storage));
    let count = RETAINED_PAGE * 2 + 50;
    for i in 0..count {
        let topic = format!("r/{i:04}");
        broker
            .publish(&topic, message(&topic, QoS::AtMostOnce, None, true))
            .await
            .unwrap();
    }
    session
        .handle_subscribe(
            subscribe_packet("r/#", QoS::AtMostOnce),
            &broker,
            &no_plugins(),
        )
        .await
        .unwrap();

    let mut topics = Vec::new();
    while session.owes_retained() {
        session.feed_retained(&broker, &no_plugins()).await;
        while session.shared.queue.len() > 0 {
            topics.push(next_publish(&mut session).await.topic_name);
        }
    }
    let expected: Vec<String> = (0..count).map(|i| format!("r/{i:04}")).collect();
    assert_eq!(topics, expected);
    assert_eq!(storage.largest.load(Ordering::Relaxed), RETAINED_PAGE);
}

#[test]
fn test_will_message_from_connect_packet() {
    let packet = create_connect_packet_with_will();
    let will = WillMessage::from_connect_packet(&packet);

    assert!(will.is_some());
    let will = will.unwrap();
    assert_eq!(will.topic, "will/topic");
    assert_eq!(will.payload.as_ref(), b"goodbye");
    assert_eq!(will.qos, QoS::AtLeastOnce);
    assert!(will.retain);
}

#[test]
fn test_will_message_keeps_will_properties() {
    let mut packet = create_connect_packet_with_will();
    packet.payload.will.as_mut().unwrap().properties =
        Some(mercurio_packets::connect::WillProperties {
            content_type: Some(mercurio_core::properties::ContentType::new("text".into())),
            ..Default::default()
        });

    let will = WillMessage::from_connect_packet(&packet).unwrap();
    assert_eq!(will.properties.content_type.unwrap().value, "text");
}

#[test]
fn test_will_message_from_connect_packet_no_will() {
    let packet = create_connect_packet_without_will();
    let will = WillMessage::from_connect_packet(&packet);

    assert!(will.is_none());
}

#[tokio::test]
async fn test_session_stores_will() {
    let packet = create_connect_packet_with_will();
    let session = Session::new(packet);

    let will = session.take_will().await;
    assert!(will.is_some());
    let will = will.unwrap();
    assert_eq!(will.topic, "will/topic");
}

/// A task that holds `will` until aborted, as a delayed will does.
fn holding(will: &Arc<WillMessage>) -> tokio::task::JoinHandle<()> {
    let will = Arc::clone(will);
    tokio::spawn(async move {
        std::future::pending::<()>().await;
        drop(will);
    })
}

#[tokio::test]
async fn test_a_newer_generation_frees_the_pending_will() {
    let session = Session::new(create_connect_packet_with_will());
    let will = Arc::new(session.take_will().await.unwrap());

    let pending = holding(&will);
    session
        .set_pending_will(0, pending.abort_handle(), (*will).clone())
        .await;
    assert!(session.snapshot().await.will_pending);
    session.set_generation(1).await;
    assert!(pending.await.unwrap_err().is_cancelled());
    assert_eq!(Arc::strong_count(&will), 1);
    assert!(!session.snapshot().await.will_pending);

    // One left by a connection already superseded is freed at once
    let stale = holding(&will);
    session
        .set_pending_will(0, stale.abort_handle(), (*will).clone())
        .await;
    assert!(stale.await.unwrap_err().is_cancelled());
    assert_eq!(Arc::strong_count(&will), 1);

    // Once claimed, a takeover no longer cuts the will off
    let claimed = holding(&will);
    session
        .set_pending_will(1, claimed.abort_handle(), (*will).clone())
        .await;
    assert!(session.claim_pending_will(1).await);
    session.set_generation(2).await;
    tokio::task::yield_now().await;
    assert!(!claimed.is_finished());
    claimed.abort();
}

#[tokio::test]
async fn test_session_no_will_when_not_set() {
    let packet = create_connect_packet_without_will();
    let session = Session::new(packet);

    let will = session.take_will().await;
    assert!(will.is_none());
}

#[tokio::test]
async fn test_clear_will() {
    let packet = create_connect_packet_with_will();
    let session = Session::new(packet);

    // Will should exist initially
    {
        let state = session.shared.state.lock().await;
        assert!(state.will_message.is_some());
    }

    // Clear the will
    session.clear_will().await;

    // Will should be None after clearing
    let will = session.take_will().await;
    assert!(will.is_none());
}

#[tokio::test]
async fn test_take_will_removes_will() {
    let packet = create_connect_packet_with_will();
    let session = Session::new(packet);

    // First take should return the will
    let will1 = session.take_will().await;
    assert!(will1.is_some());

    // Second take should return None
    let will2 = session.take_will().await;
    assert!(will2.is_none());
}

#[tokio::test]
async fn test_set_connect_packet_updates_will() {
    let packet_no_will = create_connect_packet_without_will();
    let mut session = Session::new(packet_no_will);

    // No will initially
    assert!(session.take_will().await.is_none());

    // Update with packet that has will
    let packet_with_will = create_connect_packet_with_will();
    session.set_connect_packet(packet_with_will).await;

    // Now will should exist
    let will = session.take_will().await;
    assert!(will.is_some());
    assert_eq!(will.unwrap().topic, "will/topic");
}

/// Owns bytes and records when they are freed.
struct Watched(Arc<AtomicBool>);

impl AsRef<[u8]> for Watched {
    fn as_ref(&self) -> &[u8] {
        b"secret"
    }
}

impl Drop for Watched {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn test_sessions_let_go_of_credentials_and_connect_properties() {
    let password_freed = Arc::new(AtomicBool::new(false));
    let property_freed = Arc::new(AtomicBool::new(false));
    let mut connect = create_connect_packet_without_will();
    connect.payload.user_name = Some("alice".to_string());
    connect.payload.password = Some(Bytes::from_owner(Watched(Arc::clone(&password_freed))));
    connect.properties = Some(ConnectProperties {
        authentication_data: Some(AuthenticationData::new(Bytes::from_owner(Watched(
            Arc::clone(&property_freed),
        )))),
        ..Default::default()
    });

    let mut session = Session::new(connect.clone());
    let resumed = connect.clone();
    drop(connect);
    assert!(!password_freed.load(Ordering::SeqCst));
    session.set_connect_packet(resumed).await;

    assert!(password_freed.load(Ordering::SeqCst));
    assert!(property_freed.load(Ordering::SeqCst));
    drop(session);
}
