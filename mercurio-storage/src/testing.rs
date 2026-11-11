//! What every store does, as a suite a store's tests run against it:
//! `conformance` for the client state and retained messages, `bans` for a
//! `BanStore`. Enabled with the `test-util` feature.
//!
//! Each starts from an empty store and panics at the first difference.

use std::sync::Arc;

use bytes::Bytes;
use mercurio_core::{
    message::{Message, MessageOrigin, MessageProperties},
    properties::{ContentType, ResponseTopic, UserProperty},
    qos::QoS,
};

use crate::{
    BanStore, InflightMessage, LoadedClient, MqttStore, SessionOwner, SessionState, StoredBan,
    StoredClient, StoredSubscription, StoredWillMessage, Write,
};

/// Runs every check of the suite against `store`, which starts empty.
pub async fn conformance(store: &impl MqttStore) {
    sessions(store).await;
    subscriptions(store).await;
    wills(store).await;
    inflight(store).await;
    incoming_qos2(store).await;
    if store.is_durable() {
        queue(store).await;
    }
    batches(store).await;
    delete_cascades(store).await;
    bulk_load(store).await;
    retained(store).await;
    retained_pages(store).await;
}

fn properties() -> MessageProperties {
    MessageProperties {
        response_topic: Some(ResponseTopic::new("reply".into())),
        content_type: Some(ContentType::new("text".into())),
        user_property: Some(vec![UserProperty::new("k".into(), "v".into())]),
        ..Default::default()
    }
}

/// A message as the broker queues or retains it.
pub fn message(topic: &str, payload: &str) -> Message {
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

/// A QoS 2 message in flight, with every field set.
pub fn inflight_message(packet_id: u16) -> InflightMessage {
    InflightMessage {
        packet_id,
        topic: "t".into(),
        payload: Some(Bytes::from_static(b"p")),
        qos: QoS::ExactlyOnce,
        dup: true,
        retain: true,
        expires_at: Some(u64::MAX >> 1),
        sequence: u64::MAX,
        released: false,
        properties: properties(),
        subscription_identifiers: vec![1, 268_435_455],
    }
}

/// A session of `client_id` with every field set.
pub fn session(client_id: &str) -> SessionState {
    SessionState {
        client_id: client_id.into(),
        subscriptions: vec![
            StoredSubscription {
                no_local: true,
                retain_as_published: true,
                subscription_identifier: Some(268_435_455),
                // Commas, colons and wildcards are all valid in topic filters
                ..StoredSubscription::new("a/+:x,y/#", QoS::ExactlyOnce)
            },
            StoredSubscription::new("$share/g/b", QoS::AtMostOnce),
        ],
        expiry_interval: Some(u32::MAX),
        expires_at: Some(1_700_000_000_000),
        owner: SessionOwner {
            username: Some("alice".into()),
            mount_point: Some("tenant/".into()),
        },
    }
}

fn sorted(state: Option<SessionState>) -> SessionState {
    let mut state = state.expect("the session is stored");
    state
        .subscriptions
        .sort_by(|a, b| a.topic_filter.cmp(&b.topic_filter));
    state
}

async fn apply(store: &impl MqttStore, client_id: &str, writes: &[Write]) {
    store
        .apply(client_id, writes)
        .await
        .unwrap_or_else(|e| panic!("applying {writes:?}: {e}"));
}

async fn sessions(store: &impl MqttStore) {
    assert!(store.list_sessions().await.unwrap().is_empty());
    assert!(store.load_session("c").await.unwrap().is_none());

    let stored = session("c");
    apply(store, "c", &[Write::SaveSession(stored.clone())]).await;
    assert_eq!(
        sorted(store.load_session("c").await.unwrap()),
        sorted(Some(stored.clone()))
    );

    // Anonymous, without expiry
    let plain = SessionState {
        client_id: "d".into(),
        subscriptions: Vec::new(),
        expiry_interval: None,
        expires_at: None,
        owner: SessionOwner::default(),
    };
    apply(store, "d", &[Write::SaveSession(plain.clone())]).await;
    assert_eq!(store.load_session("d").await.unwrap(), Some(plain));

    apply(
        store,
        "c",
        &[Write::SetExpiry {
            interval: 60,
            expires_at: 5,
        }],
    )
    .await;
    let loaded = store.load_session("c").await.unwrap().unwrap();
    assert_eq!(
        (loaded.expiry_interval, loaded.expires_at),
        (Some(60), Some(5))
    );
    // Recording an expiry stores no session
    apply(
        store,
        "nobody",
        &[Write::SetExpiry {
            interval: 60,
            expires_at: 5,
        }],
    )
    .await;
    assert!(store.load_session("nobody").await.unwrap().is_none());

    let mut listed = store.list_sessions().await.unwrap();
    listed.sort();
    assert_eq!(listed, ["c", "d"]);

    store.delete_session("c").await.unwrap();
    store.delete_session("d").await.unwrap();
    // Deleting what isn't stored is no error
    store.delete_session("d").await.unwrap();
    assert!(store.list_sessions().await.unwrap().is_empty());
}

/// A session saved whole replaces its subscriptions, single ones are added
/// and removed in order, applying a batch twice is harmless, and they
/// leave with the session.
async fn subscriptions(store: &impl MqttStore) {
    let detailed = session("c").subscriptions[0].clone();
    let base = SessionState {
        subscriptions: vec![
            StoredSubscription::new("old", QoS::AtMostOnce),
            StoredSubscription::new("b", QoS::AtMostOnce),
        ],
        ..session("c")
    };
    // Without a stored session there is nothing to add to
    apply(store, "c", &[Write::AddSubscription(detailed.clone())]).await;
    assert!(store.load_session("c").await.unwrap().is_none());

    let batch = [
        Write::SaveSession(base.clone()),
        Write::AddSubscription(StoredSubscription::new("b", QoS::AtLeastOnce)),
        Write::AddSubscription(detailed.clone()),
        Write::RemoveSubscription("old".into()),
        Write::AddSubscription(StoredSubscription::new("gone", QoS::AtMostOnce)),
        Write::RemoveSubscription("gone".into()),
        // Removing what isn't subscribed is no error
        Write::RemoveSubscription("never".into()),
    ];
    for _ in 0..2 {
        apply(store, "c", &batch).await;
        assert_eq!(
            sorted(store.load_session("c").await.unwrap()).subscriptions,
            [
                detailed.clone(),
                StoredSubscription::new("b", QoS::AtLeastOnce)
            ]
        );
    }

    // Saving the session whole replaces the subscriptions
    apply(store, "c", &[Write::SaveSession(base.clone())]).await;
    assert_eq!(
        sorted(store.load_session("c").await.unwrap()).subscriptions,
        [
            StoredSubscription::new("b", QoS::AtMostOnce),
            StoredSubscription::new("old", QoS::AtMostOnce)
        ]
    );

    let other = SessionState {
        client_id: "other".into(),
        ..base.clone()
    };
    apply(store, "other", &[Write::SaveSession(other)]).await;
    apply(store, "c", &[Write::DeleteSession]).await;
    assert!(store.load_session("c").await.unwrap().is_none());
    // A session stored again doesn't find the subscriptions of the one
    // deleted
    apply(
        store,
        "c",
        &[Write::SaveSession(SessionState {
            subscriptions: Vec::new(),
            ..base
        })],
    )
    .await;
    assert!(store
        .load_session("c")
        .await
        .unwrap()
        .unwrap()
        .subscriptions
        .is_empty());
    assert_eq!(
        store
            .load_session("other")
            .await
            .unwrap()
            .unwrap()
            .subscriptions
            .len(),
        2
    );
    apply(store, "c", &[Write::DeleteSession]).await;
    apply(store, "other", &[Write::DeleteSession]).await;
}

async fn wills(store: &impl MqttStore) {
    let will = StoredWillMessage {
        topic: "client/status".into(),
        payload: Bytes::from_static(b"offline"),
        qos: QoS::AtLeastOnce,
        retain: true,
        delay_secs: u32::MAX,
        properties: properties(),
    };
    assert!(store.load_will("c").await.unwrap().is_none());
    apply(store, "c", &[Write::StoreWill(will.clone())]).await;
    assert_eq!(store.load_will("c").await.unwrap(), Some(will.clone()));

    let replaced = StoredWillMessage {
        payload: Bytes::new(),
        qos: QoS::AtMostOnce,
        retain: false,
        delay_secs: 0,
        properties: Default::default(),
        ..will
    };
    apply(store, "c", &[Write::StoreWill(replaced.clone())]).await;
    assert_eq!(store.load_will("c").await.unwrap(), Some(replaced));
    apply(store, "c", &[Write::DeleteWill, Write::DeleteWill]).await;
    assert!(store.load_will("c").await.unwrap().is_none());
}

async fn inflight(store: &impl MqttStore) {
    let sorted = |mut messages: Vec<InflightMessage>| {
        messages.sort_by_key(|m| m.packet_id);
        messages
    };
    let full = inflight_message(u16::MAX);
    let plain = InflightMessage {
        packet_id: 1,
        payload: None,
        qos: QoS::AtLeastOnce,
        dup: false,
        retain: false,
        expires_at: None,
        sequence: 0,
        properties: Default::default(),
        subscription_identifiers: Vec::new(),
        ..inflight_message(1)
    };
    apply(
        store,
        "c",
        &[
            Write::StoreInflight(full.clone()),
            Write::StoreInflight(plain.clone()),
        ],
    )
    .await;
    assert_eq!(
        sorted(store.load_inflight("c").await.unwrap()),
        [plain.clone(), full.clone()]
    );

    // Released, only the record stays
    apply(store, "c", &[Write::ReleaseInflight(u16::MAX)]).await;
    let released = InflightMessage {
        released: true,
        payload: None,
        ..full.clone()
    };
    assert_eq!(
        sorted(store.load_inflight("c").await.unwrap()),
        [plain, released]
    );

    // Releasing or removing what isn't stored is no error
    apply(
        store,
        "c",
        &[
            Write::RemoveInflight(1),
            Write::RemoveInflight(1),
            Write::ReleaseInflight(7),
            Write::RemoveInflight(u16::MAX),
        ],
    )
    .await;
    assert!(store.load_inflight("c").await.unwrap().is_empty());
}

/// Batches apply in order, applying one twice is harmless, and deleting
/// the session removes them.
async fn incoming_qos2(store: &impl MqttStore) {
    let sorted = |mut ids: Vec<u16>| {
        ids.sort_unstable();
        ids
    };
    let batch = [
        Write::StoreIncomingQos2(1),
        Write::StoreIncomingQos2(u16::MAX),
        Write::StoreIncomingQos2(7),
        Write::RemoveIncomingQos2(7),
        Write::RemoveIncomingQos2(1),
        Write::StoreIncomingQos2(1),
    ];
    for _ in 0..2 {
        apply(store, "c", &batch).await;
        assert_eq!(
            sorted(store.load_incoming_qos2("c").await.unwrap()),
            [1, u16::MAX]
        );
    }
    apply(store, "other", &[Write::StoreIncomingQos2(1)]).await;
    // Removing what isn't stored is no error
    apply(store, "c", &[Write::RemoveIncomingQos2(2)]).await;

    apply(store, "c", &[Write::DeleteSession]).await;
    assert!(store.load_incoming_qos2("c").await.unwrap().is_empty());
    assert_eq!(store.load_incoming_qos2("other").await.unwrap(), [1]);
    apply(store, "other", &[Write::DeleteSession]).await;
}

fn payloads(queue: Vec<(u64, Message)>) -> Vec<(u64, String)> {
    queue
        .into_iter()
        .map(|(sequence, message)| {
            let payload = message.payload.unwrap_or_default();
            (sequence, String::from_utf8(payload.to_vec()).unwrap())
        })
        .collect()
}

/// Each queued message leaves the queue on its own, in the batch that
/// stores it in flight when it is sent, batches apply in order, and
/// applying one twice is harmless.
async fn queue(store: &impl MqttStore) {
    let mut detailed = message("q", "1");
    detailed.qos = QoS::ExactlyOnce;
    detailed.retain = true;
    detailed.properties = properties();
    detailed.expires_at = Some(u64::MAX >> 1);
    detailed.subscription_identifiers = vec![4, 268_435_455];
    apply(store, "c", &[Write::Enqueue(u64::MAX, detailed.clone())]).await;
    let loaded = store.load_queue("c").await.unwrap();
    assert_eq!(loaded.len(), 1);
    let (sequence, loaded) = &loaded[0];
    assert_eq!(*sequence, u64::MAX);
    assert_eq!(
        (
            &*loaded.topic,
            &loaded.payload,
            loaded.qos,
            loaded.retain,
            &loaded.properties,
            loaded.expires_at,
            &loaded.subscription_identifiers
        ),
        (
            "q",
            &detailed.payload,
            QoS::ExactlyOnce,
            true,
            &detailed.properties,
            detailed.expires_at,
            &detailed.subscription_identifiers
        )
    );
    apply(store, "c", &[Write::ClearQueue]).await;
    assert!(store.load_queue("c").await.unwrap().is_empty());

    let batch = [
        Write::Enqueue(1, message("q", "1")),
        Write::Enqueue(3, message("q", "3")),
        Write::Enqueue(2, message("q", "2")),
        Write::StoreInflight(inflight_message(9)),
        Write::Dequeue(1),
        Write::Dequeue(3),
        Write::Enqueue(3, message("q", "3 again")),
        Write::Enqueue(4, message("q", "4")),
        Write::Dequeue(4),
        // Removing what isn't stored is no error
        Write::Dequeue(10),
    ];
    for _ in 0..2 {
        apply(store, "c", &batch).await;
        assert_eq!(
            payloads(store.load_queue("c").await.unwrap()),
            [(2, "2".to_string()), (3, "3 again".to_string())]
        );
        let inflight = store.load_inflight("c").await.unwrap();
        assert_eq!(inflight.len(), 1);
        assert_eq!(inflight[0].packet_id, 9);
    }
    apply(store, "other", &[Write::Enqueue(2, message("q", "other"))]).await;
    apply(store, "c", &[Write::Dequeue(2)]).await;
    assert_eq!(
        payloads(store.load_queue("c").await.unwrap()),
        [(3, "3 again".to_string())]
    );
    assert_eq!(store.load_queue("other").await.unwrap().len(), 1);
    apply(store, "c", &[Write::DeleteSession]).await;
    apply(store, "other", &[Write::DeleteSession]).await;
}

/// The changes of one batch apply in the order given, whichever kind they
/// are, also where a store applies runs of them in bulk.
async fn batches(store: &impl MqttStore) {
    let first = [
        Write::SaveSession(SessionState {
            subscriptions: vec![StoredSubscription::new("a", QoS::AtMostOnce)],
            ..session("c")
        }),
        Write::SetExpiry {
            interval: 60,
            expires_at: 5,
        },
        Write::StoreInflight(inflight_message(1)),
        Write::StoreInflight(inflight_message(2)),
        Write::ReleaseInflight(2),
        Write::RemoveInflight(1),
        Write::Enqueue(1, message("q", "1")),
    ];
    apply(store, "c", &first).await;
    let loaded = store.load_session("c").await.unwrap().unwrap();
    assert_eq!(loaded.subscriptions.len(), 1);
    assert_eq!(
        (loaded.expiry_interval, loaded.expires_at),
        (Some(60), Some(5))
    );
    let left = store.load_inflight("c").await.unwrap();
    assert_eq!(left.len(), 1);
    assert!(left[0].released);

    // Rows stored by an earlier batch, then a run that reuses packet id 1
    // and clears the queue between two enqueues
    apply(
        store,
        "c",
        &[
            Write::StoreInflight(inflight_message(4)),
            Write::StoreInflight(inflight_message(5)),
        ],
    )
    .await;
    let mut reused = inflight_message(1);
    reused.sequence = 7;
    let second = [
        Write::ReleaseInflight(4),
        Write::RemoveInflight(5),
        Write::StoreInflight(inflight_message(1)),
        Write::RemoveInflight(1),
        Write::StoreInflight(reused),
        Write::ReleaseInflight(1),
        Write::Enqueue(5, message("q", "dropped")),
        Write::ClearQueue,
        Write::Enqueue(6, message("q", "kept")),
        Write::Enqueue(6, message("q", "kept, again")),
        Write::StoreIncomingQos2(3),
        Write::RemoveIncomingQos2(3),
        Write::StoreIncomingQos2(4),
    ];
    // Applied again, as after a failure, it leaves the same
    for _ in 0..2 {
        apply(store, "c", &second).await;
        let mut left = store.load_inflight("c").await.unwrap();
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
        if store.is_durable() {
            assert_eq!(
                payloads(store.load_queue("c").await.unwrap()),
                [(6, "kept, again".to_string())]
            );
        }
        assert_eq!(store.load_incoming_qos2("c").await.unwrap(), [4]);
    }

    // A session saved after its deletion in the same batch is stored, with
    // only what came after the deletion
    apply(
        store,
        "c",
        &[
            Write::DeleteSession,
            Write::SaveSession(SessionState {
                subscriptions: Vec::new(),
                ..session("c")
            }),
            Write::AddSubscription(StoredSubscription::new("x", QoS::AtLeastOnce)),
            Write::StoreInflight(inflight_message(8)),
        ],
    )
    .await;
    let loaded = store.load_session("c").await.unwrap().unwrap();
    assert_eq!(
        loaded.subscriptions,
        [StoredSubscription::new("x", QoS::AtLeastOnce)]
    );
    let left = store.load_inflight("c").await.unwrap();
    assert_eq!(left.iter().map(|m| m.packet_id).collect::<Vec<_>>(), [8]);
    assert!(store.load_incoming_qos2("c").await.unwrap().is_empty());
    apply(store, "c", &[Write::DeleteSession]).await;
}

/// Deleting a session removes everything stored for its client, and only
/// for it, whether through `Write::DeleteSession` or `delete_session`.
async fn delete_cascades(store: &impl MqttStore) {
    let everything = |client_id: &str| {
        vec![
            Write::SaveSession(session(client_id)),
            Write::StoreWill(StoredWillMessage {
                topic: "w".into(),
                payload: Bytes::from_static(b"bye"),
                qos: QoS::AtMostOnce,
                retain: false,
                delay_secs: 0,
                properties: Default::default(),
            }),
            Write::StoreInflight(inflight_message(1)),
            Write::StoreIncomingQos2(2),
            Write::Enqueue(3, message("q", "3")),
        ]
    };
    for client_id in ["a", "b", "kept"] {
        apply(store, client_id, &everything(client_id)).await;
    }
    apply(store, "a", &[Write::DeleteSession]).await;
    store.delete_session("b").await.unwrap();
    for client_id in ["a", "b"] {
        assert!(store.load_session(client_id).await.unwrap().is_none());
        assert!(store.load_will(client_id).await.unwrap().is_none());
        assert!(store.load_inflight(client_id).await.unwrap().is_empty());
        assert!(store
            .load_incoming_qos2(client_id)
            .await
            .unwrap()
            .is_empty());
        assert!(store.load_queue(client_id).await.unwrap().is_empty());
    }
    assert!(store.load_session("kept").await.unwrap().is_some());
    assert!(store.load_will("kept").await.unwrap().is_some());
    assert_eq!(store.load_inflight("kept").await.unwrap().len(), 1);
    assert_eq!(store.load_incoming_qos2("kept").await.unwrap(), [2]);
    if store.is_durable() {
        assert_eq!(store.load_queue("kept").await.unwrap().len(), 1);
    }
    store.delete_session("kept").await.unwrap();
    assert!(store.list_sessions().await.unwrap().is_empty());
}

/// What a stored client holds, comparable.
fn summary(mut client: StoredClient) -> String {
    client
        .session
        .subscriptions
        .sort_by(|a, b| a.topic_filter.cmp(&b.topic_filter));
    client.inflight.sort_by_key(|m| m.packet_id);
    client.incoming_qos2.sort_unstable();
    format!(
        "{:?} {:?} {:?} {:?} {:?}",
        client.session,
        client.will,
        payloads(client.queue),
        client.inflight,
        client.incoming_qos2
    )
}

/// Sessions load a page at a time, each with everything stored with it,
/// as `load_client` loads it, and stored rows without a session are left
/// out.
async fn bulk_load(store: &impl MqttStore) {
    let mut expected = Vec::new();
    for i in 0..10 {
        let client_id = format!("client/{i}");
        let mut writes = vec![Write::SaveSession(session(&client_id))];
        if i % 2 == 0 {
            writes.extend([
                Write::StoreWill(StoredWillMessage {
                    topic: format!("will/{i}"),
                    payload: Bytes::from_static(b"bye"),
                    qos: QoS::AtLeastOnce,
                    retain: false,
                    delay_secs: i,
                    properties: properties(),
                }),
                Write::Enqueue(1, message("q", &format!("{i}.1"))),
                Write::Enqueue(2, message("q", &format!("{i}.2"))),
                Write::StoreInflight(inflight_message(i as u16 + 1)),
                Write::StoreInflight(inflight_message(100)),
                Write::StoreIncomingQos2(i as u16),
            ]);
        }
        apply(store, &client_id, &writes).await;
        expected.push(client_id);
    }
    // Rows of clients without a session
    apply(
        store,
        "orphan",
        &[
            Write::StoreInflight(inflight_message(1)),
            Write::StoreIncomingQos2(1),
        ],
    )
    .await;

    let mut found = Vec::new();
    loop {
        let after = found.last().map(|c: &LoadedClient| c.client_id.clone());
        let page = store.load_clients(after.as_deref(), 3).await.unwrap();
        assert!(page.len() <= 3);
        let last = page.len() < 3;
        found.extend(page);
        if last {
            break;
        }
    }
    let mut ids: Vec<String> = found.iter().map(|c| c.client_id.clone()).collect();
    ids.sort();
    assert_eq!(ids, expected);
    for client in found {
        let alone = store.load_client(&client.client_id).await.unwrap().unwrap();
        let loaded = client.stored.unwrap();
        assert_eq!(loaded.session.client_id, client.client_id);
        assert_eq!(summary(loaded), summary(alone), "{}", client.client_id);
    }
    assert!(store.load_client("orphan").await.unwrap().is_none());

    for client_id in expected.iter().map(String::as_str).chain(["orphan"]) {
        store.delete_session(client_id).await.unwrap();
    }
    assert!(store.load_clients(None, 3).await.unwrap().is_empty());
}

async fn topics(store: &impl MqttStore, filter: &str) -> Vec<String> {
    let mut topics: Vec<String> = store
        .get_retained(filter)
        .await
        .unwrap()
        .iter()
        .map(|m| {
            assert!(m.retain, "retained messages are delivered with RETAIN");
            m.topic.to_string()
        })
        .collect();
    topics.sort();
    topics
}

async fn retained(store: &impl MqttStore) {
    let mut kept = message("a/b", "kept");
    kept.qos = QoS::ExactlyOnce;
    kept.properties = properties();
    kept.expires_at = Some(u64::MAX >> 1);
    kept.origin = Some(Arc::new(MessageOrigin {
        client_id: "c".into(),
        username: Some("alice".into()),
    }));
    store
        .store_retained("a/b", Some(kept.clone()))
        .await
        .unwrap();
    let found = store.get_retained("a/b").await.unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(
        (
            &*found[0].topic,
            &found[0].payload,
            found[0].qos,
            &found[0].properties,
            found[0].expires_at,
            found[0].origin.as_deref()
        ),
        (
            "a/b",
            &kept.payload,
            QoS::ExactlyOnce,
            &kept.properties,
            kept.expires_at,
            kept.origin.as_deref()
        )
    );

    for topic in [
        "a", "ab", "a%b/c", "axb/c", "a_b/c", "$SYS/x", "a/b/c", "é/x",
    ] {
        store
            .store_retained(topic, Some(message(topic, topic)))
            .await
            .unwrap();
    }
    // Replacing keeps one per topic
    store
        .store_retained("a", Some(message("a", "again")))
        .await
        .unwrap();
    let mut expired = message("a/expired", "gone");
    expired.expires_at = Some(1);
    store
        .store_retained("a/expired", Some(expired))
        .await
        .unwrap();

    assert_eq!(topics(store, "a/#").await, ["a", "a/b", "a/b/c"]);
    assert_eq!(topics(store, "a/+").await, ["a/b"]);
    assert_eq!(topics(store, "a%b/+").await, ["a%b/c"]);
    assert_eq!(topics(store, "a_b/#").await, ["a_b/c"]);
    assert_eq!(topics(store, "+/c").await, ["a%b/c", "a_b/c", "axb/c"]);
    assert_eq!(topics(store, "+/+/c").await, ["a/b/c"]);
    assert_eq!(topics(store, "é/+").await, ["é/x"]);
    assert_eq!(topics(store, "$SYS/#").await, ["$SYS/x"]);
    assert_eq!(topics(store, "a/expired").await, Vec::<String>::new());
    assert_eq!(topics(store, "#").await.len(), 8);
    let again = store.get_retained("a").await.unwrap();
    assert_eq!(again[0].payload.as_deref(), Some(&b"again"[..]));

    store.store_retained("a/b", None).await.unwrap();
    // Clearing what isn't retained is no error
    store.store_retained("a/b", None).await.unwrap();
    assert!(store.get_retained("a/b").await.unwrap().is_empty());
    assert!(store.count_retained().await.unwrap() >= 8);

    store.clear_all_retained().await.unwrap();
    assert!(store.get_retained("#").await.unwrap().is_empty());
    assert_eq!(store.count_retained().await.unwrap(), 0);
}

/// Retained messages come a page at a time, in topic order, expired ones
/// left out until they are removed, and storing tells whether the topic
/// held one before.
async fn retained_pages(store: &impl MqttStore) {
    assert!(!store
        .store_retained("p", Some(message("p", "p")))
        .await
        .unwrap());
    assert!(store
        .store_retained("p", Some(message("p", "again")))
        .await
        .unwrap());
    let mut expected = vec!["p".to_string()];
    for i in 0..250 {
        let topic = format!("p/{i:03}");
        store
            .store_retained(&topic, Some(message(&topic, "")))
            .await
            .unwrap();
        expected.push(topic);
    }
    for topic in ["q/x", "o/x", "$SYS/p"] {
        store
            .store_retained(topic, Some(message(topic, "")))
            .await
            .unwrap();
    }
    let mut expired = message("p/expired", "");
    expired.expires_at = Some(1);
    store
        .store_retained("p/expired", Some(expired))
        .await
        .unwrap();

    for (filter, wanted) in [
        ("p/#", expected.clone()),
        (
            "+/+",
            expected[1..]
                .iter()
                .cloned()
                .chain(["q/x".into()])
                .collect(),
        ),
    ] {
        let mut wanted = wanted;
        if filter == "+/+" {
            wanted.insert(0, "o/x".into());
        }
        let mut found: Vec<String> = Vec::new();
        loop {
            let page = store
                .get_retained_page(filter, found.last().map(String::as_str), 7)
                .await
                .unwrap();
            assert!(page.len() <= 7);
            let last = page.len() < 7;
            found.extend(page.iter().map(|m| m.topic.to_string()));
            if last {
                break;
            }
        }
        assert_eq!(found, wanted, "{filter}");
    }
    let exact = store.get_retained_page("p/007", None, 7).await.unwrap();
    assert_eq!(exact.len(), 1);
    assert!(store
        .get_retained_page("p/007", Some("p/007"), 7)
        .await
        .unwrap()
        .is_empty());

    // The expired message counts until it is removed
    let count = store.count_retained().await.unwrap();
    assert_eq!(count, 255);
    assert_eq!(store.remove_expired_retained().await.unwrap(), 1);
    assert_eq!(store.count_retained().await.unwrap(), 254);
    assert_eq!(store.remove_expired_retained().await.unwrap(), 0);

    assert!(store.store_retained("p", None).await.unwrap());
    assert!(!store.store_retained("p", None).await.unwrap());
    store.clear_all_retained().await.unwrap();
}

pub fn ban(kind: &str, value: &str, prefix: bool) -> StoredBan {
    StoredBan {
        kind: kind.into(),
        value: value.into(),
        prefix,
        reason: None,
        created_at: 1_800_000_000,
        expires_at: None,
    }
}

/// What every `BanStore` does: a ban is named by its kind, value and
/// prefix flag, and storing one again replaces it.
pub async fn bans(store: &dyn BanStore) {
    let sorted = |mut bans: Vec<StoredBan>| {
        bans.sort_by(|a, b| (&a.kind, &a.value, a.prefix).cmp(&(&b.kind, &b.value, b.prefix)));
        bans
    };
    assert!(store.load_bans().await.unwrap().is_empty());

    let mut exact = ban("clientid", "c", false);
    exact.reason = Some("spam".into());
    exact.expires_at = Some(1_800_003_600);
    let prefixed = ban("clientid", "c", true);
    let address = ban("address", "10.0.0.0/8", false);
    for stored in [&exact, &prefixed, &address] {
        store.store_ban(stored).await.unwrap();
    }
    assert_eq!(
        sorted(store.load_bans().await.unwrap()),
        vec![address.clone(), exact.clone(), prefixed.clone()]
    );

    exact.reason = None;
    exact.expires_at = None;
    exact.created_at = 1_800_000_100;
    store.store_ban(&exact).await.unwrap();
    store.delete_ban("clientid", "c", true).await.unwrap();
    // Deleting what isn't stored is no error
    store.delete_ban("username", "c", false).await.unwrap();
    assert_eq!(
        sorted(store.load_bans().await.unwrap()),
        vec![address, exact]
    );
}
