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
    let config = ServerConfig {
        limits,
        ..Default::default()
    };
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
    let addr = start_broker(Limits {
        max_packet_size: 64,
        ..Default::default()
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
    let addr = start_broker(Limits {
        max_connections: Some(1),
        ..Default::default()
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
    let addr = start_broker(Limits {
        max_keepalive: Some(1),
        ..Default::default()
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
    let config = ServerConfig {
        policy: mercurio_server::server::ConnectPolicy {
            response_information: Some("responses/%c".into()),
            ..Default::default()
        },
        ..Default::default()
    };
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
    let config = ServerConfig {
        policy: mercurio_server::server::ConnectPolicy {
            redirect: Some(mercurio_server::server::Redirect {
                server_reference: "other:1883".into(),
                permanent: true,
            }),
            ..Default::default()
        },
        ..Default::default()
    };
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
    let addr = start_broker(Limits {
        maximum_qos: mercurio_core::qos::QoS::AtLeastOnce,
        ..Default::default()
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
    let addr = start_broker(Limits {
        retain_available: false,
        ..Default::default()
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
    let addr = start_broker(Limits {
        persistent_client_expiration: Some(Duration::from_secs(1)),
        ..Default::default()
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
    let addr = start_broker(Limits {
        max_subscriptions: Some(2),
        ..Default::default()
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
