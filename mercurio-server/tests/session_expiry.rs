use std::{net::SocketAddr, time::Duration};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::{sleep, timeout},
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

/// Connects as client "c", disconnects cleanly and returns whether the
/// broker reported a resumed session.
async fn connect_and_disconnect(
    addr: SocketAddr,
    level: u8,
    clean: bool,
    session_expiry: Option<u32>,
) -> bool {
    let mut stream = TcpStream::connect(addr).await.unwrap();

    let mut packet = vec![0x00, 0x04, b'M', b'Q', b'T', b'T', level];
    packet.push(if clean { 0x02 } else { 0x00 });
    packet.extend_from_slice(&[0x00, 0x3c]);
    if level == 5 {
        match session_expiry {
            Some(expiry) => {
                packet.extend_from_slice(&[0x05, 0x11]);
                packet.extend_from_slice(&expiry.to_be_bytes());
            }
            None => packet.push(0x00),
        }
    }
    packet.extend_from_slice(&[0x00, 0x01, b'c']);
    let mut connect = vec![0x10, packet.len() as u8];
    connect.extend_from_slice(&packet);
    stream.write_all(&connect).await.unwrap();

    let mut connack = vec![0u8; if level == 5 { 5 } else { 4 }];
    timeout(Duration::from_secs(2), stream.read_exact(&mut connack))
        .await
        .expect("no CONNACK")
        .unwrap();
    assert_eq!(connack[3], 0x00, "connection refused");

    stream.write_all(&[0xe0, 0x00]).await.unwrap();
    // Wait for the broker to close the connection
    let _ = timeout(Duration::from_secs(2), stream.read(&mut [0u8; 1])).await;

    connack[2] == 0x01
}

#[tokio::test]
async fn mqtt5_session_ends_with_connection_by_default() {
    let addr = start_broker().await;

    assert!(!connect_and_disconnect(addr, 5, true, None).await);
    assert!(!connect_and_disconnect(addr, 5, false, None).await);
}

#[tokio::test]
async fn mqtt5_session_expires_after_interval() {
    let addr = start_broker().await;

    assert!(!connect_and_disconnect(addr, 5, true, Some(1)).await);
    assert!(connect_and_disconnect(addr, 5, false, Some(1)).await);

    sleep(Duration::from_millis(1500)).await;
    assert!(!connect_and_disconnect(addr, 5, false, Some(1)).await);
}

#[tokio::test]
async fn mqtt5_session_without_expiry_persists() {
    let addr = start_broker().await;

    assert!(!connect_and_disconnect(addr, 5, true, Some(u32::MAX)).await);
    assert!(connect_and_disconnect(addr, 5, false, Some(u32::MAX)).await);
}

#[tokio::test]
async fn mqtt311_clean_session_ends_with_connection() {
    let addr = start_broker().await;

    assert!(!connect_and_disconnect(addr, 4, true, None).await);
    assert!(!connect_and_disconnect(addr, 4, false, None).await);
    // Persistent sessions are kept
    assert!(connect_and_disconnect(addr, 4, false, None).await);
}
