use std::time::Duration;

use mercurio_client::{ConnectOptions, Event, MqttClient};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

async fn accept(listener: &TcpListener) -> TcpStream {
    let (mut socket, _) = listener.accept().await.unwrap();
    let mut connect = [0u8; 256];
    let _ = socket.read(&mut connect).await.unwrap();
    socket
        .write_all(&[0x20, 0x03, 0x00, 0x00, 0x00])
        .await
        .unwrap();
    socket
}

#[tokio::test]
async fn dropping_the_client_disconnects_normally() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let broker = tokio::spawn(async move {
        let mut socket = accept(&listener).await;
        let mut received = Vec::new();
        let mut buf = [0u8; 64];
        loop {
            match timeout(Duration::from_secs(2), socket.read(&mut buf)).await {
                Ok(Ok(0)) => return (received, true),
                Ok(Ok(n)) => received.extend_from_slice(&buf[..n]),
                Ok(Err(_)) => return (received, true),
                Err(_) => return (received, false),
            }
        }
    });

    let client = MqttClient::connect(ConnectOptions::new("127.0.0.1", port).keep_alive(0))
        .await
        .unwrap();
    drop(client);

    let (received, closed) = broker.await.unwrap();
    // DISCONNECT with Normal Disconnection, so the broker drops the will
    assert_eq!(received, [0xe0, 0x01, 0x00], "{received:02x?}");
    assert!(closed, "the connection stayed open");
}

#[tokio::test]
async fn dropping_the_client_while_reconnecting_stops_it() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let client = MqttClient::connect(
        ConnectOptions::new("127.0.0.1", port)
            .keep_alive(0)
            .reconnect(Duration::from_millis(50)),
    );
    let (socket, client) = tokio::join!(accept(&listener), client);
    let client = client.unwrap();
    drop(socket);
    assert!(matches!(
        client.recv().await,
        Some(Event::Disconnected { .. })
    ));
    drop(client);

    assert!(
        timeout(Duration::from_millis(500), listener.accept())
            .await
            .is_err(),
        "a dropped client reconnected"
    );
}
