#![cfg(unix)]

use std::time::Duration;

use mercurio_client::{ClientError, ConnectOptions, Event, MqttClient, QoS};
use mercurio_server::server::{ListenerConfig, ServerConfig, Transport};
use tokio::{
    net::{TcpListener, UnixListener},
    time::timeout,
};

#[tokio::test]
async fn clients_connect_through_unix_sockets() {
    let path = std::env::temp_dir().join(format!("mercurio-client-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut config = ServerConfig::default();
    config.listeners = vec![ListenerConfig::new(
        UnixListener::bind(&path).unwrap(),
        Transport::Mqtt,
    )];
    tokio::spawn(mercurio_server::server::run_with_config(
        TcpListener::bind("127.0.0.1:0").await.unwrap(),
        config,
        std::future::pending::<()>(),
    ));

    let client = MqttClient::connect(ConnectOptions::unix(&path))
        .await
        .unwrap();
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
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn a_missing_socket_fails_to_connect() {
    let path = std::env::temp_dir().join("mercurio-client-no-such.sock");
    let result = MqttClient::connect(ConnectOptions::unix(path)).await;
    assert!(matches!(result, Err(ClientError::ConnectionFailed(_))));
}
