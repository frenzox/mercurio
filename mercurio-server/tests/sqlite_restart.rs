mod common;

use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use mercurio_server::{error::ServerError, server::ServerConfig};
use mercurio_storage::sqlite::SqliteStore;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinHandle,
    time::{sleep, timeout},
};

/// A broker serving from a database; once stopped, it has let go of it.
struct Broker {
    stop: oneshot::Sender<()>,
    task: JoinHandle<Result<(), ServerError>>,
}

impl Broker {
    async fn stop(self) {
        let _ = self.stop.send(());
        timeout(Duration::from_secs(15), self.task)
            .await
            .expect("the broker didn't stop")
            .unwrap()
            .unwrap();
    }
}

async fn start_broker(db: &PathBuf) -> (SocketAddr, Broker) {
    start_broker_with(db, ServerConfig::default()).await
}

async fn start_broker_with(db: &PathBuf, config: ServerConfig) -> (SocketAddr, Broker) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let storage = Arc::new(SqliteStore::new(db).unwrap());
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(
        common::broker(listener, config)
            .storage(storage)
            .run(async {
                let _ = stopped.await;
            }),
    );
    (addr, Broker { stop, task })
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
            let packet = ControlPacket::parse(buffer, ProtocolVersion::V5).unwrap();
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

/// Once `run` returns, the broker's tasks are gone, those holding its
/// storage included: an offline session's expiry timer and the writer of
/// its stored queue, the `$SYS` publisher, and a will waiting for its
/// delay. The same database opens again at once, on the same runtime.
#[tokio::test]
async fn a_stopped_broker_lets_go_of_its_database() {
    let db = std::env::temp_dir().join(format!("mercurio-reopen-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&db);
    let mut config = ServerConfig::default();
    config.sys_interval = Some(Duration::from_millis(100));
    let (addr, broker) = start_broker_with(&db, config).await;

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
    publish(&mut publisher, 1, b'1').await;

    // MQTT 5 CONNECT with a will on "w" delayed by an hour, in a session
    // that expires an hour after the connection
    let mut willing = TcpStream::connect(addr).await.unwrap();
    willing
        .write_all(&[
            0x10, 0x21, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x05, 0x06, 0x00, 0x3c, 0x05, 0x11,
            0x00, 0x00, 0x0e, 0x10, 0x00, 0x01, b'w', 0x05, 0x18, 0x00, 0x00, 0x0e, 0x10, 0x00,
            0x01, b'w', 0x00, 0x03, b'b', b'y', b'e',
        ])
        .await
        .unwrap();
    common::read_connack(&mut willing).await;
    drop(willing);
    sleep(Duration::from_millis(300)).await;

    broker.stop().await;
    SqliteStore::new(&db).expect("the stopped broker still holds the database");

    let _ = std::fs::remove_file(&db);
}
