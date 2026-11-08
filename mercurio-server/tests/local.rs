use std::time::Duration;

use mercurio_server::server::{AuthConfig, LocalConnector, ServerConfig};
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
    let config = ServerConfig {
        auth: AuthConfig {
            require_auth: true,
            allow_anonymous: false,
            ..Default::default()
        },
        local_listener: Some(local_listener),
        ..Default::default()
    };
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

#[tokio::test]
async fn control_topics_need_an_authorizer() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (connector, local_listener) = LocalConnector::new();
    let config = ServerConfig {
        local_listener: Some(local_listener),
        ..Default::default()
    };
    tokio::spawn(mercurio_server::server::run_with_config(
        listener,
        config,
        std::future::pending::<()>(),
    ));

    // Not Authorized for a client of a broker without access control
    let tcp = TcpStream::connect(addr).await.unwrap();
    assert_eq!(control_puback_reason(tcp).await, 0x87);
    // but trusted local connections may: No Matching Subscribers
    let local = connector.connect().unwrap();
    assert_eq!(control_puback_reason(local).await, 0x10);
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
    let config = ServerConfig {
        local_listener: Some(local_listener),
        ..Default::default()
    };
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
