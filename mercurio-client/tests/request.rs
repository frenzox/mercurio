mod common;

use std::time::Duration;

use bytes::Bytes;
use common::Peer;
use mercurio_client::{
    ClientError, ConnectOptions, Event, MessageProperties, MqttClient, QoS, ReasonCode,
    RequestOptions,
};
use mercurio_core::properties::{CorrelationData, ResponseInformation};
use mercurio_packets::connack::ConnAckProperties;
use mercurio_packets::puback::PubAckPacket;
use mercurio_packets::publish::{PublishPacket, PublishProperties};
use mercurio_packets::ControlPacket;
use mercurio_server::server::ServerConfig;
use tokio::net::TcpListener;
use tokio::time::timeout;

fn message(
    topic: &str,
    payload: &str,
    correlation: Option<&Bytes>,
    packet_id: Option<u16>,
) -> ControlPacket {
    ControlPacket::Publish(PublishPacket {
        qos_level: if packet_id.is_some() {
            QoS::AtLeastOnce
        } else {
            QoS::AtMostOnce
        },
        topic_name: topic.to_string(),
        packet_id,
        properties: correlation.map(|data| PublishProperties {
            correlation_data: Some(CorrelationData::new(data.clone())),
            ..Default::default()
        }),
        payload: Some(Bytes::from(payload.to_string())),
        ..Default::default()
    })
}

async fn next_publish(peer: &mut Peer) -> PublishPacket {
    match peer.read_skipping_pings().await {
        ControlPacket::Publish(publish) => publish,
        other => panic!("expected PUBLISH, got {other:?}"),
    }
}

/// The response topic and correlation data of a request.
fn addressing(publish: &PublishPacket) -> (String, Bytes) {
    let properties = publish.properties.as_ref().expect("properties");
    (
        properties.response_topic.as_ref().unwrap().value.clone(),
        properties.correlation_data.as_ref().unwrap().value.clone(),
    )
}

async fn puback(peer: &mut Peer, publish: &PublishPacket) {
    peer.write(ControlPacket::PubAck(PubAckPacket {
        packet_id: publish.packet_id.unwrap(),
        ..Default::default()
    }))
    .await;
}

#[tokio::test]
async fn responses_resolve_requests_and_never_become_events() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let broker = tokio::spawn(async move {
        let properties = ConnAckProperties {
            response_information: Some(ResponseInformation::new("resp/".to_string())),
            ..Default::default()
        };
        let (mut peer, _) = Peer::accept_with(&listener, false, properties).await;
        let ControlPacket::Subscribe(subscribe) = peer.read_skipping_pings().await else {
            panic!("expected SUBSCRIBE");
        };
        let filter = subscribe.payload[0].topic_filter.clone();
        assert!(filter.starts_with("resp/") && filter.len() > 5, "{filter}");
        peer.suback(subscribe.packet_id, &[ReasonCode::GrantedQoS1])
            .await;

        let request = next_publish(&mut peer).await;
        assert_eq!(request.topic_name, "svc");
        let (response_topic, correlation) = addressing(&request);
        assert_eq!(response_topic, filter);
        puback(&mut peer, &request).await;
        peer.write(message("other", "event", None, None)).await;
        // An answer to no request of this client
        peer.write(message(
            &response_topic,
            "stray",
            Some(&Bytes::from("x")),
            None,
        ))
        .await;
        peer.write(message(
            &response_topic,
            "pong",
            Some(&correlation),
            Some(7),
        ))
        .await;
        let ControlPacket::PubAck(ack) = peer.read_skipping_pings().await else {
            panic!("the response wasn't acknowledged");
        };
        assert_eq!(ack.packet_id, 7);

        // Subscribed already; this one goes unanswered
        let second = next_publish(&mut peer).await;
        let (topic, other) = addressing(&second);
        assert_eq!(topic, response_topic);
        assert_ne!(other, correlation);
        puback(&mut peer, &second).await;
        peer.next(Duration::from_secs(2)).await;
    });

    let client = MqttClient::connect(
        ConnectOptions::new("127.0.0.1", port)
            .keep_alive(0)
            .manual_acks(true),
    )
    .await
    .unwrap();
    let response = client
        .request("svc", "ping", RequestOptions::new(Duration::from_secs(5)))
        .await
        .unwrap();
    assert_eq!(&response.payload[..], b"pong");
    let unanswered = client
        .request(
            "svc",
            "ping",
            RequestOptions::new(Duration::from_millis(300)),
        )
        .await;
    assert!(
        matches!(unanswered, Err(ClientError::Timeout)),
        "{unanswered:?}"
    );

    let event = timeout(Duration::from_secs(2), client.recv())
        .await
        .unwrap();
    let Some(Event::Message { topic, .. }) = event else {
        panic!("expected the other message, got {event:?}");
    };
    assert_eq!(topic, "other");
    let more = timeout(Duration::from_millis(200), client.recv()).await;
    assert!(more.is_err(), "{more:?}");
    broker.abort();
}

#[tokio::test]
async fn a_response_topic_needs_response_information_or_a_prefix() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _peer = Peer::accept_with(&listener, false, Default::default()).await;
        std::future::pending::<()>().await;
    });
    let client = MqttClient::connect(ConnectOptions::new("127.0.0.1", port))
        .await
        .unwrap();
    let result = client
        .request("svc", "ping", RequestOptions::new(Duration::from_secs(1)))
        .await;
    assert!(
        matches!(result, Err(ClientError::InvalidArgument(_))),
        "{result:?}"
    );
    let result = client
        .request(
            "svc",
            "ping",
            RequestOptions::new(Duration::from_secs(1)).response_prefix("a/+"),
        )
        .await;
    assert!(
        matches!(result, Err(ClientError::InvalidArgument(_))),
        "{result:?}"
    );
}

#[tokio::test]
async fn requests_reach_a_responder_through_the_broker() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut config = ServerConfig::default();
    config.sys_interval = None;
    tokio::spawn(common::broker(listener, config).run(std::future::pending::<()>()));

    let responder = MqttClient::connect(ConnectOptions::new("127.0.0.1", port))
        .await
        .unwrap();
    responder
        .subscribe(&[("svc/echo", QoS::AtLeastOnce)])
        .await
        .unwrap();
    tokio::spawn(async move {
        while let Some(event) = responder.recv().await {
            if let Event::Message {
                payload,
                properties,
                ..
            } = event
            {
                let reply = MessageProperties::default()
                    .correlation_data(properties.correlation_data.unwrap());
                let mut answer = b"echo:".to_vec();
                answer.extend_from_slice(&payload);
                responder
                    .publish_with_properties(
                        &properties.response_topic.unwrap(),
                        answer,
                        QoS::AtLeastOnce,
                        false,
                        reply,
                    )
                    .await
                    .unwrap();
            }
        }
    });

    let client = MqttClient::connect(ConnectOptions::new("127.0.0.1", port))
        .await
        .unwrap();
    let options = RequestOptions::new(Duration::from_secs(5)).response_prefix("replies");
    let (a, b) = tokio::join!(
        client.request("svc/echo", "a", options.clone()),
        client.request("svc/echo", "b", options.clone()),
    );
    assert_eq!(&a.unwrap().payload[..], b"echo:a");
    let b = b.unwrap();
    assert_eq!(&b.payload[..], b"echo:b");
    assert!(b.topic.starts_with("replies/"), "{}", b.topic);

    let third = client
        .request(
            "svc/echo",
            "c",
            options.clone().properties(MessageProperties::default()),
        )
        .await
        .unwrap();
    assert_eq!(&third.payload[..], b"echo:c");
}
