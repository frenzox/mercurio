use std::time::Duration;

use bytes::BytesMut;
use mercurio_core::{protocol::ProtocolVersion, qos::QoS};
use mercurio_packets::ControlPacket;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::{sleep, timeout},
};

const MESSAGES: u8 = 100;

async fn connect(addr: std::net::SocketAddr, client_id: u8) -> TcpStream {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(&[
            0x10, 0x0e, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x05, 0x02, 0x00, 0x3c, 0x00, 0x00,
            0x01, client_id,
        ])
        .await
        .unwrap();
    let mut connack = [0u8; 5];
    stream.read_exact(&mut connack).await.unwrap();
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
async fn slow_subscriber_receives_every_qos1_message() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(mercurio_server::server::run(
        listener,
        std::future::pending::<()>(),
    ));

    let mut subscriber = connect(addr, b's').await;
    // SUBSCRIBE packet id 1, "t" at QoS 1
    subscriber
        .write_all(&[0x82, 0x06, 0x00, 0x01, 0x00, 0x00, 0x01, b't', 0x01])
        .await
        .unwrap();
    let mut buffer = BytesMut::new();
    assert!(matches!(
        next_packet(&mut subscriber, &mut buffer).await,
        ControlPacket::SubAck(_)
    ));

    // Publish a burst without the subscriber reading anything
    let mut publisher = connect(addr, b'p').await;
    for i in 0..MESSAGES {
        // PUBLISH QoS 1 to "t", packet id i + 1, payload [i]
        publisher
            .write_all(&[0x32, 0x07, 0x00, 0x01, b't', 0x00, i + 1, 0x00, i])
            .await
            .unwrap();
    }
    let mut pubacks = [0u8; 4 * MESSAGES as usize];
    publisher.read_exact(&mut pubacks).await.unwrap();
    sleep(Duration::from_millis(100)).await;

    for i in 0..MESSAGES {
        let ControlPacket::Publish(publish) = next_packet(&mut subscriber, &mut buffer).await
        else {
            panic!("Expected Publish");
        };
        assert_eq!(publish.qos_level, QoS::AtLeastOnce);
        assert_eq!(publish.payload.unwrap()[..], [i]);
    }
}
