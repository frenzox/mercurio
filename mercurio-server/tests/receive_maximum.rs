mod common;

use std::{net::SocketAddr, time::Duration};

use bytes::BytesMut;
use mercurio_core::protocol::ProtocolVersion;
use mercurio_packets::ControlPacket;
use mercurio_server::server::{Limits, ServerConfig};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

async fn start_broker(limits: Limits) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = ServerConfig {
        limits,
        ..Default::default()
    };
    tokio::spawn(mercurio_server::server::run_with_config(
        listener,
        config,
        std::future::pending::<()>(),
    ));
    addr
}

/// MQTT 5 CONNECT, optionally announcing a Receive Maximum.
async fn send_connect(addr: SocketAddr, client_id: u8, receive_maximum: Option<u16>) -> TcpStream {
    let mut body = vec![0x00, 0x04, b'M', b'Q', b'T', b'T', 0x05, 0x02, 0x00, 0x3c];
    match receive_maximum {
        Some(max) => {
            body.extend_from_slice(&[0x03, 0x21]);
            body.extend_from_slice(&max.to_be_bytes());
        }
        None => body.push(0x00),
    }
    body.extend_from_slice(&[0x00, 0x01, client_id]);
    let mut packet = vec![0x10, body.len() as u8];
    packet.extend_from_slice(&body);

    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(&packet).await.unwrap();
    stream
}

async fn connect(addr: SocketAddr, client_id: u8, receive_maximum: Option<u16>) -> TcpStream {
    let mut stream = send_connect(addr, client_id, receive_maximum).await;
    common::read_connack(&mut stream).await;
    stream
}

async fn next_packet(stream: &mut TcpStream, buffer: &mut BytesMut) -> Option<ControlPacket> {
    loop {
        if ControlPacket::check(buffer).is_ok() {
            return Some(ControlPacket::parse_with_version(buffer, ProtocolVersion::V5).unwrap());
        }
        match timeout(Duration::from_millis(300), stream.read_buf(buffer)).await {
            Ok(read) => assert_ne!(read.unwrap(), 0, "connection closed"),
            Err(_) => return None,
        }
    }
}

#[tokio::test]
async fn sends_no_more_than_the_client_receive_maximum() {
    let addr = start_broker(Limits::default()).await;

    let mut subscriber = connect(addr, b's', Some(2)).await;
    subscriber
        .write_all(&[0x82, 0x07, 0x00, 0x01, 0x00, 0x00, 0x01, b't', 0x01])
        .await
        .unwrap();
    let mut buffer = BytesMut::new();
    next_packet(&mut subscriber, &mut buffer).await.unwrap();

    let mut publisher = connect(addr, b'p', None).await;
    for i in 1..=5u8 {
        publisher
            .write_all(&[0x32, 0x07, 0x00, 0x01, b't', 0x00, i, 0x00, i])
            .await
            .unwrap();
    }

    let mut first_id = None;
    for _ in 0..2 {
        let Some(ControlPacket::Publish(publish)) = next_packet(&mut subscriber, &mut buffer).await
        else {
            panic!("Expected Publish");
        };
        first_id.get_or_insert(publish.packet_id.unwrap());
    }
    assert!(next_packet(&mut subscriber, &mut buffer).await.is_none());

    // Acknowledging one makes room for exactly one more
    let id = first_id.unwrap().to_be_bytes();
    subscriber
        .write_all(&[0x40, 0x02, id[0], id[1]])
        .await
        .unwrap();
    assert!(matches!(
        next_packet(&mut subscriber, &mut buffer).await,
        Some(ControlPacket::Publish(_))
    ));
    assert!(next_packet(&mut subscriber, &mut buffer).await.is_none());
}

#[tokio::test]
async fn qos0_messages_are_not_held_back_by_a_full_window() {
    let addr = start_broker(Limits::default()).await;

    let mut subscriber = connect(addr, b's', Some(1)).await;
    subscriber
        .write_all(&[0x82, 0x07, 0x00, 0x01, 0x00, 0x00, 0x01, b't', 0x01])
        .await
        .unwrap();
    let mut buffer = BytesMut::new();
    next_packet(&mut subscriber, &mut buffer).await.unwrap();

    let mut publisher = connect(addr, b'p', None).await;
    // QoS 1 "1" and "2", then QoS 0 "3"
    for (header, id, payload) in [(0x32, 1u8, b'1'), (0x32, 2, b'2')] {
        publisher
            .write_all(&[header, 0x07, 0x00, 0x01, b't', 0x00, id, 0x00, payload])
            .await
            .unwrap();
    }
    publisher
        .write_all(&[0x30, 0x05, 0x00, 0x01, b't', 0x00, b'3'])
        .await
        .unwrap();

    let mut payloads = Vec::new();
    while let Some(ControlPacket::Publish(publish)) =
        next_packet(&mut subscriber, &mut buffer).await
    {
        payloads.push(publish.payload.unwrap()[0]);
    }
    // "2" waits for "1" to be acknowledged, "3" doesn't
    assert_eq!(payloads, [b'1', b'3']);
}

#[tokio::test]
async fn client_exceeding_the_broker_receive_maximum_is_disconnected() {
    let addr = start_broker(Limits {
        receive_maximum: 1,
        ..Default::default()
    })
    .await;
    let mut client = connect(addr, b'c', None).await;

    // Two QoS 2 publishes without releasing the first
    for id in [1u8, 2] {
        client
            .write_all(&[0x34, 0x07, 0x00, 0x01, b't', 0x00, id, 0x00, b'x'])
            .await
            .unwrap();
    }

    let mut received = Vec::new();
    timeout(Duration::from_secs(2), client.read_to_end(&mut received))
        .await
        .expect("connection still open")
        .unwrap();
    // PUBREC for the first (no matching subscribers), then DISCONNECT,
    // Receive Maximum exceeded
    let pubrec_len = 2 + received[1] as usize;
    assert_eq!(&received[..5], [0x50, received[1], 0x00, 0x01, 0x10]);
    assert_eq!(common::disconnect_reason(&received[pubrec_len..]), 0x93);
}

#[tokio::test]
async fn zero_receive_maximum_is_refused() {
    let addr = start_broker(Limits::default()).await;
    let mut stream = send_connect(addr, b'z', Some(0)).await;

    let connack = common::read_connack(&mut stream).await;
    // Protocol error
    assert_eq!(connack[3], 0x82);
}
