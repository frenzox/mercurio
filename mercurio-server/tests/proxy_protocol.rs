mod common;

use std::time::Duration;

use mercurio_server::server::{ListenerConfig, ServerConfig, Transport};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

/// Every peer may send the header
const ANY: &[&str] = &["0.0.0.0/0", "::/0"];

const CONNECT: [u8; 16] = [
    0x10, 0x0e, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x05, 0x02, 0x00, 0x3c, 0x00, 0x00, 0x01, b'p',
];

#[tokio::test]
async fn proxied_connections_start_with_a_header() {
    let proxied = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = proxied.local_addr().unwrap();
    let config = ServerConfig::default();
    let listeners = vec![{
        let mut value = ListenerConfig::new(proxied, Transport::Mqtt);
        value.proxy_protocol = true;
        value.proxy_from = vec!["127.0.0.1".parse().unwrap()];
        value
    }];
    tokio::spawn(
        common::broker(TcpListener::bind("127.0.0.1:0").await.unwrap(), config)
            .listeners(listeners)
            .run(std::future::pending::<()>()),
    );

    let mut stream = TcpStream::connect(addr).await.unwrap();
    let mut bytes = b"PROXY TCP4 192.0.2.1 127.0.0.1 56324 1883\r\n".to_vec();
    bytes.extend_from_slice(&CONNECT);
    stream.write_all(&bytes).await.unwrap();
    assert_eq!(common::read_connack(&mut stream).await[3], 0x00);

    // Without the header, the connection is closed unanswered; with the
    // CONNECT left unread, the close may arrive as a reset
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(&CONNECT).await.unwrap();
    let mut received = Vec::new();
    let read = timeout(Duration::from_secs(2), stream.read_to_end(&mut received))
        .await
        .expect("connection still open");
    if let Err(e) = read {
        assert_eq!(e.kind(), std::io::ErrorKind::ConnectionReset);
    }
    assert!(received.is_empty(), "{received:02x?}");
}

/// Whether a CONNECT sent after `header`, if any, is answered.
async fn answered(addr: std::net::SocketAddr, header: Option<&str>) -> bool {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    if let Some(header) = header {
        let _ = stream.write_all(header.as_bytes()).await;
    }
    let _ = stream.write_all(&CONNECT).await;
    let mut first = [0u8; 1];
    matches!(
        timeout(Duration::from_secs(2), stream.read(&mut first)).await,
        Ok(Ok(1)) if first[0] == 0x20
    )
}

async fn start_proxied(proxy_from: &[&str], max_per_ip: Option<usize>) -> std::net::SocketAddr {
    let proxied = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = proxied.local_addr().unwrap();
    let mut listener = ListenerConfig::new(proxied, Transport::Mqtt);
    listener.proxy_protocol = true;
    listener.proxy_from = proxy_from.iter().map(|r| r.parse().unwrap()).collect();
    let mut config = ServerConfig::default();
    config.limits.max_connections_per_ip = max_per_ip;
    tokio::spawn(
        common::broker(TcpListener::bind("127.0.0.1:0").await.unwrap(), config)
            .listener(listener)
            .run(std::future::pending::<()>()),
    );
    addr
}

#[tokio::test]
async fn only_trusted_proxies_may_send_the_header() {
    let header = "PROXY TCP4 192.0.2.1 127.0.0.1 56324 1883\r\n";
    let untrusted = start_proxied(&["10.0.0.0/8"], None).await;
    assert!(!answered(untrusted, Some(header)).await);
    let trusted = start_proxied(&["127.0.0.0/8"], None).await;
    assert!(answered(trusted, Some(header)).await);
    let any = start_proxied(ANY, None).await;
    assert!(answered(any, Some(header)).await);
}

/// Version 2 knows only the LOCAL and PROXY commands; a connection with
/// another can't be trusted to carry what it claims.
#[tokio::test]
async fn unknown_v2_commands_are_refused() {
    let addr = start_proxied(ANY, None).await;
    let mut header = b"\r\n\r\n\0\r\nQUIT\n".to_vec();
    header.extend_from_slice(&[0x22, 0x11, 0x00, 0x0c]);
    header.extend_from_slice(&[192, 0, 2, 1, 127, 0, 0, 1, 0xdc, 0x04, 0x07, 0x5b]);
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(&header).await.unwrap();
    stream.write_all(&CONNECT).await.unwrap();
    let mut first = [0u8; 1];
    let read = timeout(Duration::from_secs(2), stream.read(&mut first))
        .await
        .expect("connection still open");
    assert!(matches!(read, Ok(0) | Err(_)), "answered: {first:02x?}");
}

/// Behind a proxy, connections are counted by the address the header
/// gives, not the proxy's.
#[tokio::test]
async fn connections_are_limited_per_client_address() {
    let addr = start_proxied(ANY, Some(1)).await;
    let from = |ip: &str| format!("PROXY TCP4 {ip} 127.0.0.1 56324 1883\r\n");

    let mut held = TcpStream::connect(addr).await.unwrap();
    held.write_all(from("192.0.2.1").as_bytes()).await.unwrap();
    held.write_all(&CONNECT).await.unwrap();
    let mut first = [0u8; 1];
    timeout(Duration::from_secs(2), held.read_exact(&mut first))
        .await
        .unwrap()
        .unwrap();

    // The same client address again: closed
    assert!(!answered(addr, Some(&from("192.0.2.1"))).await);
    // Another address through the same proxy: served
    assert!(answered(addr, Some(&from("192.0.2.2"))).await);
}
