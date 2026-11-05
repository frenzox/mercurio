mod common;

use std::time::Duration;

use bytes::BytesMut;
use mercurio_core::protocol::ProtocolVersion;
use mercurio_packets::ControlPacket;
use mercurio_server::server::Limits;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

async fn connect() -> (TcpStream, Vec<u8>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(mercurio_server::server::run(
        listener,
        std::future::pending::<()>(),
    ));

    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(&[
            0x10, 0x0e, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x05, 0x02, 0x00, 0x3c, 0x00, 0x00,
            0x01, b'c',
        ])
        .await
        .unwrap();
    let connack = common::read_connack(&mut stream).await;
    (stream, connack)
}

#[tokio::test]
async fn connack_advertises_maximum_packet_size() {
    let (_, connack) = connect().await;

    let mut buffer = BytesMut::from(&connack[..]);
    let ControlPacket::ConnAck(connack) =
        ControlPacket::parse_with_version(&mut buffer, ProtocolVersion::V5).unwrap()
    else {
        panic!("Expected ConnAck");
    };
    let properties = connack.properties.unwrap();
    let maximum = properties.maximum_packet_size.unwrap();
    assert_eq!(maximum.value as usize, Limits::default().max_packet_size);
    assert_eq!(
        properties.topic_alias_max.unwrap().value,
        Limits::default().topic_alias_maximum
    );
}

#[tokio::test]
async fn oversized_packet_is_refused_from_its_header() {
    let (mut stream, _) = connect().await;

    // PUBLISH fixed header announcing 2 MiB, without sending the body
    stream
        .write_all(&[0x30, 0x80, 0x80, 0x80, 0x01])
        .await
        .unwrap();

    let mut buf = [0u8; 16];
    let closed = timeout(Duration::from_secs(2), async {
        loop {
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "connection still open");
}
