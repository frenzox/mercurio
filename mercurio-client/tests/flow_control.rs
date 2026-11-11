mod common;

use std::{net::SocketAddr, time::Duration};

use mercurio_client::{ConnectOptions, DisconnectReason, Event, MqttClient, QoS};
use mercurio_server::server::ServerConfig;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

async fn start_broker() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut config = ServerConfig::default();
    config.limits.send_topic_aliases = true;
    config.policy.response_information = Some("replies/%c/".into());
    tokio::spawn(common::broker(listener, config).run(std::future::pending::<()>()));
    addr
}

fn options(addr: SocketAddr, id: &str) -> ConnectOptions {
    ConnectOptions::new("127.0.0.1", addr.port()).client_id(id)
}

async fn next_message(client: &MqttClient) -> Option<(String, Vec<u8>)> {
    match timeout(Duration::from_millis(500), client.recv()).await {
        Ok(Some(Event::Message { topic, payload, .. })) => Some((topic, payload.to_vec())),
        _ => None,
    }
}

#[tokio::test]
async fn aliased_topics_are_resolved() {
    let addr = start_broker().await;
    let subscriber = MqttClient::connect(options(addr, "sub").topic_alias_maximum(4))
        .await
        .unwrap();
    subscriber
        .subscribe(&[("t", QoS::AtMostOnce)])
        .await
        .unwrap();
    let publisher = MqttClient::connect(options(addr, "pub")).await.unwrap();

    // The broker sets the alias with the first message and uses it alone
    // for the second
    for payload in ["1", "2"] {
        publisher
            .publish("t", payload, QoS::AtLeastOnce)
            .await
            .unwrap();
        assert_eq!(
            next_message(&subscriber).await,
            Some(("t".into(), payload.into()))
        );
    }
}

#[tokio::test]
async fn broker_drops_messages_larger_than_the_client_accepts() {
    let addr = start_broker().await;
    let subscriber = MqttClient::connect(options(addr, "sub").maximum_packet_size(64))
        .await
        .unwrap();
    subscriber
        .subscribe(&[("t", QoS::AtMostOnce)])
        .await
        .unwrap();
    let publisher = MqttClient::connect(options(addr, "pub")).await.unwrap();

    publisher
        .publish("t", vec![b'x'; 100], QoS::AtLeastOnce)
        .await
        .unwrap();
    publisher
        .publish("t", "small", QoS::AtLeastOnce)
        .await
        .unwrap();
    assert_eq!(
        next_message(&subscriber).await,
        Some(("t".into(), b"small".to_vec()))
    );
}

#[tokio::test]
async fn response_information_is_requested_on_demand() {
    let addr = start_broker().await;
    let asking = MqttClient::connect(options(addr, "a").request_response_information(true))
        .await
        .unwrap();
    assert_eq!(asking.response_information().as_deref(), Some("replies/a/"));

    let not_asking = MqttClient::connect(options(addr, "b")).await.unwrap();
    assert_eq!(not_asking.response_information(), None);
}

/// Reads one packet whose remaining length fits in a byte.
async fn read_packet(socket: &mut TcpStream) -> Vec<u8> {
    let mut header = [0u8; 2];
    socket.read_exact(&mut header).await.unwrap();
    let mut body = vec![0u8; header[1] as usize];
    socket.read_exact(&mut body).await.unwrap();
    [header.to_vec(), body].concat()
}

/// Connects a client with the given options to a fake broker that sends
/// `packets`, and returns the CONNECT, the client's last packet and the
/// reason it gives.
async fn misbehaving_broker(
    options: impl FnOnce(ConnectOptions) -> ConnectOptions,
    packets: &'static [u8],
) -> (Vec<u8>, Vec<u8>, DisconnectReason) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let broker = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let connect = read_packet(&mut socket).await;
        socket
            .write_all(&[0x20, 0x03, 0x00, 0x00, 0x00])
            .await
            .unwrap();
        socket.write_all(packets).await.unwrap();
        // Skip the acknowledgements; the DISCONNECT comes last
        let mut sent = Vec::new();
        socket.read_to_end(&mut sent).await.unwrap();
        (connect, sent)
    });

    let client = MqttClient::connect(options(
        ConnectOptions::new("127.0.0.1", port).keep_alive(0),
    ))
    .await
    .unwrap();
    let reason = loop {
        match timeout(Duration::from_secs(2), client.recv())
            .await
            .unwrap()
        {
            Some(Event::Disconnected { reason }) => break reason,
            Some(_) => continue,
            None => panic!("client stayed connected"),
        }
    };
    drop(client);
    let (connect, sent) = timeout(Duration::from_secs(2), broker)
        .await
        .unwrap()
        .unwrap();
    let disconnect = sent[sent.len() - 3..].to_vec();
    (connect, disconnect, reason)
}

#[tokio::test]
async fn unrequested_topic_alias_ends_the_connection() {
    // PUBLISH QoS 0 on "t" with Topic Alias 1
    let (_, disconnect, reason) = misbehaving_broker(
        |o| o,
        &[0x30, 0x08, 0x00, 0x01, b't', 0x03, 0x23, 0x00, 0x01, b'x'],
    )
    .await;
    assert_eq!(disconnect, [0xe0, 0x01, 0x94]);
    assert!(matches!(reason, DisconnectReason::ProtocolError(_)));
}

#[tokio::test]
async fn unknown_topic_alias_ends_the_connection() {
    // PUBLISH QoS 0 with an empty topic and Topic Alias 1, never set
    let (_, disconnect, _) = misbehaving_broker(
        |o| o.topic_alias_maximum(4),
        &[0x30, 0x07, 0x00, 0x00, 0x03, 0x23, 0x00, 0x01, b'x'],
    )
    .await;
    assert_eq!(disconnect, [0xe0, 0x01, 0x82]);
}

#[tokio::test]
async fn exceeding_receive_maximum_ends_the_connection() {
    // Two QoS 2 PUBLISHes on "t" with no PUBREL in between
    let (connect, disconnect, _) = misbehaving_broker(
        |o| o.receive_maximum(1),
        &[
            0x34, 0x07, 0x00, 0x01, b't', 0x00, 0x01, 0x00, b'x', 0x34, 0x07, 0x00, 0x01, b't',
            0x00, 0x02, 0x00, b'y',
        ],
    )
    .await;
    assert!(connect.windows(3).any(|w| w == [0x21, 0x00, 0x01]));
    assert_eq!(disconnect, [0xe0, 0x01, 0x93]);
}

#[tokio::test]
async fn oversized_packet_ends_the_connection() {
    let (connect, disconnect, _) = misbehaving_broker(
        |o| o.maximum_packet_size(16).request_problem_information(false),
        &[
            0x30, 0x14, 0x00, 0x01, b't', 0x00, b'0', b'1', b'2', b'3', b'4', b'5', b'6', b'7',
            b'8', b'9', b'a', b'b', b'c', b'd', b'e', b'f',
        ],
    )
    .await;
    assert!(connect
        .windows(5)
        .any(|w| w == [0x27, 0x00, 0x00, 0x00, 0x10]));
    assert!(connect.windows(2).any(|w| w == [0x17, 0x00]));
    assert_eq!(disconnect, [0xe0, 0x01, 0x95]);
}
