use std::{sync::Arc, time::Duration};

use mercurio_server::{
    plugin::{ready, Access, BoxFuture, ClientInfo, Decision, Plugin},
    server::{LocalConnector, ServerConfig},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

/// MQTT 5 CONNECT without credentials
const CONNECT: [u8; 16] = [
    0x10, 0x0e, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x05, 0x02, 0x00, 0x3c, 0x00, 0x00, 0x01, b'l',
];

async fn connack_reason<S: AsyncReadExt + AsyncWriteExt + Unpin>(stream: &mut S) -> u8 {
    stream.write_all(&CONNECT).await.unwrap();
    let mut connack = vec![0u8; 2];
    timeout(Duration::from_secs(2), stream.read_exact(&mut connack))
        .await
        .expect("no CONNACK")
        .unwrap();
    assert_eq!(connack[0], 0x20, "expected CONNACK");
    connack.resize(2 + connack[1] as usize, 0);
    stream.read_exact(&mut connack[2..]).await.unwrap();
    connack[3]
}

#[tokio::test]
async fn local_connections_skip_authentication() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (connector, local_listener) = LocalConnector::new();
    let mut config = ServerConfig::default();
    config.auth.require_auth = true;
    config.auth.allow_anonymous = false;
    config.local_listener = Some(local_listener);
    tokio::spawn(mercurio_server::server::run_with_config(
        listener,
        config,
        std::future::pending::<()>(),
    ));

    let mut tcp = TcpStream::connect(addr).await.unwrap();
    assert_ne!(connack_reason(&mut tcp).await, 0x00);

    let mut local = connector.connect().unwrap();
    assert_eq!(connack_reason(&mut local).await, 0x00);
}

/// An in-process bridge is part of the broker's own setup, so a broker
/// full of network clients still serves it, and it takes no network
/// client's place.
#[tokio::test]
async fn local_connections_skip_the_connection_limit() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (connector, local_listener) = LocalConnector::new();
    let mut config = ServerConfig::default();
    config.limits.max_connections = Some(1);
    config.local_listener = Some(local_listener);
    tokio::spawn(mercurio_server::server::run_with_config(
        listener,
        config,
        std::future::pending::<()>(),
    ));

    let mut local = connector.connect().unwrap();
    assert_eq!(connack_reason(&mut local).await, 0x00);
    let mut tcp = TcpStream::connect(addr).await.unwrap();
    let mut connect = CONNECT;
    connect[15] = b't';
    tcp.write_all(&connect).await.unwrap();
    let mut first = [0u8; 1];
    timeout(Duration::from_secs(2), tcp.read_exact(&mut first))
        .await
        .expect("no CONNACK")
        .expect("network client refused");
    assert_eq!(first[0], 0x20);

    let mut second = connector.connect().unwrap();
    let mut connect = CONNECT;
    connect[15] = b'm';
    second.write_all(&connect).await.unwrap();
    timeout(Duration::from_secs(2), second.read_exact(&mut first))
        .await
        .expect("no CONNACK")
        .expect("in-process client refused");
    assert_eq!(first[0], 0x20);
}

#[tokio::test]
async fn control_topics_need_an_explicit_allow() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (connector, local_listener) = LocalConnector::new();
    let mut config = ServerConfig::default();
    config.local_listener = Some(local_listener);
    tokio::spawn(mercurio_server::server::run_with_config(
        listener,
        config,
        std::future::pending::<()>(),
    ));

    // Not Authorized for a client no plugin allows
    let tcp = TcpStream::connect(addr).await.unwrap();
    assert_eq!(control_puback_reason(tcp).await, 0x87);
    // but trusted local connections may: No Matching Subscribers
    let local = connector.connect().unwrap();
    assert_eq!(control_puback_reason(local).await, 0x10);
}

#[tokio::test]
async fn control_topics_open_to_clients_a_plugin_allows() {
    struct AllowControl;
    impl Plugin for AllowControl {
        fn name(&self) -> &str {
            "allow-control"
        }
        fn authorize<'a>(
            &'a self,
            _: &'a ClientInfo,
            topic: &'a str,
            _: Access,
        ) -> BoxFuture<'a, Decision> {
            ready(if topic.starts_with("$CONTROL/") {
                Decision::Allow
            } else {
                Decision::Defer
            })
        }
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut config = ServerConfig::default();
    config.plugins = vec![Arc::new(AllowControl)];
    tokio::spawn(mercurio_server::server::run_with_config(
        listener,
        config,
        std::future::pending::<()>(),
    ));

    // No Matching Subscribers: accepted
    let tcp = TcpStream::connect(addr).await.unwrap();
    assert_eq!(control_puback_reason(tcp).await, 0x10);
}

/// Connects on `stream` and publishes to `$CONTROL/x` at QoS 1, returning the
/// PUBACK reason code.
async fn control_puback_reason<S: AsyncReadExt + AsyncWriteExt + Unpin>(mut stream: S) -> u8 {
    assert_eq!(connack_reason(&mut stream).await, 0x00);
    let mut publish = vec![0x32, 0x0f, 0x00, 0x0a];
    publish.extend_from_slice(b"$CONTROL/x");
    publish.extend_from_slice(&[0x00, 0x01, 0x00]);
    stream.write_all(&publish).await.unwrap();
    let mut puback = [0u8; 5];
    timeout(Duration::from_secs(2), stream.read_exact(&mut puback))
        .await
        .expect("no PUBACK")
        .unwrap();
    assert_eq!(puback[0], 0x40, "expected PUBACK");
    puback[4]
}

/// In-process clients don't reconnect, so a network client taking their
/// client id over would stop them for good.
#[tokio::test]
async fn network_clients_cant_take_an_in_process_client_id() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (connector, local_listener) = LocalConnector::new();
    let mut config = ServerConfig::default();
    config.local_listener = Some(local_listener);
    tokio::spawn(mercurio_server::server::run_with_config(
        listener,
        config,
        std::future::pending::<()>(),
    ));

    let mut local = connector.connect().unwrap();
    assert_eq!(connack_reason(&mut local).await, 0x00);

    // Same client id, `l`: Not authorized
    let mut tcp = TcpStream::connect(addr).await.unwrap();
    assert_eq!(connack_reason(&mut tcp).await, 0x87);

    // The in-process client is still connected: it answers a PINGREQ
    local.write_all(&[0xc0, 0x00]).await.unwrap();
    let mut pingresp = [0u8; 2];
    timeout(Duration::from_secs(2), local.read_exact(&mut pingresp))
        .await
        .expect("in-process client was disconnected")
        .unwrap();
    assert_eq!(pingresp, [0xd0, 0x00]);

    // Once it leaves, the id is free again
    drop(local);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut tcp = TcpStream::connect(addr).await.unwrap();
    assert_eq!(connack_reason(&mut tcp).await, 0x00);
}
