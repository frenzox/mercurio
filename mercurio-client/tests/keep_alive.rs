use std::time::Duration;

use mercurio_client::{ConnectOptions, DisconnectReason, Event, MqttClient};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    time::timeout,
};

/// A broker that accepts the connection, imposes a 1 second keep-alive
/// and then never answers again.
async fn silent_broker() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut connect = [0u8; 256];
        let _ = socket.read(&mut connect).await.unwrap();
        // CONNACK with Server Keep Alive (0x13) of 1 second
        socket
            .write_all(&[0x20, 0x06, 0x00, 0x00, 0x03, 0x13, 0x00, 0x01])
            .await
            .unwrap();
        let mut sink = [0u8; 256];
        while socket.read(&mut sink).await.unwrap_or(0) > 0 {}
    });
    port
}

#[tokio::test]
async fn unanswered_pings_end_the_connection() {
    let port = silent_broker().await;
    // Asks for a minute; the broker's second wins
    let client = MqttClient::connect(ConnectOptions::new("127.0.0.1", port).keep_alive(60))
        .await
        .unwrap();

    let event = timeout(Duration::from_secs(3), client.recv())
        .await
        .expect("dead broker not noticed");
    assert!(matches!(
        event,
        Some(Event::Disconnected {
            reason: DisconnectReason::KeepAliveTimeout
        })
    ));
}

#[tokio::test]
async fn keep_alive_zero_sends_no_pings() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let broker = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut connect = [0u8; 256];
        let _ = socket.read(&mut connect).await.unwrap();
        socket
            .write_all(&[0x20, 0x03, 0x00, 0x00, 0x00])
            .await
            .unwrap();
        // Nothing should arrive
        let mut buf = [0u8; 16];
        timeout(Duration::from_secs(2), socket.read(&mut buf))
            .await
            .is_err()
    });

    let _client = MqttClient::connect(ConnectOptions::new("127.0.0.1", port).keep_alive(0))
        .await
        .unwrap();
    assert!(broker.await.unwrap(), "client sent something");
}
