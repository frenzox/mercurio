//! What a broker on a durable store keeps across a restart, including one
//! that stops at once, as a crash would.

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use bytes::{Bytes, BytesMut};
use mercurio_core::{
    properties::MessageExpiryInterval, protocol::ProtocolVersion, qos::QoS, reason::ReasonCode,
};
use mercurio_packets::{
    connect::{ConnectPacket, ConnectPayload, ConnectProperties},
    puback::PubAckPacket,
    publish::{PublishPacket, PublishProperties},
    pubrel::PubRelPacket,
    subscribe::{SubscribePacket, SubscribePayload, SubscriptionOptions},
    unsubscribe::{UnsubscribePacket, UnsubscribePayload},
    ControlPacket,
};
use mercurio_server::server::ServerConfig;
use mercurio_storage::sqlite::SqliteStore;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::oneshot,
    time::{sleep, timeout},
};

/// A database file of its own for each test.
fn database(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "mercurio-durability-{name}-{}.db",
        std::process::id()
    ));
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    path
}

/// A broker serving from a SQLite database in a runtime of its own, so
/// dropping the runtime stops every task the broker started, its storage
/// writers included.
struct Broker {
    addr: SocketAddr,
    runtime: tokio::runtime::Runtime,
    stop: oneshot::Sender<()>,
}

impl Broker {
    fn start(db: &Path) -> Self {
        Self::with_config(db, ServerConfig::default())
    }

    fn with_config(db: &Path, config: ServerConfig) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        // A crashed broker's runtime lets go of the database in the
        // background, and its lock with it
        let opening = std::time::Instant::now();
        let storage = loop {
            match SqliteStore::new(db) {
                Ok(storage) => break Arc::new(storage),
                Err(_) if opening.elapsed() < Duration::from_secs(10) => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => panic!("{e}"),
            }
        };
        let (stop, stopped) = oneshot::channel::<()>();
        let _entered = runtime.enter();
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        runtime.spawn(mercurio_server::server::run_with_storage(
            listener,
            config,
            storage,
            async {
                let _ = stopped.await;
            },
        ));
        Broker {
            addr,
            runtime,
            stop,
        }
    }

    /// Stops without a shutdown: what the broker hadn't stored is lost.
    /// Waits a moment first, so storage has caught up with what clients
    /// were told.
    async fn crash(self) {
        sleep(Duration::from_millis(300)).await;
        self.runtime.shutdown_background();
        // Lets SQLite work already handed to blocking threads finish
        sleep(Duration::from_millis(100)).await;
    }

    async fn stop(self) {
        let _ = self.stop.send(());
        sleep(Duration::from_millis(300)).await;
        self.runtime.shutdown_background();
        sleep(Duration::from_millis(100)).await;
    }
}

struct Client {
    stream: TcpStream,
    buffer: BytesMut,
}

/// How a test client connects.
#[derive(Clone, Copy)]
struct Connect {
    clean_start: bool,
    receive_maximum: u16,
    protocol_version: ProtocolVersion,
}

impl Default for Connect {
    fn default() -> Self {
        Connect {
            clean_start: true,
            receive_maximum: 100,
            protocol_version: ProtocolVersion::V5,
        }
    }
}

impl Client {
    /// Connects with a one hour session expiry; returns the client and
    /// whether its session was resumed.
    async fn connect(addr: SocketAddr, client_id: &str, options: Connect) -> (Self, bool) {
        let v5 = options.protocol_version == ProtocolVersion::V5;
        let connect = ConnectPacket {
            protocol_version: options.protocol_version,
            clean_start: options.clean_start,
            keepalive: 60,
            properties: v5.then(|| ConnectProperties {
                session_expiry_interval: Some(
                    mercurio_core::properties::SessionExpiryInterval::new(3600),
                ),
                receive_maximum: Some(mercurio_core::properties::ReceiveMaximum::new(
                    options.receive_maximum,
                )),
                ..Default::default()
            }),
            payload: ConnectPayload {
                client_id: client_id.into(),
                ..Default::default()
            },
        };
        let mut client = Client {
            stream: TcpStream::connect(addr).await.unwrap(),
            buffer: BytesMut::new(),
        };
        client.send(ControlPacket::Connect(connect)).await;
        let Some(ControlPacket::ConnAck(connack)) = client.receive().await else {
            panic!("Expected CONNACK");
        };
        assert_eq!(connack.reason_code, ReasonCode::Success);
        (client, connack.flags.session_present)
    }

    async fn send(&mut self, packet: ControlPacket) {
        let mut encoded = BytesMut::new();
        packet.encode(&mut encoded, ProtocolVersion::V5).unwrap();
        self.stream.write_all(&encoded).await.unwrap();
    }

    /// The next packet, or None once the broker sends nothing for a while
    /// or closes the connection.
    async fn receive(&mut self) -> Option<ControlPacket> {
        loop {
            if ControlPacket::check(&self.buffer).is_ok() {
                let packet = ControlPacket::parse(&mut self.buffer, ProtocolVersion::V5);
                return Some(packet.unwrap());
            }
            match timeout(
                Duration::from_millis(500),
                self.stream.read_buf(&mut self.buffer),
            )
            .await
            {
                Ok(Ok(read)) if read > 0 => {}
                _ => return None,
            }
        }
    }

    async fn subscribe(&mut self, filter: &str, qos: QoS) -> ReasonCode {
        self.send(ControlPacket::Subscribe(SubscribePacket {
            packet_id: 1,
            properties: None,
            payload: vec![SubscribePayload {
                topic_filter: filter.into(),
                subs_opt: SubscriptionOptions {
                    qos,
                    ..Default::default()
                },
            }],
        }))
        .await;
        let Some(ControlPacket::SubAck(suback)) = self.receive().await else {
            panic!("Expected SUBACK");
        };
        suback.payload[0].reason_code
    }

    /// Publishes `publish` at QoS 1 and waits for its PUBACK.
    async fn publish(&mut self, publish: PublishPacket) {
        self.send(ControlPacket::Publish(PublishPacket {
            qos_level: QoS::AtLeastOnce,
            packet_id: Some(1),
            ..publish
        }))
        .await;
        let Some(ControlPacket::PubAck(_)) = self.receive().await else {
            panic!("Expected PUBACK");
        };
    }

    async fn puback(&mut self, packet_id: u16) {
        self.send(ControlPacket::PubAck(PubAckPacket {
            packet_id,
            reason: ReasonCode::Success,
            properties: None,
        }))
        .await;
    }

    /// The next PUBLISH, skipping nothing else.
    async fn next_publish(&mut self) -> Option<PublishPacket> {
        match self.receive().await? {
            ControlPacket::Publish(publish) => Some(publish),
            other => panic!("Expected PUBLISH, got {other:?}"),
        }
    }
}

fn message(topic: &str, payload: &str) -> PublishPacket {
    PublishPacket {
        topic_name: topic.into(),
        payload: Some(Bytes::from(payload.to_string())),
        ..Default::default()
    }
}

/// A message sent but not acknowledged before the broker stopped is resent
/// with the RETAIN flag it was first sent with, unless it expired since.
#[tokio::test]
async fn in_flight_messages_keep_retain_and_expiry_across_a_crash() {
    let db = database("inflight");
    let broker = Broker::start(&db);
    let (mut publisher, _) = Client::connect(broker.addr, "p", Connect::default()).await;
    publisher
        .publish(PublishPacket {
            retain: true,
            ..message("r", "kept")
        })
        .await;

    let (mut subscriber, _) = Client::connect(broker.addr, "s", Connect::default()).await;
    subscriber.subscribe("r", QoS::AtLeastOnce).await;
    let retained = subscriber.next_publish().await.unwrap();
    assert!(retained.retain);
    subscriber.subscribe("e", QoS::AtLeastOnce).await;
    publisher
        .publish(PublishPacket {
            properties: Some(PublishProperties {
                message_expiry_interval: Some(MessageExpiryInterval::new(1)),
                ..Default::default()
            }),
            ..message("e", "expiring")
        })
        .await;
    let expiring = subscriber.next_publish().await.unwrap();
    assert_eq!(expiring.payload.as_deref(), Some(&b"expiring"[..]));
    broker.crash().await;
    sleep(Duration::from_millis(1100)).await;

    let broker = Broker::start(&db);
    let (mut subscriber, resumed) = Client::connect(
        broker.addr,
        "s",
        Connect {
            clean_start: false,
            ..Default::default()
        },
    )
    .await;
    assert!(resumed);
    let resent = subscriber.next_publish().await.unwrap();
    assert!(resent.dup);
    assert!(resent.retain);
    assert_eq!(resent.payload.as_deref(), Some(&b"kept"[..]));
    assert_eq!(resent.packet_id, retained.packet_id);
    assert!(subscriber.receive().await.is_none(), "the expired one too");
    broker.stop().await;
}

/// The broker remembers across a crash which of a client's QoS 2 messages
/// it took and awaits PUBREL for, so a copy the client sends again isn't
/// forwarded twice.
#[tokio::test]
async fn incoming_qos2_messages_are_forwarded_once_across_a_crash() {
    let db = database("incoming");
    let broker = Broker::start(&db);
    let (mut subscriber, _) = Client::connect(broker.addr, "s", Connect::default()).await;
    subscriber.subscribe("t", QoS::AtMostOnce).await;
    let (mut publisher, _) = Client::connect(broker.addr, "p", Connect::default()).await;
    let publish = PublishPacket {
        qos_level: QoS::ExactlyOnce,
        packet_id: Some(5),
        ..message("t", "once")
    };
    publisher
        .send(ControlPacket::Publish(publish.clone()))
        .await;
    assert!(matches!(
        publisher.receive().await,
        Some(ControlPacket::PubRec(ref r)) if r.packet_id == 5
    ));
    let delivered = subscriber.next_publish().await.unwrap();
    assert_eq!(delivered.payload.as_deref(), Some(&b"once"[..]));
    broker.crash().await;

    let broker = Broker::start(&db);
    let resume = Connect {
        clean_start: false,
        ..Default::default()
    };
    let (mut subscriber, _) = Client::connect(broker.addr, "s", resume).await;
    let (mut publisher, resumed) = Client::connect(broker.addr, "p", resume).await;
    assert!(resumed);
    publisher
        .send(ControlPacket::Publish(PublishPacket {
            dup: true,
            ..publish
        }))
        .await;
    assert!(matches!(
        publisher.receive().await,
        Some(ControlPacket::PubRec(ref r)) if r.packet_id == 5 && r.reason == ReasonCode::Success
    ));
    publisher
        .send(ControlPacket::PubRel(PubRelPacket {
            packet_id: 5,
            reason: ReasonCode::Success,
            properties: None,
        }))
        .await;
    assert!(matches!(
        publisher.receive().await,
        Some(ControlPacket::PubComp(ref c)) if c.packet_id == 5 && c.reason == ReasonCode::Success
    ));
    assert!(subscriber.receive().await.is_none(), "forwarded twice");
    broker.stop().await;
}

/// Subscriptions added and removed one at a time are all in storage after
/// a crash.
#[tokio::test]
async fn subscription_changes_survive_a_crash() {
    let db = database("subscriptions");
    let broker = Broker::start(&db);
    let (mut subscriber, _) = Client::connect(broker.addr, "s", Connect::default()).await;
    for filter in ["a", "b", "c"] {
        subscriber.subscribe(filter, QoS::AtMostOnce).await;
    }
    subscriber
        .send(ControlPacket::Unsubscribe(UnsubscribePacket {
            packet_id: 2,
            properties: None,
            payload: vec![UnsubscribePayload {
                topic_filter: "b".into(),
            }],
        }))
        .await;
    assert!(matches!(
        subscriber.receive().await,
        Some(ControlPacket::UnsubAck(_))
    ));
    broker.crash().await;

    let broker = Broker::start(&db);
    let (mut subscriber, resumed) = Client::connect(
        broker.addr,
        "s",
        Connect {
            clean_start: false,
            ..Default::default()
        },
    )
    .await;
    assert!(resumed);
    let (mut publisher, _) = Client::connect(broker.addr, "p", Connect::default()).await;
    for topic in ["a", "b", "c"] {
        publisher.publish(message(topic, topic)).await;
    }
    let mut received = Vec::new();
    while let Some(publish) = subscriber.next_publish().await {
        received.push(publish.topic_name);
    }
    assert_eq!(received, ["a", "c"]);
    broker.stop().await;
}

/// Payloads of the PUBLISH packets the client receives until the broker
/// pauses, acknowledging each, with whether it was flagged as a duplicate.
async fn drain(client: &mut Client) -> Vec<(String, bool)> {
    let mut received = Vec::new();
    while let Some(publish) = client.next_publish().await {
        if let Some(packet_id) = publish.packet_id {
            client.puback(packet_id).await;
        }
        let payload = publish.payload.unwrap_or_default();
        received.push((String::from_utf8(payload.to_vec()).unwrap(), publish.dup));
    }
    received
}

/// A client with a backlog of QoS 1 messages reconnects with a small
/// Receive Maximum, takes a few, and the broker crashes: after the restart
/// it gets every message it hadn't acknowledged, each once, in order.
#[tokio::test]
async fn queued_messages_of_connected_clients_survive_a_crash() {
    let db = database("queue");
    let broker = Broker::start(&db);
    let (mut subscriber, _) = Client::connect(broker.addr, "s", Connect::default()).await;
    subscriber.subscribe("q", QoS::AtLeastOnce).await;
    drop(subscriber);
    let (mut publisher, _) = Client::connect(broker.addr, "p", Connect::default()).await;
    for i in 1..=10 {
        publisher.publish(message("q", &i.to_string())).await;
    }

    let resume = Connect {
        clean_start: false,
        receive_maximum: 2,
        ..Default::default()
    };
    let (mut subscriber, resumed) = Client::connect(broker.addr, "s", resume).await;
    assert!(resumed);
    let first = subscriber.next_publish().await.unwrap();
    let second = subscriber.next_publish().await.unwrap();
    // Acknowledging the first lets the third through
    subscriber.puback(first.packet_id.unwrap()).await;
    let third = subscriber.next_publish().await.unwrap();
    assert_eq!(
        [&first, &second, &third].map(|p| p.payload.clone().unwrap()),
        [&b"1"[..], b"2", b"3"]
    );
    broker.crash().await;

    let broker = Broker::start(&db);
    let (mut subscriber, resumed) = Client::connect(
        broker.addr,
        "s",
        Connect {
            receive_maximum: 100,
            ..resume
        },
    )
    .await;
    assert!(resumed);
    let received = drain(&mut subscriber).await;
    let expected: Vec<(String, bool)> = (2..=10).map(|i| (i.to_string(), i <= 3)).collect();
    assert_eq!(received, expected);

    // Every one acknowledged, nothing is left for the next restart
    broker.crash().await;
    let broker = Broker::start(&db);
    let (mut subscriber, _) = Client::connect(broker.addr, "s", resume).await;
    assert!(drain(&mut subscriber).await.is_empty());
    broker.stop().await;
}

/// A queued message dropped rather than sent, here as too large for the
/// client, leaves storage too.
#[tokio::test]
async fn dropped_queued_messages_stay_dropped_after_a_crash() {
    let db = database("dropped");
    let broker = Broker::start(&db);
    let (mut subscriber, _) = Client::connect(broker.addr, "s", Connect::default()).await;
    subscriber.subscribe("q", QoS::AtLeastOnce).await;
    drop(subscriber);
    let (mut publisher, _) = Client::connect(broker.addr, "p", Connect::default()).await;
    publisher.publish(message("q", &"x".repeat(200))).await;
    publisher.publish(message("q", "small")).await;

    // Resumes with a Maximum Packet Size the first message exceeds
    let mut stream = TcpStream::connect(broker.addr).await.unwrap();
    let connect = ConnectPacket {
        protocol_version: ProtocolVersion::V5,
        clean_start: false,
        keepalive: 60,
        properties: Some(ConnectProperties {
            session_expiry_interval: Some(mercurio_core::properties::SessionExpiryInterval::new(
                3600,
            )),
            maximum_packet_size: Some(mercurio_core::properties::MaximumPacketSize::new(100)),
            ..Default::default()
        }),
        payload: ConnectPayload {
            client_id: "s".into(),
            ..Default::default()
        },
    };
    let mut encoded = BytesMut::new();
    ControlPacket::Connect(connect)
        .encode(&mut encoded, ProtocolVersion::V5)
        .unwrap();
    stream.write_all(&encoded).await.unwrap();
    let mut subscriber = Client {
        stream,
        buffer: BytesMut::new(),
    };
    assert!(matches!(
        subscriber.receive().await,
        Some(ControlPacket::ConnAck(_))
    ));
    let received = drain(&mut subscriber).await;
    assert_eq!(received, [("small".to_string(), false)]);
    broker.crash().await;

    let broker = Broker::start(&db);
    let (mut subscriber, _) = Client::connect(
        broker.addr,
        "s",
        Connect {
            clean_start: false,
            ..Default::default()
        },
    )
    .await;
    assert!(drain(&mut subscriber).await.is_empty());
    broker.stop().await;
}

/// Connects an MQTT 3.1.1 client asking to keep its session; returns the
/// connection and whether a session was resumed.
async fn connect_v311(addr: SocketAddr, client_id: u8) -> (TcpStream, bool) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(&[
            0x10, 0x0d, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x04, 0x00, 0x00, 0x3c, 0x00, 0x01,
            client_id,
        ])
        .await
        .unwrap();
    let mut connack = [0u8; 4];
    timeout(Duration::from_secs(2), stream.read_exact(&mut connack))
        .await
        .expect("no CONNACK")
        .unwrap();
    assert_eq!(connack[3], 0, "connection refused");
    (stream, connack[2] == 0x01)
}

/// The session of an MQTT 3.x client is stored with the expiry
/// `persistent_client_expiration` gives it from the start, so a crash while
/// the client is connected doesn't leave a session that never expires.
#[tokio::test]
async fn persistent_client_expiration_holds_across_a_crash() {
    let db = database("persistent-client-expiration");
    let mut config = ServerConfig::default();
    config.limits.persistent_client_expiration = Some(Duration::from_secs(1));
    let broker = Broker::with_config(&db, config);
    let (_client, _) = connect_v311(broker.addr, b'c').await;
    broker.crash().await;

    let broker = Broker::start(&db);
    sleep(Duration::from_millis(1500)).await;
    let (_client, resumed) = connect_v311(broker.addr, b'c').await;
    assert!(!resumed, "the session outlived its expiry");
    broker.stop().await;
}
