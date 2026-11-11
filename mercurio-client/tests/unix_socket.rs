#![cfg(unix)]

mod common;

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
    let config = ServerConfig::default();
    let listeners = vec![ListenerConfig::new(
        UnixListener::bind(&path).unwrap(),
        Transport::Mqtt,
    )];
    tokio::spawn(
        common::broker(TcpListener::bind("127.0.0.1:0").await.unwrap(), config)
            .listeners(listeners)
            .run(std::future::pending::<()>()),
    );

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
    let Err(error @ ClientError::ConnectionFailed(_)) = result else {
        panic!("connecting to a missing socket gave {:?}", result.err());
    };
    // The I/O error stays available to tell causes apart
    let ClientError::ConnectionFailed(io) = &error else {
        unreachable!()
    };
    assert_eq!(io.kind(), std::io::ErrorKind::NotFound);
    let source = std::error::Error::source(&error).expect("the I/O error is the source");
    assert!(source.downcast_ref::<std::io::Error>().is_some());
    assert!(error.is_transient(), "the socket may appear later");
}
