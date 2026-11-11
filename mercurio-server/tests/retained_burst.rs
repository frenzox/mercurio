mod common;

use std::{collections::BTreeSet, net::SocketAddr, time::Duration};

use bytes::{Bytes, BytesMut};
use mercurio_core::{protocol::ProtocolVersion, qos::QoS};
use mercurio_packets::{publish::PublishPacket, ControlPacket, Packet};
use mercurio_server::server::{Limits, QueueLimits, ServerConfig};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

const TOPICS: usize = 50;

async fn start_broker(max_qos0: usize, max_messages: usize) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut queue = QueueLimits::default();
    queue.max_qos0 = max_qos0;
    queue.max_messages = max_messages;
    let mut limits = Limits::default();
    limits.queue = queue;
    let mut config = ServerConfig::default();
    config.limits = limits;
    tokio::spawn(mercurio_server::server::run_with_config(
        listener,
        config,
        std::future::pending::<()>(),
    ));
    addr
}

/// MQTT 5 CONNECT with a Receive Maximum of 2.
async fn connect(addr: SocketAddr, client_id: u8) -> TcpStream {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(&[
            0x10, 0x11, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x05, 0x02, 0x00, 0x3c, 0x03, 0x21,
            0x00, 0x02, 0x00, 0x01, client_id,
        ])
        .await
        .unwrap();
    common::read_connack(&mut stream).await;
    stream
}

async fn next_packet(stream: &mut TcpStream, buffer: &mut BytesMut) -> Option<ControlPacket> {
    loop {
        if ControlPacket::check(buffer).is_ok() {
            return Some(ControlPacket::parse(buffer, ProtocolVersion::V5).unwrap());
        }
        match timeout(Duration::from_millis(500), stream.read_buf(buffer)).await {
            Ok(read) => assert_ne!(read.unwrap(), 0, "connection closed"),
            Err(_) => return None,
        }
    }
}

/// Retains `TOPICS` messages at `qos` under `d/<i>/s`.
async fn retain_many(addr: SocketAddr, qos: QoS) {
    let mut publisher = connect(addr, b'p').await;
    let mut packets = BytesMut::new();
    for i in 0..TOPICS {
        PublishPacket {
            dup: false,
            qos_level: qos,
            retain: true,
            topic_name: format!("d/{i}/s"),
            packet_id: (qos != QoS::AtMostOnce).then_some(i as u16 + 1),
            properties: None,
            payload: Some(Bytes::from(i.to_string())),
        }
        .encode(&mut packets, ProtocolVersion::V5);
    }
    publisher.write_all(&packets).await.unwrap();
    // PINGREQ, answered once every PUBLISH before it was handled
    publisher.write_all(&[0xc0, 0x00]).await.unwrap();
    let mut buffer = BytesMut::new();
    while let Some(packet) = next_packet(&mut publisher, &mut buffer).await {
        if matches!(packet, ControlPacket::PingResp(_)) {
            return;
        }
    }
    panic!("no PINGRESP");
}

/// Subscribes to `d/+/s` at `qos` and returns the retained topics received.
async fn receive_retained(addr: SocketAddr, qos: QoS) -> BTreeSet<String> {
    let mut subscriber = connect(addr, b's').await;
    subscriber
        .write_all(&[
            0x82, 0x0b, 0x00, 0x01, 0x00, 0x00, 0x05, b'd', b'/', b'+', b'/', b's', qos as u8,
        ])
        .await
        .unwrap();
    let mut buffer = BytesMut::new();
    assert!(matches!(
        next_packet(&mut subscriber, &mut buffer).await,
        Some(ControlPacket::SubAck(_))
    ));

    let mut topics = BTreeSet::new();
    while let Some(packet) = next_packet(&mut subscriber, &mut buffer).await {
        let ControlPacket::Publish(publish) = packet else {
            panic!("Expected Publish");
        };
        assert!(publish.retain);
        if let Some(id) = publish.packet_id {
            let id = id.to_be_bytes();
            subscriber
                .write_all(&[0x40, 0x02, id[0], id[1]])
                .await
                .unwrap();
        }
        assert!(topics.insert(publish.topic_name), "received twice");
    }
    topics
}

#[tokio::test]
async fn every_retained_qos0_message_reaches_a_new_subscriber() {
    let addr = start_broker(3, 10_000).await;
    retain_many(addr, QoS::AtMostOnce).await;

    assert_eq!(receive_retained(addr, QoS::AtMostOnce).await.len(), TOPICS);
}

#[tokio::test]
async fn every_retained_qos1_message_reaches_a_new_subscriber() {
    let addr = start_broker(3, 4).await;
    retain_many(addr, QoS::AtLeastOnce).await;

    assert_eq!(receive_retained(addr, QoS::AtLeastOnce).await.len(), TOPICS);
}
