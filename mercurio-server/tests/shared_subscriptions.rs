//! What becomes of the messages a member of a share group was given when
//! it leaves the group.

use std::{net::SocketAddr, time::Duration};

use bytes::{Bytes, BytesMut};
use mercurio_core::{
    properties::{ReceiveMaximum, SessionExpiryInterval},
    protocol::ProtocolVersion,
    qos::QoS,
    reason::ReasonCode,
};
use mercurio_packets::{
    connect::{ConnectPacket, ConnectPayload, ConnectProperties},
    puback::PubAckPacket,
    publish::PublishPacket,
    subscribe::{SubscribePacket, SubscribePayload, SubscriptionOptions},
    ControlPacket,
};
use mercurio_server::server::{ServerConfig, ShareRedelivery};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::{sleep, timeout},
};

async fn start_broker(redelivery: ShareRedelivery) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut config = ServerConfig::default();
    config.share_redelivery = redelivery;
    tokio::spawn(mercurio_server::server::run_with_config(
        listener,
        config,
        std::future::pending::<()>(),
    ));
    addr
}

struct Client {
    stream: TcpStream,
    buffer: BytesMut,
}

impl Client {
    /// Connects with a Session Expiry Interval of `expiry` seconds and a
    /// Receive Maximum of `receive_maximum`; returns whether the session was
    /// resumed.
    async fn connect(
        addr: SocketAddr,
        client_id: &str,
        clean_start: bool,
        expiry: u32,
        receive_maximum: u16,
    ) -> (Self, bool) {
        let mut client = Client {
            stream: TcpStream::connect(addr).await.unwrap(),
            buffer: BytesMut::new(),
        };
        client
            .send(ControlPacket::Connect(ConnectPacket {
                protocol_version: ProtocolVersion::V5,
                clean_start,
                keepalive: 60,
                properties: Some(ConnectProperties {
                    session_expiry_interval: Some(SessionExpiryInterval::new(expiry)),
                    receive_maximum: Some(ReceiveMaximum::new(receive_maximum)),
                    ..Default::default()
                }),
                payload: ConnectPayload {
                    client_id: client_id.into(),
                    ..Default::default()
                },
            }))
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

    /// The next packet, or None once the broker sends nothing for a while.
    async fn receive(&mut self) -> Option<ControlPacket> {
        loop {
            if ControlPacket::check(&self.buffer).is_ok() {
                let packet = ControlPacket::parse(&mut self.buffer, ProtocolVersion::V5);
                return Some(packet.unwrap());
            }
            match timeout(
                Duration::from_millis(400),
                self.stream.read_buf(&mut self.buffer),
            )
            .await
            {
                Ok(Ok(read)) if read > 0 => {}
                _ => return None,
            }
        }
    }

    async fn subscribe(&mut self, filter: &str, qos: QoS) {
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
        let Some(ControlPacket::SubAck(_)) = self.receive().await else {
            panic!("Expected SUBACK");
        };
    }

    /// Publishes to "jobs" without waiting for an acknowledgement.
    async fn publish(&mut self, payload: &'static str, qos: QoS, packet_id: u16) {
        self.send(ControlPacket::Publish(PublishPacket {
            topic_name: "jobs".into(),
            payload: Some(Bytes::from_static(payload.as_bytes())),
            qos_level: qos,
            packet_id: Some(packet_id),
            ..Default::default()
        }))
        .await;
    }

    /// The payloads of the PUBLISH packets that come, each acknowledged if
    /// at QoS 1, with whether it was flagged as a duplicate.
    async fn drain(&mut self) -> Vec<(String, bool)> {
        let mut received = Vec::new();
        while let Some(packet) = self.receive().await {
            let ControlPacket::Publish(publish) = packet else {
                continue;
            };
            if publish.qos_level == QoS::AtLeastOnce {
                self.send(ControlPacket::PubAck(PubAckPacket {
                    packet_id: publish.packet_id.unwrap(),
                    reason: ReasonCode::Success,
                    properties: None,
                }))
                .await;
            }
            let payload = publish.payload.unwrap_or_default();
            received.push((String::from_utf8(payload.to_vec()).unwrap(), publish.dup));
        }
        received
    }
}

/// A member of `$share/g/jobs` holding, unacknowledged, "two" at QoS 2 and
/// "one" at QoS 1, and "three" queued behind its Receive Maximum; then a
/// second member, `b`, which acknowledges what it gets.
async fn member_holding_messages(addr: SocketAddr, expiry: u32) -> (Client, Client) {
    let (mut a, _) = Client::connect(addr, "a", true, expiry, 2).await;
    a.subscribe("$share/g/jobs", QoS::ExactlyOnce).await;
    let (mut publisher, _) = Client::connect(addr, "p", true, 0, 10).await;
    publisher.publish("two", QoS::ExactlyOnce, 1).await;
    publisher.publish("one", QoS::AtLeastOnce, 2).await;
    publisher.publish("three", QoS::AtLeastOnce, 3).await;
    let mut sent = Vec::new();
    while let Some(packet) = a.receive().await {
        if let ControlPacket::Publish(publish) = packet {
            sent.push(publish.payload.unwrap());
        }
    }
    assert_eq!(sent, ["two", "one"]);

    let (mut b, _) = Client::connect(addr, "b", true, 0, 10).await;
    b.subscribe("$share/g/jobs", QoS::AtLeastOnce).await;
    (a, b)
}

/// Once the session of a member ends, the QoS 1 message it never
/// acknowledged and the one still queued for it go to another member. The
/// QoS 2 message it was sent goes to no one else [MQTT-4.8.2-5].
#[tokio::test]
async fn an_ended_members_messages_go_to_another_member() {
    let addr = start_broker(ShareRedelivery::default()).await;
    let (a, mut b) = member_holding_messages(addr, 1).await;

    drop(a);
    // The session outlives the connection by a second, and its messages
    // wait for it meanwhile
    assert!(b.drain().await.is_empty());
    sleep(Duration::from_millis(800)).await;
    let received = b.drain().await;
    assert_eq!(
        received,
        [("one".to_string(), false), ("three".to_string(), false)]
    );
}

/// With redelivery on disconnect, the messages go to another member as
/// soon as the first loses its connection, and the session that comes
/// back has only the QoS 2 one left to finish.
#[tokio::test]
async fn a_disconnected_members_messages_go_to_another_member_when_asked() {
    let addr = start_broker(ShareRedelivery::OnDisconnect).await;
    let (a, mut b) = member_holding_messages(addr, 3600).await;

    drop(a);
    let received = b.drain().await;
    assert_eq!(
        received,
        [("one".to_string(), false), ("three".to_string(), false)]
    );

    let (mut a, resumed) = Client::connect(addr, "a", false, 3600, 2).await;
    assert!(resumed);
    let mut resent = Vec::new();
    while let Some(packet) = a.receive().await {
        if let ControlPacket::Publish(publish) = packet {
            resent.push((publish.payload.unwrap(), publish.dup));
        }
    }
    assert_eq!(resent, [(Bytes::from_static(b"two"), true)]);
}

/// Without another member connected, a disconnected member keeps its
/// messages for its return.
#[tokio::test]
async fn a_disconnected_member_keeps_what_no_one_else_takes() {
    let addr = start_broker(ShareRedelivery::OnDisconnect).await;
    let (a, b) = member_holding_messages(addr, 3600).await;
    drop(b);
    sleep(Duration::from_millis(200)).await;

    drop(a);
    sleep(Duration::from_millis(200)).await;
    let (mut a, resumed) = Client::connect(addr, "a", false, 3600, 10).await;
    assert!(resumed);
    let mut resent: Vec<String> = Vec::new();
    while let Some(packet) = a.receive().await {
        if let ControlPacket::Publish(publish) = packet {
            resent.push(String::from_utf8(publish.payload.unwrap().to_vec()).unwrap());
        }
    }
    assert_eq!(resent, ["two", "one", "three"]);
}
