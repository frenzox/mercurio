use std::time::Duration;

use mercurio_client::{ConnectOptions, Event, MqttClient, QoS};
use mercurio_server::server::{LocalConnector, ServerConfig};
use tokio::{net::TcpListener, time::timeout};

#[tokio::test]
async fn connector_replaces_tcp() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (connector, local_listener) = LocalConnector::new();
    let config = ServerConfig {
        local_listener: Some(local_listener),
        ..Default::default()
    };
    tokio::spawn(mercurio_server::server::run_with_config(
        listener,
        config,
        std::future::pending::<()>(),
    ));

    // Nothing listens on this host and port; only the connector can work
    let options = ConnectOptions::new("unused.invalid", 1).connector(move || connector.connect());
    let client = MqttClient::connect(options).await.unwrap();
    client.subscribe(&[("t", QoS::AtLeastOnce)]).await.unwrap();
    client.publish("t", "hi", QoS::AtLeastOnce).await.unwrap();

    let event = timeout(Duration::from_secs(2), client.recv())
        .await
        .unwrap()
        .unwrap();
    let Event::Message { payload, .. } = event else {
        panic!("expected a message, got {event:?}");
    };
    assert_eq!(&payload[..], b"hi");
}
