#![cfg(feature = "tls")]

use std::time::Duration;

use mercurio_client::{ClientError, ConnectOptions, Event, MqttClient, QoS};
use mercurio_server::{
    server::ServerConfig,
    tls::{Tls, TlsConfig},
};
use tokio::{net::TcpListener, time::timeout};

const CA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/ca.pem");
const CERT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/server.pem");
const KEY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/server-key.pem");

/// A broker serving TLS with the test CA's certificate for `localhost`.
async fn start_broker() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut config = ServerConfig::default();
    config.tls = Some(Tls::new(TlsConfig::new(CERT, KEY)).unwrap());
    tokio::spawn(mercurio_server::server::run_with_config(
        listener,
        config,
        std::future::pending::<()>(),
    ));
    port
}

#[tokio::test]
async fn a_broker_verified_with_the_ca_file_carries_messages() {
    let port = start_broker().await;
    let client = MqttClient::connect(ConnectOptions::new("localhost", port).ca_path(CA))
        .await
        .unwrap();
    client.subscribe(&[("t", QoS::AtLeastOnce)]).await.unwrap();
    client
        .publish("t", "secret", QoS::AtLeastOnce)
        .await
        .unwrap();
    let event = timeout(Duration::from_secs(2), client.recv())
        .await
        .unwrap()
        .unwrap();
    let Event::Message { payload, .. } = event else {
        panic!("expected a message, got {event:?}");
    };
    assert_eq!(&payload[..], b"secret");
}

#[tokio::test]
async fn a_broker_outside_the_trusted_roots_is_refused() {
    let port = start_broker().await;
    // The test CA is in neither the system's store nor the bundled roots
    let result = MqttClient::connect(ConnectOptions::new("localhost", port).tls(true)).await;
    assert!(
        matches!(result, Err(ClientError::Tls { .. })),
        "{:?}",
        result.err()
    );
}
