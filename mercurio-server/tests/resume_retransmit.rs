mod common;

use std::{net::SocketAddr, time::Duration};

use bytes::{Bytes, BytesMut};
use mercurio_core::{protocol::ProtocolVersion, qos::QoS};
use mercurio_packets::ControlPacket;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

/// MQTT 5 CONNECT with a one minute session expiry.
async fn connect(addr: SocketAddr, client_id: u8, clean: bool) -> TcpStream {
    let flags = if clean { 0x02 } else { 0x00 };
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(&[
            0x10, 0x13, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x05, flags, 0x00, 0x3c, 0x05, 0x11,
            0x00, 0x00, 0x00, 0x3c, 0x00, 0x01, client_id,
        ])
        .await
        .unwrap();
    common::read_connack(&mut stream).await;
    stream
}

async fn next_packet(stream: &mut TcpStream, buffer: &mut BytesMut) -> ControlPacket {
    loop {
        if ControlPacket::check(buffer).is_ok() {
            return ControlPacket::parse_with_version(buffer, ProtocolVersion::V5).unwrap();
        }
        let read = timeout(Duration::from_secs(2), stream.read_buf(buffer))
            .await
            .expect("timed out waiting for broker")
            .unwrap();
        assert_ne!(read, 0, "connection closed");
    }
}

#[tokio::test]
async fn unacknowledged_messages_are_resent_in_order_on_resume() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(mercurio_server::server::run(
        listener,
        std::future::pending::<()>(),
    ));

    let mut subscriber = connect(addr, b'r', true).await;
    let mut buffer = BytesMut::new();
    // SUBSCRIBE packet id 1, "t" at QoS 2
    subscriber
        .write_all(&[0x82, 0x07, 0x00, 0x01, 0x00, 0x00, 0x01, b't', 0x02])
        .await
        .unwrap();
    next_packet(&mut subscriber, &mut buffer).await;

    let mut publisher = connect(addr, b'p', true).await;
    // QoS 1 "1" then QoS 2 "2" to "t"
    publisher
        .write_all(&[0x32, 0x07, 0x00, 0x01, b't', 0x00, 0x01, 0x00, b'1'])
        .await
        .unwrap();
    publisher
        .write_all(&[0x34, 0x07, 0x00, 0x01, b't', 0x00, 0x02, 0x00, b'2'])
        .await
        .unwrap();

    // Leave the QoS 1 message unacknowledged
    let ControlPacket::Publish(first) = next_packet(&mut subscriber, &mut buffer).await else {
        panic!("Expected Publish");
    };
    assert_eq!(first.qos_level, QoS::AtLeastOnce);

    // Take the QoS 2 message up to PUBREL, but never send PUBCOMP
    let ControlPacket::Publish(second) = next_packet(&mut subscriber, &mut buffer).await else {
        panic!("Expected Publish");
    };
    let second_id = second.packet_id.unwrap();
    subscriber
        .write_all(&[0x50, 0x02, 0x00, second_id as u8])
        .await
        .unwrap();
    assert!(matches!(
        next_packet(&mut subscriber, &mut buffer).await,
        ControlPacket::PubRel(ref p) if p.packet_id == second_id
    ));
    drop(subscriber);

    let mut resumed = connect(addr, b'r', false).await;
    let mut buffer = BytesMut::new();

    let ControlPacket::Publish(resent) = next_packet(&mut resumed, &mut buffer).await else {
        panic!("Expected the unacknowledged PUBLISH first");
    };
    assert!(resent.dup);
    assert_eq!(resent.packet_id, first.packet_id);
    assert_eq!(resent.payload, Some(Bytes::from("1")));

    assert!(matches!(
        next_packet(&mut resumed, &mut buffer).await,
        ControlPacket::PubRel(ref p) if p.packet_id == second_id
    ));
}

/// MQTT 5 CONNECT with a one minute session expiry, a Receive Maximum and
/// optionally a Maximum Packet Size.
async fn connect_with(
    addr: SocketAddr,
    client_id: u8,
    clean: bool,
    receive_maximum: u16,
    maximum_packet_size: Option<u32>,
) -> TcpStream {
    let mut properties = vec![0x11, 0x00, 0x00, 0x00, 0x3c, 0x21];
    properties.extend_from_slice(&receive_maximum.to_be_bytes());
    if let Some(size) = maximum_packet_size {
        properties.push(0x27);
        properties.extend_from_slice(&size.to_be_bytes());
    }
    let flags = if clean { 0x02 } else { 0x00 };
    let mut body = vec![0x00, 0x04, b'M', b'Q', b'T', b'T', 0x05, flags, 0x00, 0x3c];
    body.push(properties.len() as u8);
    body.extend_from_slice(&properties);
    body.extend_from_slice(&[0x00, 0x01, client_id]);
    let mut packet = vec![0x10, body.len() as u8];
    packet.extend_from_slice(&body);

    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(&packet).await.unwrap();
    common::read_connack(&mut stream).await;
    stream
}

/// Whether the broker sends nothing more for a while.
async fn quiet(stream: &mut TcpStream, buffer: &mut BytesMut) -> bool {
    ControlPacket::check(buffer).is_err()
        && timeout(Duration::from_millis(300), stream.read_buf(buffer))
            .await
            .is_err()
}

/// Leaves `payloads`, sent at QoS 1 to "t", unacknowledged by client `r`.
async fn leave_unacknowledged(addr: SocketAddr, payloads: &[&[u8]]) {
    let mut subscriber = connect_with(addr, b'r', true, 100, None).await;
    let mut buffer = BytesMut::new();
    subscriber
        .write_all(&[0x82, 0x07, 0x00, 0x01, 0x00, 0x00, 0x01, b't', 0x01])
        .await
        .unwrap();
    next_packet(&mut subscriber, &mut buffer).await;

    let mut publisher = connect(addr, b'p', true).await;
    for (i, payload) in payloads.iter().enumerate() {
        let mut packet = vec![0x32, 6 + payload.len() as u8, 0x00, 0x01, b't', 0x00];
        packet.extend_from_slice(&[i as u8 + 1, 0x00]);
        packet.extend_from_slice(payload);
        publisher.write_all(&packet).await.unwrap();
    }
    for _ in payloads {
        assert!(matches!(
            next_packet(&mut subscriber, &mut buffer).await,
            ControlPacket::Publish(_)
        ));
    }
}

#[tokio::test]
async fn resent_messages_respect_the_new_receive_maximum() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(mercurio_server::server::run(
        listener,
        std::future::pending::<()>(),
    ));
    leave_unacknowledged(addr, &[b"1", b"2", b"3", b"4"]).await;

    let mut resumed = connect_with(addr, b'r', false, 2, None).await;
    let mut buffer = BytesMut::new();
    let mut ids = Vec::new();
    for expected in ["1", "2"] {
        let ControlPacket::Publish(resent) = next_packet(&mut resumed, &mut buffer).await else {
            panic!("Expected Publish");
        };
        assert!(resent.dup);
        assert_eq!(resent.payload, Some(Bytes::from(expected)));
        ids.push(resent.packet_id.unwrap());
    }
    assert!(quiet(&mut resumed, &mut buffer).await);

    // Each acknowledgement lets the next one through, in order
    for (acked, expected) in ids.into_iter().zip(["3", "4"]) {
        let id = acked.to_be_bytes();
        resumed
            .write_all(&[0x40, 0x02, id[0], id[1]])
            .await
            .unwrap();
        let ControlPacket::Publish(resent) = next_packet(&mut resumed, &mut buffer).await else {
            panic!("Expected Publish");
        };
        assert!(resent.dup);
        assert_eq!(resent.payload, Some(Bytes::from(expected)));
        assert!(quiet(&mut resumed, &mut buffer).await);
    }
}

#[tokio::test]
async fn resent_messages_respect_the_new_maximum_packet_size() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(mercurio_server::server::run(
        listener,
        std::future::pending::<()>(),
    ));
    let large = [b'x'; 64];
    leave_unacknowledged(addr, &[b"1", &large, b"3"]).await;

    let mut resumed = connect_with(addr, b'r', false, 10, Some(32)).await;
    let mut buffer = BytesMut::new();
    for expected in ["1", "3"] {
        let ControlPacket::Publish(resent) = next_packet(&mut resumed, &mut buffer).await else {
            panic!("Expected Publish");
        };
        assert_eq!(resent.payload, Some(Bytes::from(expected)));
    }
    assert!(quiet(&mut resumed, &mut buffer).await);
}
