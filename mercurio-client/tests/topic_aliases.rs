mod common;

use std::time::Duration;

use common::Peer;
use mercurio_client::{ConnectOptions, Event, MqttClient, QoS};
use mercurio_core::properties::TopicAliasMaximum;
use mercurio_packets::connack::ConnAckProperties;
use mercurio_packets::puback::PubAckPacket;
use mercurio_packets::publish::PublishPacket;
use mercurio_packets::ControlPacket;
use mercurio_server::server::ServerConfig;
use tokio::net::TcpListener;
use tokio::time::timeout;

fn aliases(maximum: u16) -> ConnAckProperties {
    ConnAckProperties {
        topic_alias_max: Some(TopicAliasMaximum::new(maximum)),
        ..Default::default()
    }
}

async fn next_publish(peer: &mut Peer) -> PublishPacket {
    match peer.read_skipping_pings().await {
        ControlPacket::Publish(publish) => publish,
        other => panic!("expected PUBLISH, got {other:?}"),
    }
}

/// The topic and alias a PUBLISH carried on the wire.
fn wire(publish: &PublishPacket) -> (String, Option<u16>) {
    let alias = publish
        .properties
        .as_ref()
        .and_then(|p| p.topic_alias.as_ref())
        .map(|a| a.value);
    (publish.topic_name.clone(), alias)
}

/// What the broker sees for QoS 0 publishes to `topics`, given the most
/// aliases the client may use and the most the broker announces.
async fn sent_for(
    client_maximum: Option<u16>,
    broker_maximum: u16,
    topics: &[&str],
) -> Vec<(String, Option<u16>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let count = topics.len();
    let broker = tokio::spawn(async move {
        let (mut peer, _) = Peer::accept_with(&listener, false, aliases(broker_maximum)).await;
        let mut seen = Vec::new();
        for _ in 0..count {
            seen.push(wire(&next_publish(&mut peer).await));
        }
        seen
    });
    let mut options = ConnectOptions::new("127.0.0.1", port);
    if let Some(maximum) = client_maximum {
        options = options.outbound_topic_aliases(maximum);
    }
    let client = MqttClient::connect(options).await.unwrap();
    for topic in topics {
        client.publish(topic, "x", QoS::AtMostOnce).await.unwrap();
    }
    timeout(Duration::from_secs(10), broker)
        .await
        .unwrap()
        .unwrap()
}

fn full(topic: &str, alias: Option<u16>) -> (String, Option<u16>) {
    (topic.to_string(), alias)
}

fn aliased(alias: u16) -> (String, Option<u16>) {
    (String::new(), Some(alias))
}

#[tokio::test]
async fn repeated_topics_go_out_as_aliases_replacing_the_least_recently_used() {
    let seen = sent_for(Some(10), 2, &["a", "a", "b", "b", "c", "a", "c"]).await;
    assert_eq!(
        seen,
        [
            full("a", Some(1)),
            aliased(1),
            full("b", Some(2)),
            aliased(2),
            // The broker allows two: `a`, used least recently, gives way
            full("c", Some(1)),
            full("a", Some(2)),
            aliased(1),
        ]
    );
}

#[tokio::test]
async fn aliases_stay_within_the_client_maximum() {
    let seen = sent_for(Some(1), 10, &["a", "b", "b"]).await;
    assert_eq!(seen, [full("a", Some(1)), full("b", Some(1)), aliased(1)]);
}

#[tokio::test]
async fn no_aliases_unless_both_sides_allow_them() {
    let plain = [full("a", None), full("a", None)];
    assert_eq!(sent_for(None, 10, &["a", "a"]).await, plain);
    assert_eq!(sent_for(Some(10), 0, &["a", "a"]).await, plain);
}

#[tokio::test]
async fn aliases_start_over_on_each_connection_and_never_reach_resent_messages() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let broker = tokio::spawn(async move {
        let (mut peer, _) = Peer::accept_with(&listener, false, aliases(5)).await;
        let first = next_publish(&mut peer).await;
        let second = next_publish(&mut peer).await;
        assert_eq!(wire(&first), full("a", Some(1)));
        assert_eq!(wire(&second), aliased(1));
        // Neither is acknowledged; the session survives the connection
        drop(peer);

        let (mut peer, _) = Peer::accept_with(&listener, true, aliases(5)).await;
        let mut resent = Vec::new();
        for _ in 0..2 {
            let publish = next_publish(&mut peer).await;
            assert!(publish.dup);
            resent.push(wire(&publish));
            peer.write(ControlPacket::PubAck(PubAckPacket {
                packet_id: publish.packet_id.unwrap(),
                ..Default::default()
            }))
            .await;
        }
        // A new message defines the alias again on this connection
        let fresh = wire(&next_publish(&mut peer).await);
        (resent, fresh)
    });

    let client = MqttClient::connect(
        ConnectOptions::new("127.0.0.1", port)
            .client_id("aliases")
            .clean_start(false)
            .session_expiry(60)
            .reconnect(Duration::from_millis(50))
            .outbound_topic_aliases(5),
    )
    .await
    .unwrap();
    let first = client
        .start_publish("a", "1", QoS::AtLeastOnce, false, Default::default())
        .await
        .unwrap();
    let second = client
        .start_publish("a", "2", QoS::AtLeastOnce, false, Default::default())
        .await
        .unwrap();
    first.await.unwrap();
    second.await.unwrap();
    client.publish("a", "3", QoS::AtMostOnce).await.unwrap();

    let (resent, fresh) = timeout(Duration::from_secs(10), broker)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resent, [full("a", None), full("a", None)]);
    assert_eq!(fresh, full("a", Some(1)));
}

#[tokio::test]
async fn aliased_messages_reach_subscribers_with_their_topics() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(mercurio_server::server::run_with_config(
        listener,
        ServerConfig::default(),
        std::future::pending::<()>(),
    ));
    let subscriber = MqttClient::connect(ConnectOptions::new("127.0.0.1", port))
        .await
        .unwrap();
    subscriber
        .subscribe(&[("t/#", QoS::AtLeastOnce)])
        .await
        .unwrap();
    let publisher =
        MqttClient::connect(ConnectOptions::new("127.0.0.1", port).outbound_topic_aliases(2))
            .await
            .unwrap();
    let topics = ["t/a", "t/a", "t/b", "t/c", "t/a", "t/b"];
    for (i, topic) in topics.iter().enumerate() {
        publisher
            .publish(topic, i.to_string(), QoS::AtLeastOnce)
            .await
            .unwrap();
    }
    for (i, expected) in topics.iter().enumerate() {
        let event = timeout(Duration::from_secs(5), subscriber.recv())
            .await
            .unwrap()
            .unwrap();
        let Event::Message { topic, payload, .. } = event else {
            panic!("expected a message, got {event:?}");
        };
        assert_eq!(
            (topic.as_str(), &payload[..]),
            (*expected, i.to_string().as_bytes())
        );
    }
}
