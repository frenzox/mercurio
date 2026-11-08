use std::time::Duration;

use mercurio_server::server::ServerConfig;
use tokio::{
    io::AsyncReadExt,
    net::{TcpListener, TcpStream},
    time::timeout,
};

#[tokio::test]
async fn connection_without_connect_is_closed() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut config = ServerConfig::default();
    config.limits.connect_timeout = Duration::from_millis(200);
    tokio::spawn(mercurio_server::server::run_with_config(
        listener,
        config,
        std::future::pending::<()>(),
    ));

    // Connect and never send CONNECT
    let mut stream = TcpStream::connect(addr).await.unwrap();

    let read = timeout(Duration::from_secs(2), stream.read(&mut [0u8; 1]))
        .await
        .expect("connection still open");
    assert!(matches!(read, Ok(0) | Err(_)));
}

/// The PROXY header and CONNECT share one deadline: a client can't take
/// the time allowed once for each step.
#[tokio::test]
async fn every_handshake_step_shares_one_deadline() {
    use mercurio_server::server::{ListenerConfig, Transport};
    use tokio::io::AsyncWriteExt;

    let proxied = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = proxied.local_addr().unwrap();
    let mut listener = ListenerConfig::new(proxied, Transport::Mqtt);
    listener.proxy_protocol = true;
    let mut config = ServerConfig::default();
    config.listeners.push(listener);
    config.limits.connect_timeout = Duration::from_millis(1000);
    tokio::spawn(mercurio_server::server::run_with_config(
        TcpListener::bind("127.0.0.1:0").await.unwrap(),
        config,
        std::future::pending::<()>(),
    ));

    let mut stream = TcpStream::connect(addr).await.unwrap();
    // Each step alone fits the timeout, both together don't
    tokio::time::sleep(Duration::from_millis(700)).await;
    stream
        .write_all(b"PROXY TCP4 192.0.2.1 127.0.0.1 56324 1883\r\n")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(700)).await;
    let _ = stream
        .write_all(&[
            0x10, 0x0e, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x05, 0x02, 0x00, 0x3c, 0x00, 0x00,
            0x01, b'p',
        ])
        .await;

    let read = timeout(Duration::from_secs(2), stream.read(&mut [0u8; 1]))
        .await
        .expect("connection still open");
    assert!(matches!(read, Ok(0) | Err(_)), "got a CONNACK");
}
