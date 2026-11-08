mod common;

use std::time::Duration;

use mercurio_server::server::{ListenerConfig, ServerConfig, Transport};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

const CONNECT: [u8; 16] = [
    0x10, 0x0e, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x05, 0x02, 0x00, 0x3c, 0x00, 0x00, 0x01, b'p',
];

#[tokio::test]
async fn proxied_connections_start_with_a_header() {
    let proxied = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = proxied.local_addr().unwrap();
    let mut config = ServerConfig::default();
    config.listeners = vec![{
        let mut value = ListenerConfig::new(proxied, Transport::Mqtt);
        value.proxy_protocol = true;
        value
    }];
    tokio::spawn(mercurio_server::server::run_with_config(
        TcpListener::bind("127.0.0.1:0").await.unwrap(),
        config,
        std::future::pending::<()>(),
    ));

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
