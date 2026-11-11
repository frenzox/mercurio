mod common;

use std::{net::SocketAddr, time::Duration};

use bytes::{Bytes, BytesMut};
use mercurio_core::{properties::*, protocol::ProtocolVersion, qos::QoS};
use mercurio_packets::{
    publish::{PublishPacket, PublishProperties},
    ControlPacket, Packet,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

async fn connect(addr: SocketAddr, client_id: u8, level: u8) -> TcpStream {
    let mut body = vec![0x00, 0x04, b'M', b'Q', b'T', b'T', level, 0x02, 0x00, 0x3c];
    if level == 5 {
        body.push(0x00);
    }
    body.extend_from_slice(&[0x00, 0x01, client_id]);
    let mut packet = vec![0x10, body.len() as u8];
    packet.extend_from_slice(&body);

    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(&packet).await.unwrap();
    common::read_connack(&mut stream).await;
    stream
}

async fn subscribe(stream: &mut TcpStream, level: u8) {
    if level == 5 {
        stream
            .write_all(&[0x82, 0x07, 0x00, 0x01, 0x00, 0x00, 0x01, b't', 0x00])
            .await
            .unwrap();
    } else {
        stream
            .write_all(&[0x82, 0x06, 0x00, 0x01, 0x00, 0x01, b't', 0x00])
            .await
            .unwrap();
    }
    let mut suback = vec![0u8; if level == 5 { 6 } else { 5 }];
    stream.read_exact(&mut suback).await.unwrap();
}

async fn next_packet(
    stream: &mut TcpStream,
    buffer: &mut BytesMut,
    version: ProtocolVersion,
) -> ControlPacket {
    loop {
        if ControlPacket::check(buffer).is_ok() {
            return ControlPacket::parse(buffer, version).unwrap();
        }
        let read = timeout(Duration::from_secs(2), stream.read_buf(buffer))
            .await
            .expect("timed out waiting for broker")
            .unwrap();
        assert_ne!(read, 0, "connection closed");
    }
}

fn application_properties() -> PublishProperties {
    PublishProperties {
        payload_format_indicator: Some(PayloadFormatIndicator::new(1)),
        content_type: Some(ContentType::new("text/plain".into())),
        response_topic: Some(ResponseTopic::new("replies".into())),
        correlation_data: Some(CorrelationData::new(Bytes::from_static(b"req-1"))),
        user_property: Some(vec![UserProperty::new("k".into(), "v".into())]),
        ..Default::default()
    }
}

#[tokio::test]
async fn application_properties_are_forwarded() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(mercurio_server::server::run(
        listener,
        std::future::pending::<()>(),
    ));

    let mut v5 = connect(addr, b'5', 5).await;
    subscribe(&mut v5, 5).await;
    let mut v3 = connect(addr, b'3', 4).await;
    subscribe(&mut v3, 4).await;

    let mut publisher = connect(addr, b'p', 5).await;
    let publish = PublishPacket {
        dup: false,
        qos_level: QoS::AtMostOnce,
        retain: true,
        topic_name: "t".into(),
        packet_id: None,
        properties: Some(application_properties()),
        payload: Some(Bytes::from_static(b"hi")),
    };
    let mut encoded = BytesMut::new();
    publish.encode(&mut encoded, ProtocolVersion::V5).unwrap();
    publisher.write_all(&encoded).await.unwrap();

    let mut buffer = BytesMut::new();
    let ControlPacket::Publish(received) =
        next_packet(&mut v5, &mut buffer, ProtocolVersion::V5).await
    else {
        panic!("Expected Publish");
    };
    assert_eq!(received.properties, Some(application_properties()));

    // MQTT 3.1.1 subscribers get the message without properties
    let mut buffer = BytesMut::new();
    let ControlPacket::Publish(received) =
        next_packet(&mut v3, &mut buffer, ProtocolVersion::V3_1_1).await
    else {
        panic!("Expected Publish");
    };
    assert_eq!(received.payload, Some(Bytes::from_static(b"hi")));

    // The retained copy keeps them too
    let mut late = connect(addr, b'l', 5).await;
    subscribe(&mut late, 5).await;
    let mut buffer = BytesMut::new();
    let ControlPacket::Publish(retained) =
        next_packet(&mut late, &mut buffer, ProtocolVersion::V5).await
    else {
        panic!("Expected Publish");
    };
    assert!(retained.retain);
    assert_eq!(retained.properties, Some(application_properties()));
}
