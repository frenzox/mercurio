use std::time::Duration;

use mercurio_client::{ClientError, ConnectOptions, MqttClient, QoS};
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
