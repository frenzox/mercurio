mod common;

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

    let connack = common::read_connack(&mut stream).await;
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

/// MQTT 5 CONNECT asking for `expiry`; returns the CONNACK's Session
/// Expiry Interval property, if any.
async fn announced_expiry(addr: SocketAddr, expiry: u32) -> Option<u32> {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let mut packet = vec![0x00, 0x04, b'M', b'Q', b'T', b'T', 0x05, 0x00, 0x00, 0x3c];
    packet.extend_from_slice(&[0x05, 0x11]);
    packet.extend_from_slice(&expiry.to_be_bytes());
    packet.extend_from_slice(&[0x00, 0x01, b'x']);
    let mut connect = vec![0x10, packet.len() as u8];
    connect.extend_from_slice(&packet);
    stream.write_all(&connect).await.unwrap();

    let connack = common::read_connack(&mut stream).await;
    assert_eq!(connack[3], 0x00, "connection refused");
    // Properties start after the flags and reason code; the length is
    // one byte here
    let properties = &connack[5..];
    let at = properties.iter().position(|&b| b == 0x11)?;
    Some(u32::from_be_bytes(
        properties[at + 1..at + 5].try_into().unwrap(),
    ))
}

#[tokio::test]
async fn session_expiry_is_capped_and_announced() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut config = mercurio_server::server::ServerConfig::default();
    config.limits.max_session_expiry = Some(Duration::from_secs(60));
    tokio::spawn(mercurio_server::server::run_with_config(
        listener,
        config,
        std::future::pending::<()>(),
    ));

    // No expiry, and a longer one, are lowered to the maximum
    assert_eq!(announced_expiry(addr, u32::MAX).await, Some(60));
    assert_eq!(announced_expiry(addr, 3600).await, Some(60));
    // A shorter one stands, and isn't announced
    assert_eq!(announced_expiry(addr, 30).await, None);
}

/// Connects as "d" asking for `connect_expiry` and disconnects asking for
/// `disconnect_expiry`. Returns whether the session was resumed.
async fn reset_expiry_on_disconnect(
    addr: SocketAddr,
    clean: bool,
    connect_expiry: u32,
    disconnect_expiry: u32,
) -> bool {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let flags = if clean { 0x02 } else { 0x00 };
    let mut packet = vec![0x00, 0x04, b'M', b'Q', b'T', b'T', 0x05, flags, 0x00, 0x3c];
    packet.extend_from_slice(&[0x05, 0x11]);
    packet.extend_from_slice(&connect_expiry.to_be_bytes());
    packet.extend_from_slice(&[0x00, 0x01, b'd']);
    let mut connect = vec![0x10, packet.len() as u8];
    connect.extend_from_slice(&packet);
    stream.write_all(&connect).await.unwrap();
    let connack = common::read_connack(&mut stream).await;
    assert_eq!(connack[3], 0x00, "connection refused");

    // Normal disconnection, Session Expiry Interval
    let mut disconnect = vec![0xe0, 0x07, 0x00, 0x05, 0x11];
    disconnect.extend_from_slice(&disconnect_expiry.to_be_bytes());
    stream.write_all(&disconnect).await.unwrap();
    let _ = timeout(Duration::from_secs(2), stream.read(&mut [0u8; 1])).await;

    connack[2] == 0x01
}

#[tokio::test]
async fn session_expiry_set_on_disconnect_is_capped() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut config = mercurio_server::server::ServerConfig::default();
    config.limits.max_session_expiry = Some(Duration::from_secs(1));
    tokio::spawn(mercurio_server::server::run_with_config(
        listener,
        config,
        std::future::pending::<()>(),
    ));

    for asked in [u32::MAX, 3600] {
        assert!(!reset_expiry_on_disconnect(addr, true, 1, asked).await);
        sleep(Duration::from_millis(1500)).await;
        assert!(
            !reset_expiry_on_disconnect(addr, false, 0, 0).await,
            "session outlived the maximum when asked for {asked}"
        );
    }
}
