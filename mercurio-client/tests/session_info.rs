use std::time::Duration;

use mercurio_client::{ConnectOptions, Event, MqttClient, QoS};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

/// Reads one packet whose remaining length fits in a byte.
async fn read_packet(socket: &mut TcpStream) -> Vec<u8> {
    let mut header = [0u8; 2];
    socket.read_exact(&mut header).await.unwrap();
    let mut body = vec![0u8; header[1] as usize];
    socket.read_exact(&mut body).await.unwrap();
    [header.to_vec(), body].concat()
}

/// An MQTT 5 CONNACK accepting the client, with these properties.
fn connack(session_present: bool, properties: &[u8]) -> Vec<u8> {
    let mut packet = vec![
        0x20,
        3 + properties.len() as u8,
        session_present as u8,
        0x00,
    ];
    packet.push(properties.len() as u8);
    packet.extend_from_slice(properties);
    packet
}

/// Accepts a connection, answers its CONNECT with `connack` and returns
/// the CONNECT and the connection.
async fn accept(listener: &TcpListener, connack: &[u8]) -> (Vec<u8>, TcpStream) {
    let (mut socket, _) = listener.accept().await.unwrap();
    let connect = read_packet(&mut socket).await;
    assert_eq!(connect[0], 0x10);
    socket.write_all(connack).await.unwrap();
    (connect, socket)
}

const FIRST_PROPERTIES: &[u8] = &[
    // Assigned Client Identifier "auto-1"
    0x12, 0x00, 0x06, b'a', b'u', b't', b'o', b'-', b'1', //
    // Server Keep Alive 30
    0x13, 0x00, 0x1e, //
    // Maximum QoS 1
    0x24, 0x01, //
    // Retain Available 0
    0x25, 0x00, //
    // Receive Maximum 5
    0x21, 0x00, 0x05, //
    // Maximum Packet Size 1000
    0x27, 0x00, 0x00, 0x03, 0xe8, //
    // Topic Alias Maximum 3
    0x22, 0x00, 0x03, //
    // Session Expiry Interval 60
    0x11, 0x00, 0x00, 0x00, 0x3c, //
    // Response Information "resp/"
    0x1a, 0x00, 0x05, b'r', b'e', b's', b'p', b'/',
];

#[tokio::test]
async fn the_negotiated_values_are_kept_and_refreshed_on_reconnecting() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let broker = tokio::spawn(async move {
        let (_, first) = accept(&listener, &connack(false, FIRST_PROPERTIES)).await;
        drop(first);
        // Nothing announced: every value takes its default
        let (connect, socket) = accept(&listener, &connack(true, &[])).await;
        (connect, socket)
    });

    let client = MqttClient::connect(
        ConnectOptions::new("127.0.0.1", port)
            .keep_alive(100)
            .session_expiry(120)
            .clean_start(false)
            .request_response_information(true)
            .reconnect(Duration::from_millis(50)),
    )
    .await
    .unwrap();
    assert_eq!(client.client_id(), "auto-1");
    let first = client.connack();
    assert!(!first.session_present);
    assert_eq!(first.keep_alive, 30);
    assert_eq!(first.maximum_qos, QoS::AtLeastOnce);
    assert!(!first.retain_available);
    assert_eq!(first.receive_maximum, 5);
    assert_eq!(first.maximum_packet_size, Some(1000));
    assert_eq!(first.topic_alias_maximum, 3);
    assert_eq!(first.session_expiry, 60);
    assert_eq!(first.response_information.as_deref(), Some("resp/"));
    assert_eq!(client.response_information().as_deref(), Some("resp/"));

    loop {
        let event = timeout(Duration::from_secs(3), client.recv())
            .await
            .unwrap()
            .unwrap();
        if matches!(event, Event::Reconnected { .. }) {
            break;
        }
    }
    let second = client.connack();
    assert!(second.session_present);
    assert_eq!(second.keep_alive, 100);
    assert_eq!(second.maximum_qos, QoS::ExactlyOnce);
    assert!(second.retain_available);
    assert_eq!(second.receive_maximum, u16::MAX);
    assert_eq!(second.maximum_packet_size, None);
    assert_eq!(second.topic_alias_maximum, 0);
    assert_eq!(second.session_expiry, 120);
    assert_eq!(second.response_information, None);
    assert_eq!(client.client_id(), "auto-1");

    // The reconnection presented the assigned id
    let (connect, _socket) = broker.await.unwrap();
    assert!(
        connect.windows(6).any(|w| w == b"auto-1"),
        "CONNECT {connect:02x?}"
    );
}
