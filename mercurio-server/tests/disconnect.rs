use std::{net::SocketAddr, time::Duration};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

async fn start_broker() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(mercurio_server::server::run(
        listener,
        std::future::pending::<()>(),
    ));
    addr
}

async fn read(stream: &mut TcpStream, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    timeout(Duration::from_secs(2), stream.read_exact(&mut buf))
        .await
        .expect("timed out waiting for broker")
        .unwrap();
    buf
}

/// MQTT 5 CONNECT; returns the stream and whether a session was resumed.
async fn connect(
    addr: SocketAddr,
    client_id: u8,
    clean: bool,
    session_expiry: u32,
    will: bool,
) -> (TcpStream, bool) {
    let mut flags = if clean { 0x02 } else { 0x00 };
    if will {
        flags |= 0x04;
    }
    let mut body = vec![0x00, 0x04, b'M', b'Q', b'T', b'T', 0x05, flags, 0x00, 0x3c];
    body.extend_from_slice(&[0x05, 0x11]);
    body.extend_from_slice(&session_expiry.to_be_bytes());
    body.extend_from_slice(&[0x00, 0x01, client_id]);
    if will {
        // No will properties, topic "w", payload "bye"
        body.extend_from_slice(&[0x00, 0x00, 0x01, b'w', 0x00, 0x03, b'b', b'y', b'e']);
    }
    let mut packet = vec![0x10, body.len() as u8];
    packet.extend_from_slice(&body);

    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(&packet).await.unwrap();
    let connack = read(&mut stream, 5).await;
    assert_eq!(connack[3], 0x00, "connection refused");
    (stream, connack[2] == 0x01)
}

fn disconnect(reason: u8, session_expiry: Option<u32>) -> Vec<u8> {
    let mut body = vec![reason];
    match session_expiry {
        Some(expiry) => {
            body.extend_from_slice(&[0x05, 0x11]);
            body.extend_from_slice(&expiry.to_be_bytes());
        }
        None => body.push(0x00),
    }
    let mut packet = vec![0xe0, body.len() as u8];
    packet.extend_from_slice(&body);
    packet
}

async fn wait_for_close(stream: &mut TcpStream) {
    let _ = timeout(Duration::from_secs(2), stream.read(&mut [0u8; 1])).await;
}

#[tokio::test]
async fn disconnect_can_shorten_session_expiry() {
    let addr = start_broker().await;

    let (mut client, _) = connect(addr, b'a', true, 60, false).await;
    client.write_all(&disconnect(0x00, Some(0))).await.unwrap();
    wait_for_close(&mut client).await;

    let (_, resumed) = connect(addr, b'a', false, 60, false).await;
    assert!(!resumed);
}

#[tokio::test]
async fn disconnect_cannot_extend_zero_session_expiry() {
    let addr = start_broker().await;

    let (mut client, _) = connect(addr, b'b', true, 0, false).await;
    client.write_all(&disconnect(0x00, Some(60))).await.unwrap();

    // Protocol error
    assert_eq!(read(&mut client, 3).await, [0xe0, 0x01, 0x82]);
    wait_for_close(&mut client).await;

    let (_, resumed) = connect(addr, b'b', false, 0, false).await;
    assert!(!resumed);
}

#[tokio::test]
async fn disconnect_with_will_message_publishes_the_will() {
    let addr = start_broker().await;

    let (mut subscriber, _) = connect(addr, b's', true, 0, false).await;
    // SUBSCRIBE packet id 1, "w" at QoS 0
    subscriber
        .write_all(&[0x82, 0x07, 0x00, 0x01, 0x00, 0x00, 0x01, b'w', 0x00])
        .await
        .unwrap();
    read(&mut subscriber, 6).await;

    let (mut client, _) = connect(addr, b'c', true, 0, true).await;
    client.write_all(&disconnect(0x04, None)).await.unwrap();

    assert_eq!(
        read(&mut subscriber, 9).await,
        [0x30, 0x07, 0x00, 0x01, b'w', 0x00, b'b', b'y', b'e']
    );
}
