use std::{net::SocketAddr, time::Duration};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::{sleep, timeout},
};

const WILL_PUBLISH: [u8; 9] = [0x30, 0x07, 0x00, 0x01, b'w', 0x00, b'b', b'y', b'e'];

async fn start_broker() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(mercurio_server::server::run(
        listener,
        std::future::pending::<()>(),
    ));
    addr
}

async fn read_within(stream: &mut TcpStream, len: usize, wait: Duration) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; len];
    match timeout(wait, stream.read_exact(&mut buf)).await {
        Ok(result) => {
            result.unwrap();
            Some(buf)
        }
        Err(_) => None,
    }
}

/// MQTT 5 CONNECT, optionally with a will on "w" carrying "bye".
async fn connect(
    addr: SocketAddr,
    client_id: u8,
    clean: bool,
    session_expiry: u32,
    will_delay: Option<u32>,
) -> TcpStream {
    let mut flags = if clean { 0x02 } else { 0x00 };
    if will_delay.is_some() {
        flags |= 0x04;
    }
    let mut body = vec![0x00, 0x04, b'M', b'Q', b'T', b'T', 0x05, flags, 0x00, 0x3c];
    body.extend_from_slice(&[0x05, 0x11]);
    body.extend_from_slice(&session_expiry.to_be_bytes());
    body.extend_from_slice(&[0x00, 0x01, client_id]);
    if let Some(delay) = will_delay {
        body.extend_from_slice(&[0x05, 0x18]);
        body.extend_from_slice(&delay.to_be_bytes());
        body.extend_from_slice(&[0x00, 0x01, b'w', 0x00, 0x03, b'b', b'y', b'e']);
    }
    let mut packet = vec![0x10, body.len() as u8];
    packet.extend_from_slice(&body);

    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(&packet).await.unwrap();
    let connack = read_within(&mut stream, 5, Duration::from_secs(2))
        .await
        .expect("no CONNACK");
    assert_eq!(connack[3], 0x00, "connection refused");
    stream
}

async fn will_subscriber(addr: SocketAddr) -> TcpStream {
    let mut subscriber = connect(addr, b's', true, 0, None).await;
    // SUBSCRIBE packet id 1, "w" at QoS 0
    subscriber
        .write_all(&[0x82, 0x06, 0x00, 0x01, 0x00, 0x00, 0x01, b'w', 0x00])
        .await
        .unwrap();
    read_within(&mut subscriber, 6, Duration::from_secs(2))
        .await
        .expect("no SUBACK");
    subscriber
}

#[tokio::test]
async fn will_is_published_after_its_delay() {
    let addr = start_broker().await;
    let mut subscriber = will_subscriber(addr).await;

    drop(connect(addr, b'c', true, 60, Some(1)).await);

    assert!(read_within(&mut subscriber, 9, Duration::from_millis(500))
        .await
        .is_none());
    assert_eq!(
        read_within(&mut subscriber, 9, Duration::from_secs(2)).await,
        Some(WILL_PUBLISH.to_vec())
    );
}

#[tokio::test]
async fn reconnecting_within_the_delay_cancels_the_will() {
    let addr = start_broker().await;
    let mut subscriber = will_subscriber(addr).await;

    drop(connect(addr, b'c', true, 60, Some(1)).await);
    sleep(Duration::from_millis(200)).await;
    let _reconnected = connect(addr, b'c', false, 60, None).await;

    assert!(read_within(&mut subscriber, 9, Duration::from_millis(1500))
        .await
        .is_none());
}

#[tokio::test]
async fn will_is_published_when_session_ends_before_delay() {
    let addr = start_broker().await;
    let mut subscriber = will_subscriber(addr).await;

    drop(connect(addr, b'c', true, 0, Some(60)).await);

    assert_eq!(
        read_within(&mut subscriber, 9, Duration::from_secs(1)).await,
        Some(WILL_PUBLISH.to_vec())
    );
}
