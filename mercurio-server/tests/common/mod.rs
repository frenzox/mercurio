use std::time::Duration;

use mercurio_server::server::{ListenerConfig, Server, ServerBuilder, ServerConfig, Transport};
use tokio::{
    io::AsyncReadExt,
    net::{TcpListener, TcpStream},
    time::timeout,
};

/// A broker with `config`, serving `listener` as plain MQTT, ready to
/// take other listeners and storage.
#[allow(dead_code)]
pub fn broker(listener: TcpListener, config: ServerConfig) -> ServerBuilder {
    Server::builder()
        .config(config)
        .listener(ListenerConfig::new(listener, Transport::Mqtt))
}

/// Reads a whole CONNACK, whatever properties it carries.
#[allow(dead_code)]
pub async fn read_connack(stream: &mut TcpStream) -> Vec<u8> {
    timeout(Duration::from_secs(2), async {
        let mut connack = vec![0u8; 2];
        stream.read_exact(&mut connack).await.unwrap();
        assert_eq!(connack[0], 0x20, "expected CONNACK");
        connack.resize(2 + connack[1] as usize, 0);
        stream.read_exact(&mut connack[2..]).await.unwrap();
        connack
    })
    .await
    .expect("no CONNACK")
}

/// The reason code of the DISCONNECT that `bytes` start with.
#[allow(dead_code)]
pub fn disconnect_reason(bytes: &[u8]) -> u8 {
    assert_eq!(
        bytes.first(),
        Some(&0xe0),
        "expected DISCONNECT: {bytes:02x?}"
    );
    bytes[2]
}

/// Reads a whole DISCONNECT, whatever properties it carries.
#[allow(dead_code)]
pub async fn read_disconnect(stream: &mut TcpStream) -> Vec<u8> {
    timeout(Duration::from_secs(2), async {
        let mut disconnect = vec![0u8; 2];
        stream.read_exact(&mut disconnect).await.unwrap();
        disconnect.resize(2 + disconnect[1] as usize, 0);
        stream.read_exact(&mut disconnect[2..]).await.unwrap();
        disconnect
    })
    .await
    .expect("no DISCONNECT")
}
