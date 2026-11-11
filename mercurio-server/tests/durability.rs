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
    codec::Encoder, properties::MessageExpiryInterval, protocol::ProtocolVersion, qos::QoS,
    reason::ReasonCode,
};
use mercurio_packets::{
    connect::{ConnectFlags, ConnectPacket, ConnectPayload, ConnectProperties},
    publish::{PublishPacket, PublishProperties},
    pubrel::PubRelPacket,
    subscribe::{SubscribePacket, SubscribePayload, SubscriptionOptions},
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
        let storage = Arc::new(SqliteStore::new(db).unwrap());
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
            flags: ConnectFlags {
                clean_start: options.clean_start,
                ..Default::default()
            },
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
        packet.encode(&mut encoded);
        self.stream.write_all(&encoded).await.unwrap();
    }

    /// The next packet, or None once the broker sends nothing for a while
    /// or closes the connection.
    async fn receive(&mut self) -> Option<ControlPacket> {
        loop {
            if ControlPacket::check(&self.buffer).is_ok() {
                let packet =
                    ControlPacket::parse_with_version(&mut self.buffer, ProtocolVersion::V5);
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
