#![cfg(feature = "websocket")]

use std::{net::SocketAddr, time::Duration};

use mercurio_client::{ClientError, ConnectOptions, Event, MqttClient, QoS};
use mercurio_server::server::{ListenerConfig, ServerConfig, Transport};
use tokio::{net::TcpListener, time::timeout};

/// Starts a broker with a WebSocket listener, given `tls` for it, and
/// returns that listener's address.
async fn start_broker(tls: Option<mercurio_server::tls::TlsConfig>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let websocket_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = websocket_listener.local_addr().unwrap();
    let mut websocket = ListenerConfig::new(websocket_listener, Transport::WebSocket);
    websocket.tls = tls;
    let mut config = ServerConfig::default();
    config.listeners = vec![websocket];
    tokio::spawn(mercurio_server::server::run_with_config(
        listener,
        config,
        std::future::pending::<()>(),
    ));
    address
}

async fn round_trip(options: ConnectOptions) {
    let client = MqttClient::connect(options).await.unwrap();
    client
        .subscribe(&[("ws/t", QoS::AtLeastOnce)])
        .await
        .unwrap();
    // Larger than a frame buffer, so the packet spans reads
    let payload = vec![b'x'; 70_000];
    client
        .publish("ws/t", payload.clone(), QoS::AtLeastOnce)
        .await
        .unwrap();
    let event = timeout(Duration::from_secs(2), client.recv())
        .await
        .unwrap()
        .unwrap();
    let Event::Message { payload: got, .. } = event else {
        panic!("expected a message, got {event:?}");
    };
    assert_eq!(&got[..], &payload[..]);
    client.disconnect().await.unwrap();
}

#[tokio::test]
async fn messages_travel_over_websocket() {
    let address = start_broker(None).await;
    round_trip(ConnectOptions::new("127.0.0.1", address.port()).websocket("/mqtt")).await;
}

#[cfg(feature = "tls")]
#[tokio::test]
async fn messages_travel_over_secure_websocket() {
    let fixtures = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
    let tls = mercurio_server::tls::TlsConfig::new(
        format!("{fixtures}/server.pem"),
        format!("{fixtures}/server-key.pem"),
    );
    let address = start_broker(Some(tls)).await;
    let options = ConnectOptions::new("localhost", address.port())
        .ca_path(format!("{fixtures}/ca.pem"))
        .websocket("/mqtt");
    round_trip(options).await;
}

#[tokio::test]
async fn a_plain_mqtt_listener_fails_the_websocket_handshake() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(mercurio_server::server::run_with_config(
        listener,
        ServerConfig::default(),
        std::future::pending::<()>(),
    ));
    let options = ConnectOptions::new("127.0.0.1", port)
        .websocket("/mqtt")
        .connect_timeout(Duration::from_secs(2));
    let result = MqttClient::connect(options).await;
    assert!(
        matches!(result, Err(ClientError::ConnectionFailed(_))),
        "{:?}",
        result.err()
    );
}

#[test]
fn a_path_must_start_with_a_slash() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let result = runtime.block_on(MqttClient::connect(
        ConnectOptions::new("127.0.0.1", 1).websocket("mqtt"),
    ));
    assert!(
        matches!(result, Err(ClientError::InvalidArgument(_))),
        "{:?}",
        result.err()
    );
}
