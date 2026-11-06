mod common;

use std::{net::SocketAddr, time::Duration};

use mercurio_server::server::{AuthConfig, ListenerConfig, ServerConfig, Transport};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

/// Starts a broker whose main listener has no settings, with one extra
/// listener configured by `settings`. Returns both addresses.
async fn start_broker(
    settings: impl FnOnce(ListenerConfig) -> ListenerConfig,
) -> (SocketAddr, SocketAddr) {
    let main = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let extra = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addrs = (main.local_addr().unwrap(), extra.local_addr().unwrap());
    let config = ServerConfig {
        listeners: vec![settings(ListenerConfig::new(extra, Transport::Mqtt))],
        ..Default::default()
    };
    tokio::spawn(mercurio_server::server::run_with_config(
        main,
        config,
        std::future::pending::<()>(),
    ));
    addrs
}

/// MQTT 5 CONNECT without credentials; returns the stream and the CONNACK
/// reason code.
async fn connect(addr: SocketAddr, client_id: u8) -> (TcpStream, u8) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(&[
            0x10, 0x0e, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x05, 0x02, 0x00, 0x3c, 0x00, 0x00,
            0x01, client_id,
        ])
        .await
        .unwrap();
    let reason = common::read_connack(&mut stream).await[3];
    (stream, reason)
}

async fn subscribe(stream: &mut TcpStream, filter: &str) {
    let mut packet = vec![0x82, 0, 0x00, 0x01, 0x00, 0x00, filter.len() as u8];
    packet.extend_from_slice(filter.as_bytes());
    packet.push(0x00);
    packet[1] = (packet.len() - 2) as u8;
    stream.write_all(&packet).await.unwrap();
    let mut suback = [0u8; 6];
    stream.read_exact(&mut suback).await.unwrap();
    assert_eq!(suback[5], 0x00, "subscription refused");
}

async fn publish(stream: &mut TcpStream, topic: &str) {
    let mut packet = vec![0x30, 0, 0x00, topic.len() as u8];
    packet.extend_from_slice(topic.as_bytes());
    packet.extend_from_slice(&[0x00, b'm']);
    packet[1] = (packet.len() - 2) as u8;
    stream.write_all(&packet).await.unwrap();
}

/// The topic of the next QoS 0 PUBLISH received.
async fn received_topic(stream: &mut TcpStream) -> String {
    let mut header = [0u8; 2];
    timeout(Duration::from_secs(2), stream.read_exact(&mut header))
        .await
        .expect("no PUBLISH")
        .unwrap();
    assert_eq!(header[0] & 0xf0, 0x30, "expected PUBLISH");
    let mut body = vec![0u8; header[1] as usize];
    stream.read_exact(&mut body).await.unwrap();
    String::from_utf8(body[2..2 + body[1] as usize].to_vec()).unwrap()
}

#[tokio::test]
async fn mount_points_confine_clients_to_their_topics() {
    let (main, mounted) = start_broker(|listener| ListenerConfig {
        mount_point: Some("tenant1/".to_string()),
        ..listener
    })
    .await;

    let (mut outside, _) = connect(main, b'o').await;
    subscribe(&mut outside, "#").await;
    let (mut tenant, _) = connect(mounted, b't').await;
    subscribe(&mut tenant, "#").await;
    subscribe(&mut tenant, "$share/g/jobs").await;

    // The tenant's topics live under the mount point
    publish(&mut tenant, "a").await;
    assert_eq!(received_topic(&mut outside).await, "tenant1/a");
    assert_eq!(received_topic(&mut tenant).await, "a");

    // and the tenant only sees that part of the tree, without the prefix
    publish(&mut outside, "elsewhere").await;
    assert_eq!(received_topic(&mut outside).await, "elsewhere");
    publish(&mut outside, "tenant1/jobs").await;
    assert_eq!(received_topic(&mut outside).await, "tenant1/jobs");
    assert_eq!(received_topic(&mut tenant).await, "jobs");
    assert_eq!(received_topic(&mut tenant).await, "jobs");
}

#[tokio::test]
async fn listeners_can_require_their_own_authentication() {
    let (main, secured) = start_broker(|listener| ListenerConfig {
        auth: Some(AuthConfig {
            require_auth: true,
            allow_anonymous: false,
            ..Default::default()
        }),
        ..listener
    })
    .await;

    assert_eq!(connect(main, b'a').await.1, 0x00);
    // Bad User Name or Password
    assert_eq!(connect(secured, b'b').await.1, 0x86);
}
