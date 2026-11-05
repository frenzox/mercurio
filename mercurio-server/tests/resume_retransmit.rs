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
