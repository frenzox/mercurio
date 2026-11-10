use std::time::Duration;

use mercurio_client::{ClientError, ConnectOptions, DisconnectReason, Event, MqttClient, QoS};
use mercurio_core::reason::ReasonCode;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

/// CONNACK announcing Receive Maximum 1, Maximum QoS 1, no retained
/// messages, Maximum Packet Size 64 and no wildcard subscriptions.
const LIMITED_CONNACK: [u8; 19] = [
    0x20, 0x11, 0x00, 0x00, 0x0e, 0x21, 0x00, 0x01, 0x24, 0x01, 0x25, 0x00, 0x27, 0x00, 0x00, 0x00,
    0x40, 0x28, 0x00,
];

/// Reads one packet whose remaining length fits in a byte.
async fn read_packet(socket: &mut TcpStream) -> Vec<u8> {
    let mut header = [0u8; 2];
    socket.read_exact(&mut header).await.unwrap();
    let mut body = vec![0u8; header[1] as usize];
    socket.read_exact(&mut body).await.unwrap();
    [header.to_vec(), body].concat()
}

async fn limited_broker() -> (u16, TcpListener) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    (listener.local_addr().unwrap().port(), listener)
}

async fn accept(listener: &TcpListener) -> TcpStream {
    let (mut socket, _) = listener.accept().await.unwrap();
    read_packet(&mut socket).await;
    socket.write_all(&LIMITED_CONNACK).await.unwrap();
    socket
}

#[tokio::test]
async fn publishes_wait_for_the_receive_maximum_window() {
    let (port, listener) = limited_broker().await;
    let broker = tokio::spawn(async move {
        let mut socket = accept(&listener).await;
        let first = read_packet(&mut socket).await;
        assert_eq!(first[0], 0x32);
        // The second publish must wait for the first to be acknowledged
        let mut early = [0u8; 1];
        assert!(
            timeout(Duration::from_millis(300), socket.read(&mut early))
                .await
                .is_err(),
            "second publish sent beyond Receive Maximum"
        );
        let id = [first[5], first[6]];
        socket.write_all(&[0x40, 0x02, id[0], id[1]]).await.unwrap();

        let second = read_packet(&mut socket).await;
        assert_eq!(second[0], 0x32);
        socket
            .write_all(&[0x40, 0x02, second[5], second[6]])
            .await
            .unwrap();
        let mut sink = [0u8; 64];
        while socket.read(&mut sink).await.unwrap_or(0) > 0 {}
    });

    let client = MqttClient::connect(ConnectOptions::new("127.0.0.1", port))
        .await
        .unwrap();
    let (a, b) = timeout(Duration::from_secs(3), async {
        tokio::join!(
            client.publish("a", "1", QoS::AtLeastOnce),
            client.publish("a", "2", QoS::AtLeastOnce)
        )
    })
    .await
    .expect("publishes never completed");
    a.unwrap();
    b.unwrap();
    client.disconnect().await.unwrap();
    broker.await.unwrap();
}

#[tokio::test]
async fn a_session_present_after_clean_start_is_refused() {
    let (port, listener) = limited_broker().await;
    let broker = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_packet(&mut socket).await;
        socket
            .write_all(&[0x20, 0x03, 0x01, 0x00, 0x00])
            .await
            .unwrap();
        let mut sent = Vec::new();
        socket.read_to_end(&mut sent).await.unwrap();
        sent
    });

    let connected =
        MqttClient::connect(ConnectOptions::new("127.0.0.1", port).clean_start(true)).await;
    assert!(
        matches!(
            connected,
            Err(ClientError::BrokerViolation(ReasonCode::ProtocolError))
        ),
        "{:?}",
        connected.err()
    );
    let sent = timeout(Duration::from_secs(2), broker)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(sent[..3], [0xe0, 0x01, 0x82], "sent {sent:02x?}");
}

#[tokio::test]
async fn unsupported_features_are_refused_locally() {
    let (port, listener) = limited_broker().await;
    let broker = tokio::spawn(async move {
        let mut socket = accept(&listener).await;
        let mut sent = Vec::new();
        socket.read_to_end(&mut sent).await.unwrap();
        sent
    });

    let client = MqttClient::connect(ConnectOptions::new("127.0.0.1", port).keep_alive(0))
        .await
        .unwrap();
    assert!(matches!(
        client.publish("a", "x", QoS::ExactlyOnce).await,
        Err(ClientError::NotSupported(_))
    ));
    assert!(matches!(
        client
            .publish_with_retain("a", "x", QoS::AtMostOnce, true)
            .await,
        Err(ClientError::NotSupported(_))
    ));
    assert!(matches!(
        client.publish("a", vec![0u8; 100], QoS::AtMostOnce).await,
        Err(ClientError::PacketTooLarge)
    ));
    assert!(matches!(
        client.subscribe(&[("a/+", QoS::AtMostOnce)]).await,
        Err(ClientError::NotSupported(_))
    ));
    client.disconnect().await.unwrap();

    // Nothing the client tried reached the broker
    let sent = timeout(Duration::from_secs(2), broker)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(sent[0], 0xe0, "sent {sent:02x?}");
    assert_eq!(sent.len(), 2 + sent[1] as usize, "sent {sent:02x?}");
}

/// Announces a packet of `remaining` bytes but sends only its start, and
/// returns what the client sent: CONNECT, then its answer.
async fn announce_oversized_packet(
    options: impl FnOnce(ConnectOptions) -> ConnectOptions,
    remaining: [u8; 4],
) -> (Vec<u8>, Vec<u8>, Option<Event>) {
    let (port, listener) = limited_broker().await;
    let broker = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let connect = read_packet(&mut socket).await;
        socket
            .write_all(&[0x20, 0x03, 0x00, 0x00, 0x00])
            .await
            .unwrap();
        let mut start = vec![0x30];
        start.extend_from_slice(&remaining);
        start.extend_from_slice(&[0x00, 0x01, b't', 0x00]);
        start.resize(start.len() + 100, b'x');
        socket.write_all(&start).await.unwrap();
        let mut answer = Vec::new();
        timeout(Duration::from_secs(2), socket.read_to_end(&mut answer))
            .await
            .expect("the client waited for the rest of the packet")
            .unwrap();
        (connect, answer)
    });

    let client = MqttClient::connect(options(
        ConnectOptions::new("127.0.0.1", port).keep_alive(0),
    ))
    .await
    .unwrap();
    let event = timeout(Duration::from_secs(2), client.recv())
        .await
        .unwrap_or_default();
    let (connect, answer) = broker.await.unwrap();
    (connect, answer, event)
}

fn refused_as_too_large(answer: &[u8], event: Option<Event>) {
    assert_eq!(answer, [0xe0, 0x01, 0x95], "{answer:02x?}");
    assert!(
        matches!(
            event,
            Some(Event::Disconnected {
                reason: DisconnectReason::ProtocolError(_)
            })
        ),
        "{event:?}"
    );
}

#[tokio::test]
async fn packets_over_the_default_limit_are_refused_from_their_header() {
    // 2 MiB, over the 1 MiB default
    let (connect, answer, event) = announce_oversized_packet(|o| o, [0x80, 0x80, 0x80, 0x01]).await;
    // Maximum Packet Size of 1 MiB, announced in CONNECT
    assert!(
        connect
            .windows(5)
            .any(|w| w == [0x27, 0x00, 0x10, 0x00, 0x00]),
        "{connect:02x?}"
    );
    refused_as_too_large(&answer, event);
}

#[tokio::test]
async fn packets_over_the_set_limit_are_refused_from_their_header() {
    // 1,000 bytes, over the 500 set
    let (_, answer, event) =
        announce_oversized_packet(|o| o.maximum_packet_size(500), [0xe8, 0x07, 0x00, 0x00]).await;
    refused_as_too_large(&answer, event);
}

#[tokio::test]
async fn started_publishes_go_out_in_order_without_waiting_for_each_other() {
    let (port, listener) = limited_broker().await;
    let broker = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_packet(&mut socket).await;
        socket
            .write_all(&[0x20, 0x03, 0x00, 0x00, 0x00])
            .await
            .unwrap();
        let mut ids = Vec::new();
        for payload in *b"123" {
            let publish = read_packet(&mut socket).await;
            assert_eq!(publish[0], 0x32);
            assert_eq!(*publish.last().unwrap(), payload, "{publish:02x?}");
            ids.push([publish[5], publish[6]]);
        }
        for id in ids.iter().rev() {
            socket.write_all(&[0x40, 0x02, id[0], id[1]]).await.unwrap();
        }
        socket
    });

    let client = MqttClient::connect(ConnectOptions::new("127.0.0.1", port).keep_alive(0))
        .await
        .unwrap();
    let mut pending = Vec::new();
    for payload in ["1", "2", "3"] {
        pending.push(
            client
                .start_publish("a", payload, QoS::AtLeastOnce, false, Default::default())
                .await
                .unwrap(),
        );
    }
    for publish in pending {
        timeout(Duration::from_secs(3), publish)
            .await
            .expect("a started publish never completed")
            .unwrap();
    }
    let _socket = broker.await.unwrap();
}
