mod common;

use std::{net::SocketAddr, time::Duration};

use mercurio_server::server::{Limits, ServerConfig};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::{sleep, timeout},
};

async fn start_broker(limits: Limits) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut config = ServerConfig::default();
    config.limits = limits;
    tokio::spawn(mercurio_server::server::run_with_config(
        listener,
        config,
        std::future::pending::<()>(),
    ));
    addr
}

async fn connect(addr: SocketAddr, client_id: u8) -> (TcpStream, Vec<u8>) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(&[
            0x10, 0x0e, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x05, 0x02, 0x00, 0x3c, 0x00, 0x00,
            0x01, client_id,
        ])
        .await
        .unwrap();
    let connack = common::read_connack(&mut stream).await;
    (stream, connack)
}

async fn read_until_closed(stream: &mut TcpStream) -> Vec<u8> {
    let mut received = Vec::new();
    timeout(Duration::from_secs(2), stream.read_to_end(&mut received))
        .await
        .expect("connection still open")
        .unwrap();
    received
}

#[tokio::test]
async fn configured_packet_size_is_advertised_and_enforced() {
    let addr = start_broker({
        let mut value = Limits::default();
        value.max_packet_size = 64;
        value
    })
    .await;

    let (mut stream, connack) = connect(addr, b'c').await;
    // Maximum Packet Size property (0x27) carrying 64
    assert!(connack
        .windows(5)
        .any(|w| w == [0x27, 0x00, 0x00, 0x00, 0x40]));

    // A 100 byte PUBLISH to "t"
    let mut publish = vec![0x30, 98, 0x00, 0x01, b't', 0x00];
    publish.resize(100, b'x');
    stream.write_all(&publish).await.unwrap();

    // DISCONNECT, Packet Too Large
    assert_eq!(
        common::disconnect_reason(&read_until_closed(&mut stream).await),
        0x95
    );
}

#[tokio::test]
async fn connections_beyond_the_limit_are_refused() {
    let addr = start_broker({
        let mut value = Limits::default();
        value.max_connections = Some(1);
        value
    })
    .await;

    let (first, _) = connect(addr, b'a').await;

    let mut refused = TcpStream::connect(addr).await.unwrap();
    assert!(read_until_closed(&mut refused).await.is_empty());

    // Closing the first connection frees its slot
    drop(first);
    sleep(Duration::from_millis(100)).await;
    connect(addr, b'b').await;
}

#[tokio::test]
async fn keep_alive_is_capped_for_mqtt5_clients() {
    let addr = start_broker({
        let mut value = Limits::default();
        value.max_keepalive = Some(1);
        value
    })
    .await;

    // MQTT 5 client asking for a one minute keep-alive
    let (mut stream, connack) = connect(addr, b'c').await;
    // Server Keep Alive property (0x13) of 1 second
    assert!(connack.windows(3).any(|w| w == [0x13, 0x00, 0x01]));
    let disconnect = read_until_closed(&mut stream).await;
    assert_eq!(common::disconnect_reason(&disconnect), 0x8d);

    // MQTT 3.1.1 can't be told, so its keep-alive is left alone
    let mut v3 = TcpStream::connect(addr).await.unwrap();
    v3.write_all(&[
        0x10, 0x0d, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x04, 0x02, 0x00, 0x00, 0x00, 0x01, b'v',
    ])
    .await
    .unwrap();
    common::read_connack(&mut v3).await;
    assert!(timeout(Duration::from_secs(2), v3.read(&mut [0u8; 1]))
        .await
        .is_err());
}

#[tokio::test]
async fn response_information_is_given_to_clients_that_ask() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut config = ServerConfig::default();
    config.policy.response_information = Some("responses/%c".into());
    tokio::spawn(mercurio_server::server::run_with_config(
        listener,
        config,
        std::future::pending::<()>(),
    ));

    // CONNECT for client "r" with Request Response Information 1
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(&[
            0x10, 0x10, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x05, 0x02, 0x00, 0x3c, 0x02, 0x19,
            0x01, 0x00, 0x01, b'r',
        ])
        .await
        .unwrap();
    let connack = common::read_connack(&mut stream).await;

    // Response Information (0x1a) "responses/r"
    let expected = [&[0x1a, 0x00, 0x0b][..], b"responses/r"].concat();
    assert!(connack.windows(expected.len()).any(|w| w == expected));

    // Clients that don't ask don't get it
    let (_, connack) = connect(addr, b'n').await;
    assert!(!connack.contains(&0x1a));
}

#[tokio::test]
async fn clients_are_redirected_when_configured() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut config = ServerConfig::default();
    config.policy.redirect = Some(mercurio_server::server::Redirect::new("other:1883", true));
    tokio::spawn(mercurio_server::server::run_with_config(
        listener,
        config,
        std::future::pending::<()>(),
    ));

    // MQTT 5: Server Moved, with Server Reference (0x1c) "other:1883"
    let (_, connack) = connect(addr, b'c').await;
    assert_eq!(connack[3], 0x9d);
    let expected = [&[0x1c, 0x00, 0x0a][..], b"other:1883"].concat();
    assert!(connack.windows(expected.len()).any(|w| w == expected));

    // MQTT 3.1.1: Server unavailable
    let mut v3 = TcpStream::connect(addr).await.unwrap();
    v3.write_all(&[
        0x10, 0x0d, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x04, 0x02, 0x00, 0x3c, 0x00, 0x01, b'v',
    ])
    .await
    .unwrap();
    assert_eq!(
        common::read_connack(&mut v3).await,
        [0x20, 0x02, 0x00, 0x03]
    );
}

#[tokio::test]
async fn maximum_qos_is_advertised_and_enforced() {
    let addr = start_broker({
        let mut value = Limits::default();
        value.maximum_qos = mercurio_core::qos::QoS::AtLeastOnce;
        value
    })
    .await;

    let (mut stream, connack) = connect(addr, b'q').await;
    // Maximum QoS property (0x24) carrying 1
    assert!(connack.windows(2).any(|w| w == [0x24, 0x01]));

    // SUBSCRIBE to "t" asking for QoS 2 is granted QoS 1
    stream
        .write_all(&[0x82, 0x07, 0x00, 0x01, 0x00, 0x00, 0x01, b't', 0x02])
        .await
        .unwrap();
    let mut suback = [0u8; 6];
    stream.read_exact(&mut suback).await.unwrap();
    assert_eq!(suback, [0x90, 0x04, 0x00, 0x01, 0x00, 0x01]);

    // A QoS 2 PUBLISH costs the connection: DISCONNECT, QoS Not Supported
    stream
        .write_all(&[0x34, 0x06, 0x00, 0x01, b't', 0x00, 0x01, 0x00])
        .await
        .unwrap();
    assert_eq!(
        common::disconnect_reason(&read_until_closed(&mut stream).await),
        0x9b
    );
}

#[tokio::test]
async fn unavailable_retain_is_advertised_and_enforced() {
    let addr = start_broker({
        let mut value = Limits::default();
        value.retain_available = false;
        value
    })
    .await;

    let (mut stream, connack) = connect(addr, b'r').await;
    // Retain Available property (0x25) carrying 0
    assert!(connack.windows(2).any(|w| w == [0x25, 0x00]));

    // A retained PUBLISH: DISCONNECT, Retain Not Supported
    stream
        .write_all(&[0x31, 0x04, 0x00, 0x01, b't', 0x00])
        .await
        .unwrap();
    assert_eq!(
        common::disconnect_reason(&read_until_closed(&mut stream).await),
        0x9a
    );

    // A retained will is refused in CONNACK
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(&[
            0x10, 0x15, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x05, 0x26, 0x00, 0x3c, 0x00, 0x00,
            0x01, b'w', 0x00, 0x00, 0x01, b'x', 0x00, 0x01, b'y',
        ])
        .await
        .unwrap();
    assert_eq!(common::read_connack(&mut stream).await[3], 0x9a);
}

#[tokio::test]
async fn persistent_mqtt3_sessions_expire_when_configured() {
    let addr = start_broker({
        let mut value = Limits::default();
        value.persistent_client_expiration = Some(Duration::from_secs(1));
        value
    })
    .await;

    // MQTT 3.1.1 CONNECT without clean session, client id "p"; returns
    // whether the session was present
    let connect = || async {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(&[
                0x10, 0x0d, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x04, 0x00, 0x00, 0x3c, 0x00, 0x01,
                b'p',
            ])
            .await
            .unwrap();
        common::read_connack(&mut stream).await[2] == 1
    };

    assert!(!connect().await);
    sleep(Duration::from_millis(100)).await;
    assert!(connect().await, "session gone before it expired");
    sleep(Duration::from_millis(1500)).await;
    assert!(!connect().await, "session outlived its expiry");
}

/// SUBSCRIBE with the given filters at QoS 0, packet id 1; returns the
/// SUBACK's reason codes.
async fn subscribe(stream: &mut TcpStream, filters: &[&str]) -> Vec<u8> {
    let mut body = vec![0x00, 0x01, 0x00];
    for filter in filters {
        body.extend((filter.len() as u16).to_be_bytes());
        body.extend(filter.as_bytes());
        body.push(0x00);
    }
    let mut packet = vec![0x82, body.len() as u8];
    packet.extend(body);
    stream.write_all(&packet).await.unwrap();

    let mut header = [0u8; 2];
    stream.read_exact(&mut header).await.unwrap();
    let mut suback = vec![0u8; header[1] as usize];
    stream.read_exact(&mut suback).await.unwrap();
    // Packet id, empty properties, then a reason code per filter
    suback[3..].to_vec()
}

#[tokio::test]
async fn subscriptions_beyond_the_limit_are_refused() {
    let addr = start_broker({
        let mut value = Limits::default();
        value.max_subscriptions = Some(2);
        value
    })
    .await;
    let (mut stream, _) = connect(addr, b's').await;

    // Quota exceeded for the third, new filter
    assert_eq!(
        subscribe(&mut stream, &["a", "b", "c"]).await,
        [0x00, 0x00, 0x97]
    );
    // Replacing a subscription it holds doesn't count as a new one
    assert_eq!(subscribe(&mut stream, &["a"]).await, [0x00]);
}

/// Retained QoS 1 PUBLISH of "v" on `topic` with packet id 1; returns the
/// PUBACK's reason code.
async fn publish_retained(stream: &mut TcpStream, topic: &str) -> u8 {
    let mut body = Vec::new();
    body.extend((topic.len() as u16).to_be_bytes());
    body.extend(topic.as_bytes());
    // Packet id 1, no properties, payload "v"
    body.extend([0x00, 0x01, 0x00, b'v']);
    let mut packet = vec![0x33, body.len() as u8];
    packet.extend(body);
    stream.write_all(&packet).await.unwrap();

    let mut header = [0u8; 2];
    stream.read_exact(&mut header).await.unwrap();
    let mut puback = vec![0u8; header[1] as usize];
    stream.read_exact(&mut puback).await.unwrap();
    // Without a reason code it is Success
    puback.get(2).copied().unwrap_or(0x00)
}

#[tokio::test]
async fn retained_messages_beyond_the_limit_are_refused() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut config = ServerConfig::default();
    config.limits.max_retained_messages = Some(1);
    // The broker's own retained $SYS topics would count too
    config.sys_interval = None;
    tokio::spawn(mercurio_server::server::run_with_config(
        listener,
        config,
        std::future::pending::<()>(),
    ));
    let (mut stream, _) = connect(addr, b'r').await;

    // Success, or No Matching Subscribers, also a success
    let accepted = |reason: u8| reason == 0x00 || reason == 0x10;
    assert!(accepted(publish_retained(&mut stream, "a").await));
    // Quota exceeded on a second topic
    assert_eq!(publish_retained(&mut stream, "b").await, 0x97);
    // Updating the retained topic is still allowed
    assert!(accepted(publish_retained(&mut stream, "a").await));
}

#[tokio::test]
async fn an_address_holds_its_slots_through_the_tls_handshake() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut config = ServerConfig::default();
    config.tls = Some(
        mercurio_server::tls::Tls::new(mercurio_server::tls::TlsConfig::new(
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/cert.pem"),
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/key.pem"),
        ))
        .unwrap(),
    );
    config.limits.max_connections_per_ip = Some(1);
    config.limits.connect_timeout = Duration::from_secs(30);
    tokio::spawn(mercurio_server::server::run_with_config(
        listener,
        config,
        std::future::pending::<()>(),
    ));

    // Neither starts its TLS handshake: the first takes the address's
    // only slot, the second is closed at once
    let _first = TcpStream::connect(addr).await.unwrap();
    sleep(Duration::from_millis(50)).await;
    let mut second = TcpStream::connect(addr).await.unwrap();
    assert!(read_until_closed(&mut second).await.is_empty());
}
