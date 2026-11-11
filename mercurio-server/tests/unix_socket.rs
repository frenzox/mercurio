#![cfg(unix)]

mod common;

use std::time::Duration;

use mercurio_server::server::{ListenerConfig, ServerConfig, Transport};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, UnixListener, UnixStream},
    time::timeout,
};

#[tokio::test]
async fn clients_connect_over_unix_sockets() {
    let path = std::env::temp_dir().join(format!("mercurio-test-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let config = ServerConfig::default();
    let listeners = vec![ListenerConfig::new(
        UnixListener::bind(&path).unwrap(),
        Transport::Mqtt,
    )];
    tokio::spawn(
        common::broker(TcpListener::bind("127.0.0.1:0").await.unwrap(), config)
            .listeners(listeners)
            .run(std::future::pending::<()>()),
    );

    let mut stream = UnixStream::connect(&path).await.unwrap();
    stream
        .write_all(&[
            0x10, 0x0e, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x05, 0x02, 0x00, 0x3c, 0x00, 0x00,
            0x01, b'u',
        ])
        .await
        .unwrap();
    let mut connack = [0u8; 4];
    timeout(Duration::from_secs(2), stream.read_exact(&mut connack))
        .await
        .expect("no CONNACK")
        .unwrap();
    assert_eq!(connack[0], 0x20, "expected CONNACK");
    assert_eq!(connack[3], 0x00);
    let _ = std::fs::remove_file(&path);
}
