mod common;

use std::time::Duration;

use common::Peer;
use mercurio_client::{
    ConnectOptions, Event, MqttClient, QoS, ReasonCode, RetainHandling, SubscriptionOptions,
};
use mercurio_packets::subscribe::SubscribePacket;
use mercurio_packets::unsuback::{UnsubAckPacket, UnsubAckPayload};
use mercurio_packets::ControlPacket;
use tokio::net::TcpListener;
use tokio::time::timeout;

const SENSORS: SubscriptionOptions = SubscriptionOptions {
    qos: QoS::AtLeastOnce,
    no_local: true,
    retain_as_pub: true,
    retain_handling: RetainHandling::SendRetainedIfNonExisting,
};

async fn next_subscribe(peer: &mut Peer) -> SubscribePacket {
    match peer.read_skipping_pings().await {
        ControlPacket::Subscribe(subscribe) => subscribe,
        other => panic!("expected SUBSCRIBE, got {other:?}"),
    }
}

/// The filters, options and identifier a SUBSCRIBE carries.
fn contents(subscribe: &SubscribePacket) -> (Vec<(String, SubscriptionOptions)>, Option<u32>) {
    let filters = subscribe
        .payload
        .iter()
        .map(|p| (p.topic_filter.clone(), p.subs_opt))
        .collect();
    let id = subscribe
        .properties
        .as_ref()
        .and_then(|p| p.subscription_id.as_ref())
        .map(|id| id.value.0);
    (filters, id)
}

fn granted(qos: QoS) -> ReasonCode {
    match qos {
        QoS::AtLeastOnce => ReasonCode::GrantedQoS1,
        QoS::ExactlyOnce => ReasonCode::GrantedQoS2,
        _ => ReasonCode::GrantedQoS0,
    }
}

fn options(qos: QoS) -> SubscriptionOptions {
    SubscriptionOptions {
        qos,
        ..Default::default()
    }
}

/// Answers the client's first subscriptions, granting all but `refused`
/// and an unsubscription, then drops the connection.
async fn first_connection(listener: TcpListener) -> TcpListener {
    let (mut peer, _) = Peer::accept_with(&listener, false, Default::default()).await;
    for _ in 0..4 {
        let subscribe = next_subscribe(&mut peer).await;
        let refused = subscribe.payload[0].topic_filter == "refused";
        let reasons: Vec<ReasonCode> = subscribe
            .payload
            .iter()
            .map(|p| match refused {
                true => ReasonCode::NotAuthorized,
                false => granted(p.subs_opt.qos),
            })
            .collect();
        peer.suback(subscribe.packet_id, &reasons).await;
    }
    let ControlPacket::Unsubscribe(unsubscribe) = peer.read_skipping_pings().await else {
        panic!("expected UNSUBSCRIBE");
    };
    peer.write(ControlPacket::UnsubAck(UnsubAckPacket {
        packet_id: unsubscribe.packet_id,
        properties: None,
        payload: vec![UnsubAckPayload {
            reason_code: ReasonCode::Success,
        }],
    }))
    .await;
    listener
}

async fn subscribe_all(client: &MqttClient) {
    client
        .subscribe_with(&[("sensors/#", SENSORS)], Some(7))
        .await
        .unwrap();
    client
        .subscribe_with(
            &[
                ("b", options(QoS::AtMostOnce)),
                ("gone", options(QoS::AtMostOnce)),
            ],
            None,
        )
        .await
        .unwrap();
    let refused = client
        .subscribe(&[("refused", QoS::AtMostOnce)])
        .await
        .unwrap();
    assert!(!refused[0].success);
    client
        .subscribe_with(&[("c", options(QoS::ExactlyOnce))], Some(9))
        .await
        .unwrap();
    client.unsubscribe(&["gone"]).await.unwrap();
}

async fn next_event(client: &MqttClient, wait: Duration) -> Option<Event> {
    timeout(wait, client.recv()).await.ok().flatten()
}

#[tokio::test]
async fn subscriptions_are_made_again_before_reconnected_when_the_session_is_gone() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let client = MqttClient::connect(
        ConnectOptions::new("127.0.0.1", port)
            .keep_alive(0)
            .reconnect(Duration::from_millis(50))
            .resubscribe(true),
    );
    let first = tokio::spawn(first_connection(listener));
    let client = client.await.unwrap();
    subscribe_all(&client).await;
    let listener = first.await.unwrap();

    // The broker comes back without the session
    let (mut peer, _) = Peer::accept_with(&listener, false, Default::default()).await;
    assert!(matches!(
        next_event(&client, Duration::from_secs(5)).await,
        Some(Event::Disconnected { .. })
    ));
    let mut resent = Vec::new();
    for _ in 0..3 {
        resent.push(next_subscribe(&mut peer).await);
    }
    // Nothing until the broker has answered
    assert!(next_event(&client, Duration::from_millis(200))
        .await
        .is_none());
    let mut seen: Vec<_> = resent.iter().map(contents).collect();
    seen.sort_by_key(|(_, id)| *id);
    assert_eq!(
        seen,
        [
            (vec![("b".to_string(), options(QoS::AtMostOnce))], None),
            (vec![("sensors/#".to_string(), SENSORS)], Some(7)),
            (vec![("c".to_string(), options(QoS::ExactlyOnce))], Some(9)),
        ]
    );
    for subscribe in &resent {
        let reason = match subscribe.payload[0].topic_filter.as_str() {
            "b" => ReasonCode::NotAuthorized,
            _ => granted(subscribe.payload[0].subs_opt.qos),
        };
        peer.suback(subscribe.packet_id, &[reason]).await;
    }

    assert!(matches!(
        next_event(&client, Duration::from_secs(5)).await,
        Some(Event::Reconnected {
            session_present: false,
            ..
        })
    ));
    match next_event(&client, Duration::from_secs(5)).await {
        Some(Event::ResubscribeFailed { topic, reason, .. }) => {
            assert_eq!((topic.as_str(), reason), ("b", ReasonCode::NotAuthorized));
        }
        other => panic!("expected ResubscribeFailed, got {other:?}"),
    }

    // A refused subscription isn't made again on the next reconnection
    drop(peer);
    let (mut peer, _) = Peer::accept_with(&listener, false, Default::default()).await;
    let mut filters = Vec::new();
    for _ in 0..2 {
        let subscribe = next_subscribe(&mut peer).await;
        filters.push(subscribe.payload[0].topic_filter.clone());
        peer.suback(subscribe.packet_id, &[ReasonCode::Success])
            .await;
    }
    filters.sort();
    assert_eq!(filters, ["c", "sensors/#"]);
}

#[tokio::test]
async fn a_kept_session_or_the_option_off_sends_no_subscriptions() {
    for (resubscribe, session_present) in [(true, true), (false, false)] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let client = MqttClient::connect(
            ConnectOptions::new("127.0.0.1", port)
                .client_id("kept")
                .clean_start(false)
                .session_expiry(60)
                .keep_alive(0)
                .reconnect(Duration::from_millis(50))
                .resubscribe(resubscribe),
        );
        let first = tokio::spawn(first_connection(listener));
        let client = client.await.unwrap();
        subscribe_all(&client).await;
        let listener = first.await.unwrap();

        let (mut peer, _) = Peer::accept_with(&listener, session_present, Default::default()).await;
        assert!(matches!(
            next_event(&client, Duration::from_secs(5)).await,
            Some(Event::Disconnected { .. })
        ));
        assert!(matches!(
            next_event(&client, Duration::from_secs(5)).await,
            Some(Event::Reconnected { .. })
        ));
        let sent = peer.next(Duration::from_millis(300)).await;
        assert!(sent.is_none(), "{resubscribe} {session_present}: {sent:?}");
    }
}
