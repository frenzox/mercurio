//! The same client id on listeners with different mount points: two
//! sessions, apart in every respect, also across a restart on durable
//! storage.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use bytes::{Bytes, BytesMut};
use mercurio_core::{
    properties::SessionExpiryInterval, protocol::ProtocolVersion, qos::QoS, reason::ReasonCode,
};
use mercurio_packets::{
    connect::{ConnectPacket, ConnectPayload, ConnectProperties, Will},
    disconnect::DisconnectPacket,
    puback::PubAckPacket,
    publish::PublishPacket,
    subscribe::{SubscribePacket, SubscribePayload, SubscriptionOptions},
    ControlPacket,
};
use mercurio_server::server::{ListenerConfig, Server, ServerConfig, Transport};
use mercurio_storage::{sqlite::SqliteStore, MqttStore};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinHandle,
    time::timeout,
};

/// A broker whose main listener has no mount point, with two more under
/// `a/` and `b/`.
struct Broker {
    main: SocketAddr,
    a: SocketAddr,
    b: SocketAddr,
    stop: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

impl Broker {
    async fn start<St: MqttStore + 'static>(storage: Arc<St>) -> Broker {
        let main = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = ServerConfig::default();
        config.sys_interval = None;
        let mut addrs = Vec::new();
        for mount_point in ["a/", "b/"] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            addrs.push(listener.local_addr().unwrap());
            let mut listener = ListenerConfig::new(listener, Transport::Mqtt);
            listener.mount_point = Some(mount_point.into());
            config.listeners.push(listener);
        }
        let main_addr = main.local_addr().unwrap();
        let server = Server::new(main, config, storage).await.unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            server
                .run(async {
                    let _ = stopped.await;
                })
                .await
                .unwrap();
        });
        Broker {
            main: main_addr,
            a: addrs[0],
            b: addrs[1],
            stop,
            task,
        }
    }

    /// Shuts down, storing everything first.
    async fn stop(self) {
        let _ = self.stop.send(());
        timeout(Duration::from_secs(10), self.task)
            .await
            .expect("the broker stops")
            .unwrap();
    }
}

struct Client {
    stream: TcpStream,
    buffer: BytesMut,
}

/// A CONNECT of `client_id` whose session lasts an hour, with a will on
/// `will`, which its listener's mount point prefixes.
fn connect_packet(client_id: &str, clean_start: bool) -> ConnectPacket {
    ConnectPacket {
        protocol_version: ProtocolVersion::V5,
        clean_start,
        keepalive: 60,
        properties: Some(ConnectProperties {
            session_expiry_interval: Some(SessionExpiryInterval::new(3600)),
            ..Default::default()
        }),
        payload: ConnectPayload {
            client_id: client_id.into(),
            will: Some(Will {
                properties: Some(Default::default()),
                topic: "will".into(),
                payload: Bytes::from(client_id.to_string()),
                ..Default::default()
            }),
            ..Default::default()
        },
    }
}

impl Client {
    /// Connects; returns the client and whether its session was resumed.
    async fn connect(addr: SocketAddr, client_id: &str, clean_start: bool) -> (Client, bool) {
        let mut client = Client {
            stream: TcpStream::connect(addr).await.unwrap(),
            buffer: BytesMut::new(),
        };
        client
            .send(ControlPacket::Connect(connect_packet(
                client_id,
                clean_start,
            )))
            .await;
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

    async fn subscribe(&mut self, filter: &str) {
        self.send(ControlPacket::Subscribe(SubscribePacket {
            packet_id: 1,
            properties: None,
            payload: vec![SubscribePayload {
                topic_filter: filter.into(),
                subs_opt: SubscriptionOptions {
                    qos: QoS::AtLeastOnce,
                    ..Default::default()
                },
            }],
        }))
        .await;
        let Some(ControlPacket::SubAck(_)) = self.receive().await else {
            panic!("Expected SUBACK");
        };
    }

    /// Publishes at QoS 1 and waits for the PUBACK.
    async fn publish(&mut self, topic: &str, payload: &str) {
        self.send(ControlPacket::Publish(PublishPacket {
            topic_name: topic.into(),
            payload: Some(Bytes::from(payload.to_string())),
            qos_level: QoS::AtLeastOnce,
            packet_id: Some(1),
            ..Default::default()
        }))
        .await;
        let Some(ControlPacket::PubAck(_)) = self.receive().await else {
            panic!("Expected PUBACK");
        };
    }

    /// The topic and payload of the next PUBLISH, and its packet id; None
    /// if nothing comes.
    async fn next_publish(&mut self) -> Option<(String, String, Option<u16>)> {
        match self.receive().await? {
            ControlPacket::Publish(publish) => Some((
                publish.topic_name,
                String::from_utf8(publish.payload.unwrap_or_default().to_vec()).unwrap(),
                publish.packet_id,
            )),
            other => panic!("Expected PUBLISH, got {other:?}"),
        }
    }

    async fn puback(&mut self, packet_id: u16) {
        self.send(ControlPacket::PubAck(PubAckPacket {
            packet_id,
            reason: ReasonCode::Success,
            properties: None,
        }))
        .await;
    }

    /// Disconnects normally, which drops the will.
    async fn disconnect(mut self) {
        self.send(ControlPacket::Disconnect(DisconnectPacket {
            reason: ReasonCode::NormalDisconnection,
            properties: None,
        }))
        .await;
        let _ = self.receive().await;
    }
}

fn message(topic: &str, payload: &str) -> Option<(String, String)> {
    Some((topic.to_string(), payload.to_string()))
}

/// Two clients with the same id on the mount points `a/` and `b/` keep
/// their own subscriptions, wills, takeovers, queues and in-flight
/// messages, the latter two across a restart on `storage`, which `reopen`
/// opens again.
async fn same_client_id_on_two_mount_points<St, F, Fut>(storage: Arc<St>, reopen: F)
where
    St: MqttStore + 'static,
    F: FnOnce(Arc<St>) -> Fut,
    Fut: std::future::Future<Output = Arc<St>>,
{
    let broker = Broker::start(Arc::clone(&storage)).await;
    let (mut watcher, _) = Client::connect(broker.main, "watcher", true).await;
    watcher.subscribe("#").await;
    let (mut on_a, resumed_a) = Client::connect(broker.a, "dev", true).await;
    let (mut on_b, resumed_b) = Client::connect(broker.b, "dev", true).await;
    assert!(!resumed_a && !resumed_b);
    on_a.subscribe("t").await;
    on_b.subscribe("t").await;

    // Each sees its own part of the tree only
    on_a.publish("t", "from a").await;
    let delivered = on_a.next_publish().await.map(|(t, p, _)| (t, p));
    assert_eq!(delivered, message("t", "from a"));
    on_a.puback(1).await;
    let watched = watcher.next_publish().await.map(|(t, p, _)| (t, p));
    assert_eq!(watched, message("a/t", "from a"));
    assert!(on_b.receive().await.is_none(), "b saw nothing of a");

    // A new connection on b takes over b's, and only b's
    let (on_b_again, resumed) = Client::connect(broker.b, "dev", false).await;
    assert!(resumed);
    let Some(ControlPacket::Disconnect(taken_over)) = on_b.receive().await else {
        panic!("Expected DISCONNECT");
    };
    assert_eq!(taken_over.reason, ReasonCode::SessionTakenOver);
    assert!(on_a.receive().await.is_none(), "a is left connected");

    // The connections taken over and lost each publish their own will,
    // under b's mount point, and a's stays
    drop(on_b_again.stream);
    let mut wills = Vec::new();
    while let Some((topic, payload, _)) = watcher.next_publish().await {
        wills.push((topic, payload));
    }
    let b_will = ("b/will".to_string(), "dev".to_string());
    assert_eq!(wills, [b_will.clone(), b_will]);
    on_a.disconnect().await;

    // Each session queues its own messages while away
    let (mut publisher, _) = Client::connect(broker.main, "publisher", true).await;
    publisher.publish("a/t", "queued for a").await;
    publisher.publish("b/t", "queued for b").await;

    // b receives its message and leaves it unacknowledged, in flight
    let (mut on_b, resumed) = Client::connect(broker.b, "dev", false).await;
    assert!(resumed);
    let (topic, payload, _) = on_b.next_publish().await.unwrap();
    assert_eq!((topic.as_str(), payload.as_str()), ("t", "queued for b"));
    on_b.disconnect().await;
    drop(watcher);
    broker.stop().await;

    let storage = reopen(storage).await;
    let broker = Broker::start(storage).await;
    let (mut on_a, resumed) = Client::connect(broker.a, "dev", false).await;
    assert!(resumed);
    let (topic, payload, packet_id) = on_a.next_publish().await.unwrap();
    assert_eq!((topic.as_str(), payload.as_str()), ("t", "queued for a"));
    on_a.puback(packet_id.unwrap()).await;
    assert!(on_a.receive().await.is_none());
    let (mut on_b, resumed) = Client::connect(broker.b, "dev", false).await;
    assert!(resumed);
    let (topic, payload, _) = on_b.next_publish().await.unwrap();
    assert_eq!((topic.as_str(), payload.as_str()), ("t", "queued for b"));
    assert!(on_b.receive().await.is_none(), "b got a's nothing");

    // Their subscriptions came back apart too
    let (mut publisher, _) = Client::connect(broker.main, "publisher", true).await;
    publisher.publish("b/t", "after").await;
    let delivered = on_b.next_publish().await.map(|(t, p, _)| (t, p));
    assert_eq!(delivered, message("t", "after"));
    assert!(on_a.receive().await.is_none());
    broker.stop().await;
}

fn database(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "mercurio-mount-points-{name}-{}.db",
        std::process::id()
    ));
    for suffix in ["", "-wal", "-shm", ".lock"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    path
}

#[tokio::test]
async fn same_client_id_on_two_mount_points_on_sqlite() {
    let path = database("sqlite");
    let storage = Arc::new(SqliteStore::new(&path).unwrap());
    // The stopped broker's tasks in this runtime still hold the database's
    // lock, so the new broker gets the same store, from which it reads
    // every session again
    same_client_id_on_two_mount_points(storage, |storage| async move { storage }).await;
    drop(database("sqlite"));
}

/// The database the PostgreSQL tests use, which they empty. They are
/// ignored unless `MERCURIO_TEST_POSTGRES_URL` was set when they were
/// built; see `build.rs`.
#[cfg(postgres_tests)]
async fn empty_postgres() -> Arc<mercurio_storage::postgres::PostgresStore> {
    use mercurio_storage::postgres::{ConnectOptions, PostgresStore};
    let url = std::env::var("MERCURIO_TEST_POSTGRES_URL")
        .expect("MERCURIO_TEST_POSTGRES_URL was set when the tests were built, but not now");
    let store = PostgresStore::connect(&url, &ConnectOptions::default())
        .await
        .unwrap();
    // Rows left without a session are under keys this test starts clean
    use mercurio_storage::{BatchStore, RetainedMessageStore};
    for key in store.list_sessions().await.unwrap() {
        store.delete_session(&key).await.unwrap();
    }
    store.clear_all_retained().await.unwrap();
    Arc::new(store)
}

#[tokio::test]
#[cfg_attr(not(postgres_tests), ignore = "set MERCURIO_TEST_POSTGRES_URL to run")]
async fn same_client_id_on_two_mount_points_on_postgres() {
    #[cfg(postgres_tests)]
    same_client_id_on_two_mount_points(empty_postgres().await, |storage| async move {
        // Another store on the same database, as a restarted broker opens
        drop(storage);
        use mercurio_storage::postgres::{ConnectOptions, PostgresStore};
        let url = std::env::var("MERCURIO_TEST_POSTGRES_URL").unwrap();
        Arc::new(
            PostgresStore::connect(&url, &ConnectOptions::default())
                .await
                .unwrap(),
        )
    })
    .await;
}
