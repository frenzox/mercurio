mod common;

use std::{collections::HashMap, net::SocketAddr, time::Duration};

use bytes::BytesMut;
use mercurio_core::protocol::ProtocolVersion;
use mercurio_packets::ControlPacket;
use mercurio_server::server::ServerConfig;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::{sleep, timeout},
};

async fn start_broker() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = ServerConfig {
        sys_interval: Some(Duration::from_millis(100)),
        ..Default::default()
    };
    tokio::spawn(mercurio_server::server::run_with_config(
        listener,
        config,
        std::future::pending::<()>(),
    ));
    addr
}

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

/// Collects the retained $SYS messages a new subscriber receives.
async fn statistics(addr: SocketAddr) -> HashMap<String, String> {
    let mut stream = connect(addr, b's').await;
    // SUBSCRIBE packet id 1, "$SYS/broker/#"
    let mut subscribe = vec![0x82, 0x13, 0x00, 0x01, 0x00, 0x00, 0x0d];
    subscribe.extend_from_slice(b"$SYS/broker/#");
    subscribe.push(0x00);
    stream.write_all(&subscribe).await.unwrap();

    let mut buffer = BytesMut::new();
    let mut values = HashMap::new();
    // Every statistic is retained, so a new subscriber gets all seven
    timeout(Duration::from_secs(2), async {
        while values.len() < 7 {
            if ControlPacket::check(&mut buffer).is_ok() {
                if let ControlPacket::Publish(publish) =
                    ControlPacket::parse_with_version(&mut buffer, ProtocolVersion::V5).unwrap()
                {
                    let value = String::from_utf8(publish.payload.unwrap().to_vec()).unwrap();
                    values.insert(publish.topic_name, value);
                }
                continue;
            }
            let read = stream.read_buf(&mut buffer).await.unwrap();
            assert_ne!(read, 0, "connection closed");
        }
    })
    .await
    .expect("not every statistic arrived");
    values
}

#[tokio::test]
async fn statistics_are_published_under_sys() {
    let addr = start_broker().await;
    let mut publisher = connect(addr, b'p').await;
    for _ in 0..3 {
        publisher
            .write_all(&[0x30, 0x06, 0x00, 0x01, b't', 0x00, b'h', b'i'])
            .await
            .unwrap();
    }
    sleep(Duration::from_millis(250)).await;

    let values = statistics(addr).await;
    assert!(values["$SYS/broker/version"].starts_with("mercurio "));
    assert_eq!(values["$SYS/broker/messages/received"], "3");
    // The publisher, and the subscriber reading the statistics
    assert!(["1", "2"].contains(&values["$SYS/broker/clients/connected"].as_str()));
    assert!(values.contains_key("$SYS/broker/uptime"));
    assert!(values.contains_key("$SYS/broker/subscriptions/count"));
}

#[tokio::test]
async fn clients_cannot_publish_to_sys() {
    let addr = start_broker().await;
    let mut client = connect(addr, b'c').await;

    // QoS 1 PUBLISH to "$SYS/broker/version", packet id 1
    let mut publish = vec![0x32, 0x19, 0x00, 0x13];
    publish.extend_from_slice(b"$SYS/broker/version");
    publish.extend_from_slice(&[0x00, 0x01, 0x00, b'f', b'a', b'k', b'e']);
    client.write_all(&publish).await.unwrap();

    let mut header = [0u8; 2];
    timeout(Duration::from_secs(2), client.read_exact(&mut header))
        .await
        .unwrap()
        .unwrap();
    let mut rest = vec![0u8; header[1] as usize];
    client.read_exact(&mut rest).await.unwrap();
    // PUBACK for packet id 1, Not authorized
    assert_eq!(header[0], 0x40);
    assert_eq!(rest[..3], [0x00, 0x01, 0x87]);
}
