use std::time::Duration;

use mercurio_server::{
    server::{ListenerConfig, ServerConfig, Transport},
    tls::TlsConfig,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::{sleep, timeout},
};

/// Connections still in their PROXY, TLS or WebSocket handshake, or yet to
/// send CONNECT, end with the server instead of holding up its shutdown.
#[tokio::test]
async fn handshaking_connections_end_with_the_server() {
    let main = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut config = ServerConfig::default();
    config.limits.connect_timeout = Duration::from_secs(30);
    config.sys_interval = None;
    config.tls = Some(TlsConfig::new(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/cert.pem"),
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/key.pem"),
    ));
    // Each listener, with the start of its first step: nothing of the TLS
    // handshake, part of CONNECT, of the HTTP upgrade, of the PROXY header
    let mut started = vec![(main.local_addr().unwrap(), &b""[..])];
    for (transport, proxy_protocol, start) in [
        (Transport::Mqtt, false, &[0x10][..]),
        (Transport::WebSocket, false, b"GET / HTTP/1.1\r\n"),
        (Transport::Mqtt, true, b"PROXY TCP4"),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        started.push((listener.local_addr().unwrap(), start));
        let mut listener = ListenerConfig::new(listener, transport);
        listener.proxy_protocol = proxy_protocol;
        if proxy_protocol {
            listener.proxy_from = vec!["127.0.0.1".parse().unwrap()];
        }
        config.listeners.push(listener);
    }
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(mercurio_server::server::run_with_config(
        main,
        config,
        async {
            let _ = stopped.await;
        },
    ));

    let mut clients = Vec::new();
    for (addr, start) in started {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream.write_all(start).await.unwrap();
        clients.push(stream);
    }
    sleep(Duration::from_millis(100)).await;
    stop.send(()).unwrap();

    timeout(Duration::from_secs(2), server)
        .await
        .expect("shutdown waited for handshaking connections")
        .unwrap()
        .unwrap();
    for mut client in clients {
        let read = timeout(Duration::from_secs(1), client.read(&mut [0u8; 64]))
            .await
            .expect("connection still open");
        assert!(matches!(read, Ok(0) | Err(_)));
    }
}
