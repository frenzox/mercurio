mod common;

use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};

use bytes::{Bytes, BytesMut};
use mercurio_core::{
    codec::Encoder, properties::SessionExpiryInterval, protocol::ProtocolVersion, qos::QoS,
    reason::ReasonCode,
};
use mercurio_packets::{
    connack::ConnAckPacket,
    connect::{ConnectPacket, ConnectPayload, ConnectProperties, Will},
    disconnect::DisconnectPacket,
    publish::PublishPacket,
    subscribe::{SubscribePacket, SubscribePayload, SubscriptionOptions},
    ControlPacket,
};
use mercurio_server::{auth::SimpleCredentialValidator, server::ServerConfig};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

async fn read(stream: &mut TcpStream, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    timeout(Duration::from_secs(2), stream.read_exact(&mut buf))
        .await
        .expect("timed out waiting for broker")
        .unwrap();
    buf
}

/// MQTT 5 CONNECT with client id "dup"; returns the CONNACK flags byte.
async fn connect(addr: SocketAddr, clean_start: bool) -> (TcpStream, u8) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let flags = if clean_start { 0x02 } else { 0x00 };
    let connect = [
        0x10, 0x10, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x05, flags, 0x00, 0x3c, 0x00, 0x00, 0x03,
        b'd', b'u', b'p',
    ];
    stream.write_all(&connect).await.unwrap();
    let connack = common::read_connack(&mut stream).await;
    assert_eq!(connack[3], 0x00, "connection refused");
    (stream, connack[2])
}

#[tokio::test]
async fn new_connection_takes_over_client_id() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(mercurio_server::server::run(
        listener,
        std::future::pending::<()>(),
    ));

    let (mut old, _) = connect(addr, true).await;
    // SUBSCRIBE packet id 1, "t" at QoS 0, then wait for SUBACK
    old.write_all(&[0x82, 0x07, 0x00, 0x01, 0x00, 0x00, 0x01, b't', 0x00])
        .await
        .unwrap();
    read(&mut old, 6).await;

    // The old session had no expiry, so it ended with the connection taken
    // over, and the new one starts afresh [MQTT-3.1.2-6]
    let (mut new, session_present) = connect(addr, false).await;
    assert_eq!(session_present, 0x00);

    // The old connection is told why, then closed
    assert_eq!(
        common::disconnect_reason(&common::read_disconnect(&mut old).await),
        0x8e
    );
    assert_eq!(old.read(&mut [0u8; 1]).await.unwrap(), 0);

    let mut publisher = TcpStream::connect(addr).await.unwrap();
    publisher
        .write_all(&[
            0x10, 0x0e, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x05, 0x02, 0x00, 0x3c, 0x00, 0x00,
            0x01, b'p',
        ])
        .await
        .unwrap();
    common::read_connack(&mut publisher).await;
    publisher
        .write_all(&[0x30, 0x06, 0x00, 0x01, b't', 0x00, b'h', b'i'])
        .await
        .unwrap();
    assert_nothing_arrives(&mut new).await;
}

async fn assert_nothing_arrives(stream: &mut TcpStream) {
    let mut buf = [0u8; 1];
    let read = timeout(Duration::from_millis(300), stream.read(&mut buf)).await;
    assert!(read.is_err(), "received {read:?}: {buf:02x?}");
}

/// MQTT 3.1.1 CONNECT with client id "dup"; returns the CONNACK.
async fn connect_311(addr: SocketAddr, clean_session: bool) -> (TcpStream, Vec<u8>) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let flags = if clean_session { 0x02 } else { 0x00 };
    let connect = [
        0x10, 0x0f, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x04, flags, 0x00, 0x3c, 0x00, 0x03, b'd',
        b'u', b'p',
    ];
    stream.write_all(&connect).await.unwrap();
    let connack = common::read_connack(&mut stream).await;
    (stream, connack)
}

#[tokio::test]
async fn mqtt311_takeover_of_a_clean_session_starts_a_new_one() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(mercurio_server::server::run(
        listener,
        std::future::pending::<()>(),
    ));

    let (mut old, _) = connect_311(addr, true).await;
    // SUBSCRIBE packet id 1, "t" at QoS 0, then wait for SUBACK
    old.write_all(&[0x82, 0x06, 0x00, 0x01, 0x00, 0x01, b't', 0x00])
        .await
        .unwrap();
    read(&mut old, 5).await;

    let (mut new, connack) = connect_311(addr, false).await;
    assert_eq!(connack, [0x20, 0x02, 0x00, 0x00], "Session Present set");
    assert_eq!(
        timeout(Duration::from_secs(2), old.read(&mut [0u8; 1]))
            .await
            .unwrap()
            .unwrap(),
        0
    );

    let (mut publisher, _) = {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(&[
                0x10, 0x0d, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x04, 0x02, 0x00, 0x3c, 0x00, 0x01,
                b'p',
            ])
            .await
            .unwrap();
        let connack = common::read_connack(&mut stream).await;
        (stream, connack)
    };
    publisher
        .write_all(&[0x30, 0x05, 0x00, 0x01, b't', b'h', b'i'])
        .await
        .unwrap();
    assert_nothing_arrives(&mut new).await;
}

/// Broker with users alice and mallory, whose passwords are `<user>-pw`.
/// Anonymous clients are allowed too.
async fn start_authenticating_broker() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut config = ServerConfig::default();
    config.auth.require_auth = true;
    config.auth.allow_anonymous = true;
    config.plugins = vec![Arc::new(SimpleCredentialValidator::new(HashMap::from([
        ("alice".to_string(), "alice-pw".to_string()),
        ("mallory".to_string(), "mallory-pw".to_string()),
    ])))];
    tokio::spawn(mercurio_server::server::run_with_config(
        listener,
        config,
        std::future::pending::<()>(),
    ));
    addr
}

struct Options<'a> {
    user: Option<&'a str>,
    clean_start: bool,
    session_expiry: Option<u32>,
    will: Option<&'a str>,
}

impl Default for Options<'_> {
    fn default() -> Self {
        Options {
            user: None,
            clean_start: true,
            session_expiry: None,
            will: None,
        }
    }
}

fn alice() -> Options<'static> {
    Options {
        user: Some("alice"),
        ..Default::default()
    }
}

fn mallory() -> Options<'static> {
    Options {
        user: Some("mallory"),
        ..Default::default()
    }
}

struct Client {
    stream: TcpStream,
    buffer: BytesMut,
}

impl Client {
    async fn connect(
        addr: SocketAddr,
        client_id: &str,
        options: Options<'_>,
    ) -> (Self, ConnAckPacket) {
        let password = options.user.map(|user| format!("{user}-pw"));
        let connect = ConnectPacket {
            protocol_version: ProtocolVersion::V5,
            clean_start: options.clean_start,
            keepalive: 60,
            properties: Some(ConnectProperties {
                session_expiry_interval: options.session_expiry.map(SessionExpiryInterval::new),
                ..Default::default()
            }),
            payload: ConnectPayload {
                client_id: client_id.into(),
                user_name: options.user.map(Into::into),
                password: password.map(Bytes::from),
                will: options.will.map(|topic| Will {
                    properties: Some(Default::default()),
                    topic: topic.into(),
                    payload: Bytes::from_static(b"bye"),
                    ..Default::default()
                }),
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
        (client, connack)
    }

    async fn send(&mut self, packet: ControlPacket) {
        let mut encoded = BytesMut::new();
        packet.encode(&mut encoded);
        self.stream.write_all(&encoded).await.unwrap();
    }

    /// The next packet, or None when none arrives for a while.
    async fn receive(&mut self) -> Option<ControlPacket> {
        loop {
            if ControlPacket::check(&self.buffer).is_ok() {
                let packet =
                    ControlPacket::parse_with_version(&mut self.buffer, ProtocolVersion::V5);
                return Some(packet.unwrap());
            }
            match timeout(
                Duration::from_millis(300),
                self.stream.read_buf(&mut self.buffer),
            )
            .await
            {
                Ok(read) => assert_ne!(read.unwrap(), 0, "connection closed"),
                Err(_) => return None,
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

    async fn publish(&mut self, topic: &str, payload: &'static [u8]) {
        self.send(ControlPacket::Publish(PublishPacket {
            qos_level: QoS::AtLeastOnce,
            topic_name: topic.into(),
            packet_id: Some(1),
            payload: Some(Bytes::from_static(payload)),
            ..Default::default()
        }))
        .await;
        let Some(ControlPacket::PubAck(_)) = self.receive().await else {
            panic!("Expected PUBACK");
        };
    }

    async fn expect_publish(&mut self, payload: &[u8]) {
        let Some(ControlPacket::Publish(publish)) = self.receive().await else {
            panic!("Expected PUBLISH");
        };
        assert_eq!(publish.payload.as_deref(), Some(payload));
    }

    async fn expect_taken_over(&mut self) {
        let Some(ControlPacket::Disconnect(disconnect)) = self.receive().await else {
            panic!("Expected DISCONNECT");
        };
        assert_eq!(disconnect.reason, ReasonCode::SessionTakenOver);
    }

    async fn disconnect(mut self) {
        self.send(ControlPacket::Disconnect(DisconnectPacket {
            reason: ReasonCode::NormalDisconnection,
            properties: None,
        }))
        .await;
        let _ = timeout(Duration::from_secs(2), self.stream.read(&mut [0u8; 1])).await;
    }
}

#[tokio::test]
async fn another_user_cannot_take_over_a_connected_client() {
    let addr = start_authenticating_broker().await;
    let (mut watcher, _) = Client::connect(addr, "watcher", Options::default()).await;
    watcher.subscribe("will/#").await;
    let (mut victim, connack) = Client::connect(
        addr,
        "sensor-1",
        Options {
            will: Some("will/sensor-1"),
            ..alice()
        },
    )
    .await;
    assert_eq!(connack.reason_code, ReasonCode::Success);
    victim.subscribe("t").await;

    for clean_start in [false, true] {
        let options = Options {
            clean_start,
            ..mallory()
        };
        let (_, connack) = Client::connect(addr, "sensor-1", options).await;
        assert_eq!(connack.reason_code, ReasonCode::NotAuthorized);
    }
    // An anonymous client is another owner too
    let (_, connack) = Client::connect(addr, "sensor-1", Options::default()).await;
    assert_eq!(connack.reason_code, ReasonCode::NotAuthorized);

    // The victim keeps its connection, its subscription and its will
    assert!(victim.receive().await.is_none());
    watcher.publish("t", b"still here").await;
    victim.expect_publish(b"still here").await;
    assert!(watcher.receive().await.is_none(), "the will was published");
}

#[tokio::test]
async fn another_user_cannot_take_over_an_offline_session() {
    let addr = start_authenticating_broker().await;
    let persistent = |options| Options {
        clean_start: false,
        session_expiry: Some(300),
        ..options
    };
    let (mut victim, _) = Client::connect(addr, "sensor-1", persistent(alice())).await;
    victim.subscribe("t").await;
    victim.disconnect().await;

    let (mut publisher, _) = Client::connect(addr, "publisher", Options::default()).await;
    publisher.publish("t", b"queued").await;

    for clean_start in [false, true] {
        let options = Options {
            clean_start,
            ..persistent(mallory())
        };
        let (_, connack) = Client::connect(addr, "sensor-1", options).await;
        assert_eq!(connack.reason_code, ReasonCode::NotAuthorized);
    }

    let (mut victim, connack) = Client::connect(addr, "sensor-1", persistent(alice())).await;
    assert_eq!(connack.reason_code, ReasonCode::Success);
    assert!(connack.flags.session_present);
    victim.expect_publish(b"queued").await;
}

#[tokio::test]
async fn the_same_user_takes_over_its_client_id() {
    let addr = start_authenticating_broker().await;
    let persistent = Options {
        session_expiry: Some(300),
        ..alice()
    };
    let (mut old, _) = Client::connect(addr, "sensor-1", persistent).await;
    old.subscribe("t").await;
    let options = Options {
        clean_start: false,
        ..alice()
    };
    let (mut new, connack) = Client::connect(addr, "sensor-1", options).await;
    assert_eq!(connack.reason_code, ReasonCode::Success);
    assert!(connack.flags.session_present);
    old.expect_taken_over().await;

    let (mut publisher, _) = Client::connect(addr, "publisher", Options::default()).await;
    publisher.publish("t", b"resumed").await;
    new.expect_publish(b"resumed").await;
}

#[tokio::test]
async fn takeover_of_a_session_without_expiry_starts_a_new_one() {
    let addr = start_authenticating_broker().await;
    let (mut old, _) = Client::connect(addr, "sensor-1", alice()).await;
    old.subscribe("t").await;
    let options = Options {
        clean_start: false,
        ..alice()
    };
    let (mut new, connack) = Client::connect(addr, "sensor-1", options).await;
    assert_eq!(connack.reason_code, ReasonCode::Success);
    assert!(!connack.flags.session_present);
    old.expect_taken_over().await;

    let (mut publisher, _) = Client::connect(addr, "publisher", Options::default()).await;
    publisher.publish("t", b"lost").await;
    assert!(new.receive().await.is_none());
}

#[tokio::test]
async fn anonymous_clients_take_over_each_other() {
    let addr = start_authenticating_broker().await;
    let (mut old, _) = Client::connect(addr, "sensor-1", Options::default()).await;
    let (_new, connack) = Client::connect(addr, "sensor-1", Options::default()).await;
    assert_eq!(connack.reason_code, ReasonCode::Success);
    old.expect_taken_over().await;
}

#[tokio::test]
async fn mqtt311_client_of_another_user_gets_not_authorized() {
    let addr = start_authenticating_broker().await;
    let (mut victim, _) = Client::connect(addr, "dup", alice()).await;

    // MQTT 3.1.1, clean session, user mallory with her password
    let mut packet = vec![0x00, 0x04, b'M', b'Q', b'T', b'T', 0x04, 0xc2, 0x00, 0x3c];
    packet.extend_from_slice(&[0x00, 0x03, b'd', b'u', b'p']);
    packet.extend_from_slice(&[0x00, 0x07]);
    packet.extend_from_slice(b"mallory");
    packet.extend_from_slice(&[0x00, 0x0a]);
    packet.extend_from_slice(b"mallory-pw");
    let mut connect = vec![0x10, packet.len() as u8];
    connect.extend_from_slice(&packet);
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(&connect).await.unwrap();
    assert_eq!(
        common::read_connack(&mut stream).await,
        [0x20, 0x02, 0x00, 0x05]
    );

    assert!(victim.receive().await.is_none());
}
