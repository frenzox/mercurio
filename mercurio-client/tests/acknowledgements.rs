use std::{net::SocketAddr, time::Duration};

use mercurio_client::{ClientError, ConnectOptions, Event, MqttClient, QoS};
use mercurio_core::reason::ReasonCode;
use tokio::{net::TcpListener, time::timeout};

async fn start_broker() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(mercurio_server::server::run(
        listener,
        std::future::pending::<()>(),
    ));
    addr
}

async fn client(addr: SocketAddr, id: &str) -> MqttClient {
    MqttClient::connect(ConnectOptions::new("127.0.0.1", addr.port()).client_id(id))
        .await
        .unwrap()
}

async fn next_message(client: &MqttClient) -> Option<(String, Vec<u8>, QoS)> {
    match timeout(Duration::from_millis(500), client.recv()).await {
        Ok(Some(Event::Message {
            topic,
            payload,
            qos,
            ..
        })) => Some((topic, payload.to_vec(), qos)),
        _ => None,
    }
}

#[tokio::test]
async fn qos2_is_delivered_exactly_once_in_both_directions() {
    let addr = start_broker().await;
    let subscriber = client(addr, "sub").await;
    let publisher = client(addr, "pub").await;

    let results = subscriber
        .subscribe(&[("q2", QoS::ExactlyOnce)])
        .await
        .unwrap();
    assert_eq!(results[0].qos, QoS::ExactlyOnce);

    // Returns once the broker sent PUBCOMP
    publisher
        .publish("q2", "once", QoS::ExactlyOnce)
        .await
        .unwrap();

    assert_eq!(
        next_message(&subscriber).await,
        Some(("q2".into(), b"once".to_vec(), QoS::ExactlyOnce))
    );
    assert_eq!(next_message(&subscriber).await, None);
}

#[tokio::test]
async fn subscribe_reports_what_the_broker_granted() {
    let addr = start_broker().await;
    let subscriber = client(addr, "sub").await;

    let results = subscriber
        .subscribe(&[
            ("ok/+", QoS::AtLeastOnce),
            ("bad/#/filter", QoS::AtLeastOnce),
        ])
        .await
        .unwrap();

    assert!(results[0].success);
    assert_eq!(results[0].qos, QoS::AtLeastOnce);
    assert!(!results[1].success);
}

#[tokio::test]
async fn refused_publish_is_an_error() {
    let addr = start_broker().await;
    let publisher = client(addr, "pub").await;

    // Clients may not publish to $SYS
    let result = publisher
        .publish("$SYS/broker/version", "fake", QoS::AtLeastOnce)
        .await;
    assert!(matches!(
        result,
        Err(ClientError::PublishRefused(ReasonCode::NotAuthorized))
    ));
}
