mod common;

use std::{net::SocketAddr, time::Duration};

use std::sync::Arc;

use mercurio_server::{
    plugin::{BoxFuture, ClientInfo, Credentials, Decision, Plugin},
    server::{AuthConfig, ListenerConfig, ServerConfig, Transport},
};
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
    let config = ServerConfig::default();
    let listeners = vec![settings(ListenerConfig::new(extra, Transport::Mqtt))];
    tokio::spawn(
        common::broker(main, config)
            .listeners(listeners)
            .run(std::future::pending::<()>()),
    );
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
    let (main, mounted) = start_broker(|listener| {
        let mut value = listener;
        value.mount_point = Some("tenant1/".to_string());
        value
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
    let (main, secured) = start_broker(|listener| {
        let mut value = listener;
        value.auth = Some({
            let mut value = AuthConfig::default();
            value.require_auth = true;
            value.allow_anonymous = false;
            value
        });
        value
    })
    .await;

    assert_eq!(connect(main, b'a').await.1, 0x00);
    // Bad User Name or Password
    assert_eq!(connect(secured, b'b').await.1, 0x86);
}

#[tokio::test]
async fn listeners_can_limit_their_connections() {
    let (main, limited) = start_broker(|listener| {
        let mut value = listener;
        value.max_connections = Some(1);
        value
    })
    .await;

    let (_first, reason) = connect(limited, b'1').await;
    assert_eq!(reason, 0x00);

    // The second is closed unanswered
    let mut second = TcpStream::connect(limited).await.unwrap();
    let mut received = Vec::new();
    timeout(Duration::from_secs(2), second.read_to_end(&mut received))
        .await
        .expect("connection still open")
        .unwrap();
    assert!(received.is_empty());

    // while the broker's other listeners are unaffected
    assert_eq!(connect(main, b'2').await.1, 0x00);
}

#[tokio::test]
async fn listeners_can_lower_packet_size_keepalive_and_connect_time() {
    let (main, strict) = start_broker(|listener| {
        let mut value = listener;
        value.max_packet_size = Some(64);
        value.max_keepalive = Some(30);
        value.connect_timeout = Some(Duration::from_millis(300));
        value
    })
    .await;

    // Maximum Packet Size (0x27) of 64, Server Keep Alive (0x13) of 30
    let connack = connack_of(strict, b's').await;
    assert!(connack.windows(5).any(|w| w == [0x27, 0, 0, 0, 64]));
    assert!(connack.windows(3).any(|w| w == [0x13, 0, 30]));
    let connack = connack_of(main, b'm').await;
    assert!(!connack.windows(5).any(|w| w == [0x27, 0, 0, 0, 64]));
    assert!(!connack.windows(3).any(|w| w == [0x13, 0, 30]));

    // A silent connection is closed after the listener's own timeout
    let mut idle = TcpStream::connect(strict).await.unwrap();
    let mut received = Vec::new();
    timeout(Duration::from_secs(2), idle.read_to_end(&mut received))
        .await
        .expect("still open after the listener's connect timeout")
        .unwrap();
    let mut idle = TcpStream::connect(main).await.unwrap();
    let mut byte = [0u8; 1];
    assert!(timeout(Duration::from_secs(1), idle.read(&mut byte))
        .await
        .is_err());
}

async fn connack_of(addr: SocketAddr, client_id: u8) -> Vec<u8> {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(&[
            0x10, 0x0e, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x05, 0x02, 0x00, 0x00, 0x00, 0x00,
            0x01, client_id,
        ])
        .await
        .unwrap();
    common::read_connack(&mut stream).await
}

/// Lets in any user with the password `secret`.
struct Passwords;

impl Plugin for Passwords {
    fn name(&self) -> &str {
        "passwords"
    }

    fn authenticate<'a>(
        &'a self,
        _: &'a ClientInfo,
        credentials: &'a Credentials<'a>,
    ) -> BoxFuture<'a, Decision> {
        let allowed = credentials.password == b"secret";
        Box::pin(async move {
            match allowed {
                true => Decision::Allow,
                false => Decision::Deny,
            }
        })
    }
}

/// MQTT 5 CONNECT with `client_id` and, when given, a user name with the
/// password `secret`; the CONNACK.
async fn connect_as(addr: SocketAddr, client_id: &str, user: Option<&str>) -> Vec<u8> {
    let mut body = vec![0x00, 0x04, b'M', b'Q', b'T', b'T', 0x05];
    body.push(if user.is_some() { 0xc2 } else { 0x02 });
    body.extend_from_slice(&[0x00, 0x3c, 0x00]);
    let field = |body: &mut Vec<u8>, value: &[u8]| {
        body.extend_from_slice(&(value.len() as u16).to_be_bytes());
        body.extend_from_slice(value);
    };
    field(&mut body, client_id.as_bytes());
    if let Some(user) = user {
        field(&mut body, user.as_bytes());
        field(&mut body, b"secret");
    }
    let mut packet = vec![0x10, body.len() as u8];
    packet.extend(body);
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(&packet).await.unwrap();
    common::read_connack(&mut stream).await
}

#[tokio::test]
async fn listeners_can_use_the_user_name_as_client_id() {
    let (_, named) = start_broker(|listener| {
        let mut value = listener;
        value.use_username_as_clientid = true;
        value.plugins = Some(vec![Arc::new(Passwords) as Arc<dyn Plugin>]);
        value
    })
    .await;

    // Assigned Client Identifier (0x12) "alice", whatever the client sent
    let connack = connect_as(named, "device-7", Some("alice")).await;
    assert_eq!(connack[3], 0x00);
    let assigned = [&[0x12, 0x00, 0x05][..], b"alice"].concat();
    assert!(connack.windows(8).any(|w| w == assigned), "{connack:?}");
    assert_eq!(connect_as(named, "", Some("bob")).await[3], 0x00);
    // Not Authorized without a verified user name
    assert_eq!(connect_as(named, "device-8", None).await[3], 0x87);
}
