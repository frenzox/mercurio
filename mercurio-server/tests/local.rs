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
    let mut connack = [0u8; 4];
    timeout(Duration::from_secs(2), stream.read_exact(&mut connack))
        .await
        .expect("no CONNACK")
        .unwrap();
    assert_eq!(connack[0], 0x20, "expected CONNACK");
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
