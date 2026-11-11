use std::{
    io::Write,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use mercurio_server::{
    server::{ListenerConfig, ServerConfig, Transport},
    tls::{Tls, TlsConfig},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::{sleep, timeout},
};

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Sends `bytes` from a new connection, then waits for the broker to close
/// it. Returns the connection's address, as the broker sees it.
async fn send(addr: SocketAddr, bytes: &[u8]) -> SocketAddr {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let local = stream.local_addr().unwrap();
    let _ = stream.write_all(bytes).await;
    if bytes.is_empty() {
        return local;
    }
    let _ = timeout(Duration::from_secs(2), async {
        let mut sink = [0u8; 256];
        while matches!(stream.read(&mut sink).await, Ok(n) if n > 0) {}
    })
    .await;
    local
}

/// Health checks, scanners and broken clients can do all of these at will,
/// so none of them may log at `error`, and each says which peer it was.
#[tokio::test]
async fn client_caused_failures_are_not_errors() {
    let logs = Logs::default();
    let writer = logs.clone();
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .init();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let tls_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let tls_addr = tls_listener.local_addr().unwrap();
    let mut config = ServerConfig::default();
    let mut tls = ListenerConfig::new(tls_listener, Transport::Mqtt);
    tls.tls = Some(
        Tls::new(TlsConfig::new(
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/cert.pem"),
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/key.pem"),
        ))
        .unwrap(),
    );
    config.listeners.push(tls);
    tokio::spawn(mercurio_server::server::run_with_config(
        listener,
        config,
        std::future::pending::<()>(),
    ));

    let mut peers = Vec::new();
    // A TCP health check: connect, then close before CONNECT
    peers.push(("health check", send(addr, b"").await));
    // Not TLS on a TLS listener
    peers.push((
        "TLS failure",
        send(tls_addr, b"GET / HTTP/1.1\r\n\r\n").await,
    ));
    // A first packet other than CONNECT
    peers.push(("no CONNECT", send(addr, &[0xc0, 0x00]).await));
    // An MQTT 3.1.1 CONNECT larger than allowed before authentication
    let mut large = vec![0x10, 0x80, 0x80, 0x08, 0x00, 0x04];
    large.extend_from_slice(b"MQTT\x04\x02\x00\x3c");
    peers.push(("oversized CONNECT", send(addr, &large).await));
    // A malformed packet once connected
    let mut connect = vec![
        0x10, 0x0e, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x05, 0x02, 0x00, 0x3c, 0x00, 0x00, 0x01,
        b'c',
    ];
    connect.extend_from_slice(&[0x00, 0x00]);
    peers.push(("protocol error", send(addr, &connect).await));
    sleep(Duration::from_millis(300)).await;

    let logs = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    for line in logs.lines() {
        assert!(!line.contains("ERROR"), "{line}\n\n{logs}");
    }
    for (event, peer) in peers {
        let peer = peer.to_string();
        assert!(
            logs.lines().any(|line| line.contains(&peer)
                && line.contains("mercurio::connections")
                && !line.contains("Got a connection")),
            "{event}: nothing logged for {peer}\n\n{logs}"
        );
    }
}
