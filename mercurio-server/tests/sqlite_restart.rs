mod common;

use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use mercurio_server::server::ServerConfig;
use mercurio_storage::sqlite::SqliteStore;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    time::{sleep, timeout},
};

async fn start_broker(db: &PathBuf) -> (SocketAddr, oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let storage = Arc::new(SqliteStore::new(db).unwrap());
    let (stop, stopped) = oneshot::channel::<()>();
    tokio::spawn(mercurio_server::server::run_with_storage(
        listener,
        ServerConfig::default(),
        storage,
        async {
            let _ = stopped.await;
        },
    ));
    (addr, stop)
}

/// MQTT 5 CONNECT with a one hour session expiry; returns whether the
/// session was resumed.
async fn connect(addr: SocketAddr, client_id: u8, clean: bool) -> (TcpStream, bool) {
    let flags = if clean { 0x02 } else { 0x00 };
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(&[
            0x10, 0x13, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x05, flags, 0x00, 0x3c, 0x05, 0x11,
            0x00, 0x00, 0x0e, 0x10, 0x00, 0x01, client_id,
        ])
        .await
        .unwrap();
    let connack = common::read_connack(&mut stream).await;
    (stream, connack[2] == 0x01)
}

#[tokio::test]
async fn sessions_and_retained_messages_survive_a_restart() {
    let db = std::env::temp_dir().join(format!("mercurio-restart-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&db);

    let (addr, stop) = start_broker(&db).await;
    let (mut subscriber, _) = connect(addr, b's', true).await;
    subscriber
        .write_all(&[0x82, 0x07, 0x00, 0x01, 0x00, 0x00, 0x01, b't', 0x01])
        .await
        .unwrap();
    let mut suback = [0u8; 6];
    subscriber.read_exact(&mut suback).await.unwrap();
    let (mut publisher, _) = connect(addr, b'p', true).await;
    // Retained QoS 0 "kept" on "r"
    publisher
        .write_all(&[0x31, 0x08, 0x00, 0x01, b'r', 0x00, b'k', b'e', b'p', b't'])
        .await
        .unwrap();
    sleep(Duration::from_millis(100)).await;
    drop((subscriber, publisher));
    stop.send(()).unwrap();
    sleep(Duration::from_millis(100)).await;

    let (addr, _stop) = start_broker(&db).await;
    let (mut subscriber, resumed) = connect(addr, b's', false).await;
    assert!(resumed);

    let (mut publisher, _) = connect(addr, b'p', true).await;
    // QoS 0 "hi" on "t"
    publisher
        .write_all(&[0x30, 0x06, 0x00, 0x01, b't', 0x00, b'h', b'i'])
        .await
        .unwrap();
    let mut publish = [0u8; 8];
    timeout(Duration::from_secs(2), subscriber.read_exact(&mut publish))
        .await
        .expect("restored subscription got nothing")
        .unwrap();
    assert_eq!(&publish[6..], b"hi");

    // The retained message is still there for new subscribers
    let (mut late, _) = connect(addr, b'l', true).await;
    late.write_all(&[0x82, 0x07, 0x00, 0x01, 0x00, 0x00, 0x01, b'r', 0x00])
        .await
        .unwrap();
    // SUBACK, then the retained PUBLISH
    let mut received = [0u8; 16];
    timeout(Duration::from_secs(2), late.read_exact(&mut received))
        .await
        .expect("no retained message")
        .unwrap();
    assert!(received.ends_with(b"kept"));

    let _ = std::fs::remove_file(&db);
}
