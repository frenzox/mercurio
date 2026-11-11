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

/// A broker on a runtime of its own, as a process of its own would be.
/// Stopping it ends every task it left, so the database is free for the
/// next.
struct Broker {
    runtime: Option<tokio::runtime::Runtime>,
    stop: Option<oneshot::Sender<()>>,
}

impl Broker {
    async fn stop(mut self) {
        let _ = self.stop.take().unwrap().send(());
        sleep(Duration::from_millis(100)).await;
        self.runtime.take().unwrap().shutdown_background();
    }
}

impl Drop for Broker {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

async fn start_broker(db: &PathBuf) -> (SocketAddr, Broker) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    // The broker stopped before lets go of the database once its runtime
    // has dropped its tasks
    let mut attempts = 0;
    let storage = loop {
        match SqliteStore::new(db) {
            Ok(store) => break Arc::new(store),
            Err(_) if attempts < 50 => {
                attempts += 1;
                sleep(Duration::from_millis(50)).await;
            }
            Err(e) => panic!("{e}"),
        }
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let listener = listener.into_std().unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    runtime.spawn(async move {
        let listener = TcpListener::from_std(listener).unwrap();
        mercurio_server::server::run_with_storage(
            listener,
            ServerConfig::default(),
            storage,
            async {
                let _ = stopped.await;
            },
        )
        .await
    });
    (
        addr,
        Broker {
            runtime: Some(runtime),
            stop: Some(stop),
        },
    )
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
    stop.stop().await;

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

async fn next_publish(stream: &mut TcpStream, buffer: &mut bytes::BytesMut) -> Option<Vec<u8>> {
    use mercurio_core::protocol::ProtocolVersion;
    use mercurio_packets::ControlPacket;

    loop {
        if ControlPacket::check(buffer).is_ok() {
            let packet = ControlPacket::parse_with_version(buffer, ProtocolVersion::V5).unwrap();
            let ControlPacket::Publish(publish) = packet else {
                continue;
            };
            return Some(publish.payload.unwrap().to_vec());
        }
        match timeout(Duration::from_millis(500), stream.read_buf(buffer)).await {
            Ok(read) => assert_ne!(read.unwrap(), 0, "connection closed"),
            Err(_) => return None,
        }
    }
}

/// QoS 1 publish of `payload` to "q".
async fn publish(publisher: &mut TcpStream, id: u8, payload: u8) {
    publisher
        .write_all(&[0x32, 0x07, 0x00, 0x01, b'q', 0x00, id, 0x00, payload])
        .await
        .unwrap();
    let mut puback = [0u8; 4];
    timeout(Duration::from_secs(2), publisher.read_exact(&mut puback))
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn messages_for_offline_clients_survive_a_restart() {
    let db = std::env::temp_dir().join(format!("mercurio-queue-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&db);

    let (addr, stop) = start_broker(&db).await;
    let (mut subscriber, _) = connect(addr, b's', true).await;
    subscriber
        .write_all(&[0x82, 0x07, 0x00, 0x01, 0x00, 0x00, 0x01, b'q', 0x01])
        .await
        .unwrap();
    let mut suback = [0u8; 6];
    subscriber.read_exact(&mut suback).await.unwrap();
    drop(subscriber);
    sleep(Duration::from_millis(100)).await;

    let (mut publisher, _) = connect(addr, b'p', true).await;
    for (id, payload) in [(1, b'1'), (2, b'2'), (3, b'3')] {
        publish(&mut publisher, id, payload).await;
    }
    sleep(Duration::from_millis(200)).await;
    stop.stop().await;

    // After the restart the offline session still collects messages
    let (addr, _stop) = start_broker(&db).await;
    let (mut publisher, _) = connect(addr, b'p', true).await;
    publish(&mut publisher, 1, b'4').await;

    let (mut subscriber, resumed) = connect(addr, b's', false).await;
    assert!(resumed);
    let mut buffer = bytes::BytesMut::new();
    let mut received = Vec::new();
    while let Some(payload) = next_publish(&mut subscriber, &mut buffer).await {
        received.extend(payload);
    }
    assert_eq!(received, b"1234");

    let _ = std::fs::remove_file(&db);
}
