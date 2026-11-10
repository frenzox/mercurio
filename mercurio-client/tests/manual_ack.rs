use std::time::Duration;

use mercurio_client::{AckToken, ConnectOptions, DisconnectReason, Event, MqttClient};
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

/// Asserts the client sends nothing for a while.
async fn silent(socket: &mut TcpStream) {
    let mut early = [0u8; 1];
    assert!(
        timeout(Duration::from_millis(300), socket.read(&mut early))
            .await
            .is_err(),
        "the client sent {early:02x?} early"
    );
}

/// An MQTT 5 PUBLISH on `t` with packet identifier `id`.
fn publish(qos: u8, id: u8, dup: bool) -> [u8; 9] {
    let flags = (qos << 1) | if dup { 0x08 } else { 0 };
    [0x30 | flags, 0x07, 0x00, 0x01, b't', 0x00, id, 0x00, b'x']
}

async fn accept(listener: &TcpListener, session_present: bool) -> TcpStream {
    let (mut socket, _) = listener.accept().await.unwrap();
    assert_eq!(read_packet(&mut socket).await[0], 0x10);
    socket
        .write_all(&[0x20, 0x03, session_present as u8, 0x00, 0x00])
        .await
        .unwrap();
    socket
}

/// Options for a client of the broker behind `listener` that acknowledges
/// by hand.
fn manual_client(listener: &TcpListener) -> ConnectOptions {
    ConnectOptions::new("127.0.0.1", listener.local_addr().unwrap().port())
        .client_id("c")
        .keep_alive(0)
        .manual_acks(true)
}

async fn next_message(client: &MqttClient) -> Option<AckToken> {
    loop {
        match timeout(Duration::from_secs(3), client.recv()).await {
            Ok(Some(Event::Message { ack, .. })) => return ack,
            Ok(Some(_)) => {}
            other => panic!("no message: {other:?}"),
        }
    }
}

#[tokio::test]
async fn messages_are_acknowledged_when_the_application_says() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let options = manual_client(&listener);
    let (tokens_tx, mut tokens) = tokio::sync::mpsc::channel(4);
    let client = tokio::spawn(async move {
        let client = MqttClient::connect(options).await.unwrap();
        for _ in 0..2 {
            tokens_tx.send(next_message(&client).await).await.unwrap();
        }
        client
    });

    let mut socket = accept(&listener, false).await;
    socket.write_all(&publish(1, 1, false)).await.unwrap();
    socket.write_all(&publish(2, 2, false)).await.unwrap();
    let qos1 = tokens.recv().await.unwrap().expect("QoS 1 carries a token");
    let qos2 = tokens.recv().await.unwrap().expect("QoS 2 carries a token");
    let client = client.await.unwrap();
    silent(&mut socket).await;

    client.ack(qos1).await.unwrap();
    assert_eq!(
        read_packet(&mut socket).await[..4],
        [0x40, 0x02, 0x00, 0x01]
    );
    client.ack(qos2).await.unwrap();
    assert_eq!(
        read_packet(&mut socket).await[..4],
        [0x50, 0x02, 0x00, 0x02]
    );
    socket.write_all(&[0x62, 0x02, 0x00, 0x02]).await.unwrap();
    assert_eq!(
        read_packet(&mut socket).await[..4],
        [0x70, 0x02, 0x00, 0x02]
    );
    // Acknowledging twice does nothing
    client.ack(qos1).await.unwrap();
    silent(&mut socket).await;
}

#[tokio::test]
async fn acknowledgements_go_out_in_the_order_messages_came() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = MqttClient::connect(manual_client(&listener));
    let (client, mut socket) = tokio::join!(client, accept(&listener, false));
    let client = client.unwrap();
    socket.write_all(&publish(1, 1, false)).await.unwrap();
    socket.write_all(&publish(1, 2, false)).await.unwrap();
    let first = next_message(&client).await.unwrap();
    let second = next_message(&client).await.unwrap();

    client.ack(second).await.unwrap();
    silent(&mut socket).await;
    client.ack(first).await.unwrap();
    assert_eq!(
        read_packet(&mut socket).await[..4],
        [0x40, 0x02, 0x00, 0x01]
    );
    assert_eq!(
        read_packet(&mut socket).await[..4],
        [0x40, 0x02, 0x00, 0x02]
    );
}

#[tokio::test]
async fn unacknowledged_messages_count_against_receive_maximum() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = MqttClient::connect(manual_client(&listener).receive_maximum(2));
    let (client, mut socket) = tokio::join!(client, accept(&listener, false));
    let client = client.unwrap();
    socket.write_all(&publish(1, 1, false)).await.unwrap();
    socket.write_all(&publish(1, 2, false)).await.unwrap();
    let first = next_message(&client).await.unwrap();
    next_message(&client).await.unwrap();
    // A slot frees up once a message is acknowledged
    client.ack(first).await.unwrap();
    assert_eq!(
        read_packet(&mut socket).await[..4],
        [0x40, 0x02, 0x00, 0x01]
    );
    socket.write_all(&publish(1, 3, false)).await.unwrap();
    next_message(&client).await.unwrap();

    // Over the limit
    socket.write_all(&publish(1, 4, false)).await.unwrap();
    assert_eq!(read_packet(&mut socket).await[..3], [0xe0, 0x01, 0x93]);
    let event = timeout(Duration::from_secs(3), client.recv())
        .await
        .unwrap();
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
async fn messages_unacknowledged_at_a_reconnect_come_again_with_a_new_token() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let options = manual_client(&listener)
        .clean_start(false)
        .reconnect(Duration::from_millis(20));
    let client = MqttClient::connect(options);
    let (client, mut socket) = tokio::join!(client, accept(&listener, false));
    let client = client.unwrap();
    socket.write_all(&publish(1, 1, false)).await.unwrap();
    let stale = next_message(&client).await.unwrap();
    drop(socket);

    let mut socket = accept(&listener, true).await;
    socket.write_all(&publish(1, 1, true)).await.unwrap();
    let fresh = next_message(&client).await.unwrap();
    assert_ne!(stale, fresh);
    client.ack(stale).await.unwrap();
    silent(&mut socket).await;
    client.ack(fresh).await.unwrap();
    assert_eq!(
        read_packet(&mut socket).await[..4],
        [0x40, 0x02, 0x00, 0x01]
    );
}

#[tokio::test]
async fn without_manual_acks_messages_carry_no_token() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let options =
        ConnectOptions::new("127.0.0.1", listener.local_addr().unwrap().port()).keep_alive(0);
    let client = MqttClient::connect(options);
    let (client, mut socket) = tokio::join!(client, accept(&listener, false));
    let client = client.unwrap();
    socket.write_all(&publish(1, 1, false)).await.unwrap();
    assert_eq!(
        read_packet(&mut socket).await[..4],
        [0x40, 0x02, 0x00, 0x01]
    );
    assert_eq!(next_message(&client).await, None);
}
