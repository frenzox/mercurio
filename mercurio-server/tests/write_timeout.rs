mod common;

use std::{net::SocketAddr, sync::Arc, time::Duration};

use bytes::{Bytes, BytesMut};
use mercurio_core::{protocol::ProtocolVersion, qos::QoS, reason::ReasonCode};
use mercurio_packets::{
    connect::{ConnectPacket, ConnectPayload, ConnectProperties},
    publish::PublishPacket,
    subscribe::{SubscribePacket, SubscribePayload, SubscriptionOptions},
    ControlPacket,
};
use mercurio_server::{
    plugin::{Access, BoxFuture, ClientInfo, Decision, Plugin},
    server::ServerConfig,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpSocket, TcpStream},
    time::timeout,
};

const WRITE_TIMEOUT: Duration = Duration::from_millis(500);

async fn start(mut config: ServerConfig) -> SocketAddr {
    config.sys_interval = None;
    config.limits.write_timeout = WRITE_TIMEOUT;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = common::broker(listener, config).build().await.unwrap();
    tokio::spawn(server.run(std::future::pending::<()>()));
    addr
}

fn connect_packet(client_id: &str, version: ProtocolVersion) -> ConnectPacket {
    ConnectPacket {
        protocol_version: version,
        clean_start: true,
        // No keep-alive: the publisher's loop holds the test's runtime while
        // the socket takes its writes, so the broker can find a keep-alive
        // long expired, and the stalled subscriber must give its id up
        // through the write timeout alone
        keepalive: 0,
        properties: (version == ProtocolVersion::V5).then(ConnectProperties::default),
        payload: ConnectPayload {
            client_id: client_id.into(),
            ..Default::default()
        },
    }
}

async fn send(stream: &mut TcpStream, packet: ControlPacket, version: ProtocolVersion) {
    let mut bytes = BytesMut::new();
    packet.encode(&mut bytes, version).unwrap();
    stream.write_all(&bytes).await.unwrap();
}

async fn receive(stream: &mut TcpStream, version: ProtocolVersion) -> ControlPacket {
    let mut buffer = BytesMut::new();
    loop {
        if let Ok(packet) = ControlPacket::parse(&mut buffer, version) {
            return packet;
        }
        let read = timeout(Duration::from_secs(2), stream.read_buf(&mut buffer)).await;
        assert!(
            matches!(read, Ok(Ok(n)) if n > 0),
            "closed or silent before a whole packet"
        );
    }
}

/// Connects `client_id` on `stream` and returns the CONNACK reason.
async fn connect_on(
    mut stream: TcpStream,
    client_id: &str,
    version: ProtocolVersion,
) -> (TcpStream, ReasonCode) {
    let packet = ControlPacket::Connect(connect_packet(client_id, version));
    send(&mut stream, packet, version).await;
    let ControlPacket::ConnAck(connack) = receive(&mut stream, version).await else {
        panic!("Expected CONNACK");
    };
    (stream, connack.reason_code)
}

async fn connect(addr: SocketAddr, client_id: &str, version: ProtocolVersion) -> ReasonCode {
    let stream = TcpStream::connect(addr).await.unwrap();
    connect_on(stream, client_id, version).await.1
}

async fn subscribe(stream: &mut TcpStream, filter: &str) {
    let subscribe = ControlPacket::Subscribe(SubscribePacket {
        packet_id: 1,
        properties: None,
        payload: vec![SubscribePayload {
            topic_filter: filter.into(),
            subs_opt: SubscriptionOptions {
                qos: QoS::AtMostOnce,
                ..Default::default()
            },
        }],
    });
    send(stream, subscribe, ProtocolVersion::V5).await;
    assert!(matches!(
        receive(stream, ProtocolVersion::V5).await,
        ControlPacket::SubAck(_)
    ));
}

fn publish(topic: &str, payload: Bytes) -> ControlPacket {
    ControlPacket::Publish(PublishPacket {
        topic_name: topic.into(),
        payload: Some(payload),
        ..Default::default()
    })
}

#[tokio::test]
async fn a_subscriber_that_stops_reading_does_not_pin_its_client_id() {
    let addr = start(ServerConfig::default()).await;

    // A small receive buffer, so the broker's writes stall sooner
    let socket = TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(4096).unwrap();
    let stream = socket.connect(addr).await.unwrap();
    let (mut stalled, reason) = connect_on(stream, "dup", ProtocolVersion::V5).await;
    assert_eq!(reason, ReasonCode::Success);
    subscribe(&mut stalled, "t").await;

    // More than the socket buffers hold, never read
    let (mut publisher, reason) = connect_on(
        TcpStream::connect(addr).await.unwrap(),
        "p",
        ProtocolVersion::V5,
    )
    .await;
    assert_eq!(reason, ReasonCode::Success);
    let payload = Bytes::from(vec![0u8; 256 * 1024]);
    for _ in 0..48 {
        send(
            &mut publisher,
            publish("t", payload.clone()),
            ProtocolVersion::V5,
        )
        .await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;

    // The stalled connection gives the id up once its write times out
    let taken_over = timeout(
        Duration::from_secs(5),
        connect(addr, "dup", ProtocolVersion::V5),
    )
    .await
    .expect("the takeover waited for the stalled connection");
    assert_eq!(taken_over, ReasonCode::Success);
}

/// Holds up the delivery of messages on `slow`, for longer than a takeover
/// may wait.
struct SlowDelivery;

impl Plugin for SlowDelivery {
    fn name(&self) -> &str {
        "slow-delivery"
    }

    fn authorize<'a>(
        &'a self,
        _: &'a ClientInfo,
        topic: &'a str,
        access: Access,
    ) -> BoxFuture<'a, Decision> {
        Box::pin(async move {
            if topic == "slow" && access == Access::Read {
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
            Decision::Allow
        })
    }
}

#[tokio::test]
async fn a_takeover_the_old_connection_holds_up_is_refused_as_busy() {
    let mut config = ServerConfig::default();
    config.plugins = vec![Arc::new(SlowDelivery)];
    let addr = start(config).await;

    let (mut busy, reason) = connect_on(
        TcpStream::connect(addr).await.unwrap(),
        "dup",
        ProtocolVersion::V5,
    )
    .await;
    assert_eq!(reason, ReasonCode::Success);
    subscribe(&mut busy, "slow").await;
    let (mut publisher, _) = connect_on(
        TcpStream::connect(addr).await.unwrap(),
        "p",
        ProtocolVersion::V5,
    )
    .await;
    send(
        &mut publisher,
        publish("slow", Bytes::from_static(b"hi")),
        ProtocolVersion::V5,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(
        connect(addr, "dup", ProtocolVersion::V5).await,
        ReasonCode::ServerBusy
    );
    // MQTT 3.1.1 has no Server Busy: Server Unavailable, return code 3
    assert_eq!(
        connect(addr, "dup", ProtocolVersion::V3_1_1).await,
        ReasonCode::ServerUnavailable
    );
}
