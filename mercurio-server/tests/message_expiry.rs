mod common;

use std::{net::SocketAddr, time::Duration};

use bytes::{Bytes, BytesMut};
use mercurio_core::{codec::Encoder, properties::MessageExpiryInterval, qos::QoS};
use mercurio_packets::publish::{PublishPacket, PublishProperties};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::{sleep, timeout},
};

async fn connect(addr: SocketAddr, client_id: u8) -> TcpStream {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(&[
            0x10, 0x0e, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x05, 0x02, 0x00, 0x3c, 0x00, 0x00,
            0x01, client_id,
        ])
        .await
        .unwrap();
    common::read_connack(&mut stream).await;
    stream
}

#[tokio::test]
async fn expired_retained_message_is_not_delivered() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(mercurio_server::server::run(
        listener,
        std::future::pending::<()>(),
    ));

    let mut publisher = connect(addr, b'p').await;
    let publish = PublishPacket {
        qos_level: QoS::AtMostOnce,
        retain: true,
        topic_name: "t".into(),
        properties: Some(PublishProperties {
            message_expiry_interval: Some(MessageExpiryInterval::new(1)),
            ..Default::default()
        }),
        payload: Some(Bytes::from_static(b"short-lived")),
        ..Default::default()
    };
    let mut encoded = BytesMut::new();
    publish.encode(&mut encoded);
    publisher.write_all(&encoded).await.unwrap();

    sleep(Duration::from_millis(1500)).await;

    let mut subscriber = connect(addr, b's').await;
    subscriber
        .write_all(&[0x82, 0x07, 0x00, 0x01, 0x00, 0x00, 0x01, b't', 0x00])
        .await
        .unwrap();
    let mut suback = [0u8; 6];
    subscriber.read_exact(&mut suback).await.unwrap();

    let mut buf = [0u8; 1];
    assert!(
        timeout(Duration::from_millis(300), subscriber.read(&mut buf))
            .await
            .is_err(),
        "expired retained message was delivered"
    );
}
