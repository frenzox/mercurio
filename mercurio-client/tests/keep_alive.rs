use std::time::Duration;

use bytes::Bytes;
use mercurio_client::{Authenticator, ConnectOptions, DisconnectReason, Event, MqttClient};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
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

/// Takes over a second to answer a challenge, stalling the client.
struct Slow;

impl Authenticator for Slow {
    fn method(&self) -> String {
        "SLOW".into()
    }

    fn start(&mut self) -> Option<Bytes> {
        None
    }

    fn respond(&mut self, _: Option<Bytes>) -> Result<Option<Bytes>, String> {
        std::thread::sleep(Duration::from_millis(1300));
        Ok(None)
    }
}

/// Reads one packet whose remaining length fits in a byte.
async fn read_packet(socket: &mut TcpStream) -> Option<Vec<u8>> {
    let mut header = [0u8; 2];
    socket.read_exact(&mut header).await.ok()?;
    let mut body = vec![0u8; header[1] as usize];
    socket.read_exact(&mut body).await.ok()?;
    Some([header.to_vec(), body].concat())
}

/// Stalls the client for longer than its ping interval during a
/// re-authentication, `ping_first` with a PINGREQ already sent, whose
/// PINGRESP only comes once the client is back.
async fn stall(ping_first: bool) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_packet(&mut socket).await.unwrap();
        // Server Keep Alive of 1 second
        socket
            .write_all(&[0x20, 0x06, 0x00, 0x00, 0x03, 0x13, 0x00, 0x01])
            .await
            .unwrap();
        let mut challenged = false;
        let mut held_ping = false;
        while let Some(packet) = read_packet(&mut socket).await {
            let answer: &[u8] = match packet[0] {
                0xc0 if ping_first && !challenged => {
                    held_ping = true;
                    continue;
                }
                0xc0 => &[0xd0, 0x00],
                0xf0 if !challenged => {
                    challenged = true;
                    &[0xf0, 0x02, 0x18, 0x00]
                }
                0xf0 if held_ping => &[0xf0, 0x02, 0x00, 0x00, 0xd0, 0x00],
                0xf0 => &[0xf0, 0x02, 0x00, 0x00],
                _ => continue,
            };
            socket.write_all(answer).await.unwrap();
        }
    });

    let client = MqttClient::connect(ConnectOptions::new("127.0.0.1", port).authenticator(Slow))
        .await
        .unwrap();
    if ping_first {
        tokio::time::sleep(Duration::from_millis(600)).await;
    }
    timeout(Duration::from_secs(3), client.reauthenticate())
        .await
        .unwrap()
        .unwrap();
    let event = timeout(Duration::from_secs(2), client.recv()).await;
    assert!(
        event.is_err(),
        "connection ended after the stall: {event:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stalled_write_ends_the_connection_and_reconnects() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let broker = tokio::spawn(async move {
        // Stops reading after CONNACK, as a half-open connection would
        let (mut stuck, _) = listener.accept().await.unwrap();
        read_packet(&mut stuck).await.unwrap();
        stuck
            .write_all(&[0x20, 0x03, 0x00, 0x00, 0x00])
            .await
            .unwrap();
        let (mut socket, _) = listener.accept().await.unwrap();
        read_packet(&mut socket).await.unwrap();
        socket
            .write_all(&[0x20, 0x03, 0x00, 0x00, 0x00])
            .await
            .unwrap();
        (stuck, socket)
    });

    let client = MqttClient::connect(
        ConnectOptions::new("127.0.0.1", port)
            .keep_alive(1)
            .reconnect(Duration::from_millis(50)),
    )
    .await
    .unwrap();
    // Far more than the socket buffers take. Encoding it is slow in a
    // debug build on a busy machine, hence the long timeout.
    let published = timeout(
        Duration::from_secs(15),
        client.publish("t", vec![0u8; 32 << 20], mercurio_client::QoS::AtMostOnce),
    )
    .await
    .expect("the write never gave up");
    assert!(published.is_err());
    let event = timeout(Duration::from_secs(5), client.recv())
        .await
        .unwrap();
    assert!(
        matches!(
            event,
            Some(Event::Disconnected {
                reason: DisconnectReason::ConnectionLost
            })
        ),
        "{event:?}"
    );
    let event = timeout(Duration::from_secs(5), client.recv())
        .await
        .unwrap();
    assert!(
        matches!(event, Some(Event::Reconnected { .. })),
        "{event:?}"
    );
    let _sockets = broker.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stall_does_not_fire_missed_pings_in_a_burst() {
    stall(false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stall_does_not_count_against_an_outstanding_ping() {
    stall(true).await;
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
