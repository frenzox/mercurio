use std::time::Duration;

use mercurio_client::{ConnectOptions, Event, MqttClient, ProtocolVersion, QoS};
use tokio::{net::TcpListener, time::timeout};

async fn round_trip(version: ProtocolVersion) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(mercurio_server::server::run(
        listener,
        std::future::pending::<()>(),
    ));

    let options = |id: &str| {
        ConnectOptions::new("127.0.0.1", port)
            .client_id(id)
            .protocol_version(version)
    };
    let subscriber = MqttClient::connect(options("sub")).await.unwrap();
    let publisher = MqttClient::connect(options("pub")).await.unwrap();

    let results = subscriber
        .subscribe(&[("a/+", QoS::AtLeastOnce)])
        .await
        .unwrap();
    assert!(results[0].success);

    publisher
        .publish("a/b", "first", QoS::AtLeastOnce)
        .await
        .unwrap();
    publisher
        .publish("a/b", "second", QoS::AtMostOnce)
        .await
        .unwrap();

    for expected in ["first", "second"] {
        let event = timeout(Duration::from_secs(2), subscriber.recv())
            .await
            .expect("timed out waiting for message")
            .unwrap();
        let Event::Message { topic, payload, .. } = event else {
            panic!("Expected message, got {:?}", event);
        };
        assert_eq!(topic, "a/b");
        assert_eq!(payload, expected);
    }
}

#[tokio::test]
async fn mqtt311_publish_subscribe() {
    round_trip(ProtocolVersion::V3_1_1).await;
}

#[tokio::test]
async fn mqtt5_publish_subscribe() {
    round_trip(ProtocolVersion::V5).await;
}
