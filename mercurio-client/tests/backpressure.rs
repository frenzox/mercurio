mod common;

use std::{net::SocketAddr, time::Duration};

use mercurio_client::{ConnectOptions, Event, MqttClient, QoS};
use mercurio_server::server::ServerConfig;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    time::timeout,
};

/// More than the client's event queue holds.
const BACKLOG: usize = 300;

/// What the client's event queue holds.
const QUEUE: usize = 256;

async fn start_broker() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(
        common::broker(listener, ServerConfig::default()).run(std::future::pending::<()>()),
    );
    addr
}

async fn connect(addr: SocketAddr, id: &str, keep_alive: u16) -> MqttClient {
    MqttClient::connect(
        ConnectOptions::new("127.0.0.1", addr.port())
            .client_id(id)
            .keep_alive(keep_alive),
    )
    .await
    .unwrap()
}

/// Has `BACKLOG` messages published on `topic`, numbered from 0.
async fn flood(addr: SocketAddr, topic: &str) {
    let publisher = connect(addr, "publisher", 60).await;
    for i in 0..BACKLOG {
        publisher
            .publish(topic, i.to_string(), QoS::AtLeastOnce)
            .await
            .unwrap();
    }
    publisher.disconnect().await.unwrap();
}

#[tokio::test]
async fn replying_from_the_receive_loop_does_not_deadlock() {
    let addr = start_broker().await;
    let worker = connect(addr, "worker", 60).await;
    worker
        .subscribe(&[("requests", QoS::AtLeastOnce)])
        .await
        .unwrap();
    flood(addr, "requests").await;

    timeout(Duration::from_secs(10), async {
        for _ in 0..BACKLOG {
            let Some(Event::Message { payload, .. }) = worker.recv().await else {
                panic!("expected a message");
            };
            worker
                .publish("replies", payload, QoS::AtLeastOnce)
                .await
                .unwrap();
        }
    })
    .await
    .expect("the client deadlocked");
}

#[tokio::test]
async fn a_slow_reader_keeps_its_connection_and_its_messages() {
    let addr = start_broker().await;
    // The broker gives up on it after 1.5 s without a packet
    let reader = connect(addr, "reader", 1).await;
    reader.subscribe(&[("t", QoS::AtLeastOnce)]).await.unwrap();
    flood(addr, "t").await;

    tokio::time::sleep(Duration::from_secs(3)).await;
    timeout(
        Duration::from_secs(2),
        reader.publish("other", "x", QoS::AtLeastOnce),
    )
    .await
    .expect("the client stopped serving requests")
    .unwrap();

    for i in 0..BACKLOG {
        let event = timeout(Duration::from_secs(2), reader.recv())
            .await
            .unwrap();
        let Some(Event::Message { payload, .. }) = event else {
            panic!("expected message {i}, got {event:?}");
        };
        assert_eq!(payload, i.to_string());
    }
    assert!(
        timeout(Duration::from_millis(500), reader.recv())
            .await
            .is_err(),
        "the connection did not survive"
    );
}

/// QoS 0 PUBLISH of `payload` on topic `t`, MQTT 5.
fn publish_packet(payload: &[u8]) -> Vec<u8> {
    let mut packet = vec![0x30, 4 + payload.len() as u8, 0x00, 0x01, b't', 0x00];
    packet.extend_from_slice(payload);
    packet
}

#[tokio::test]
async fn reconnecting_does_not_wait_for_the_reader() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let broker = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut connect = [0u8; 256];
            let _ = socket.read(&mut connect).await.unwrap();
            socket
                .write_all(&[0x20, 0x03, 0x00, 0x00, 0x00])
                .await
                .unwrap();
            // Fills the event queue; then the end of the connection
            // needs a place too
            let backlog: Vec<u8> = (0..QUEUE)
                .flat_map(|i| publish_packet(i.to_string().as_bytes()))
                .collect();
            socket.write_all(&backlog).await.unwrap();
        }
        listener
    });

    let client = MqttClient::connect(
        ConnectOptions::new("127.0.0.1", port)
            .keep_alive(0)
            .reconnect(Duration::from_millis(50)),
    )
    .await
    .unwrap();
    let _listener = timeout(Duration::from_secs(3), broker)
        .await
        .expect("the client did not reconnect")
        .unwrap();

    let mut events = Vec::new();
    while let Ok(Some(event)) = timeout(Duration::from_millis(500), client.recv()).await {
        events.push(event);
    }
    assert!(matches!(events[QUEUE], Event::Disconnected { .. }));
    assert!(matches!(events[QUEUE + 1], Event::Reconnected { .. }));
    let messages = events
        .iter()
        .filter(|e| matches!(e, Event::Message { .. }))
        .count();
    assert_eq!(messages, 2 * QUEUE);
}

#[tokio::test]
async fn a_full_queue_stops_reading_from_the_broker() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let broker = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut connect = [0u8; 256];
        let _ = socket.read(&mut connect).await.unwrap();
        socket
            .write_all(&[0x20, 0x03, 0x00, 0x00, 0x00])
            .await
            .unwrap();
        // 64 MiB of 1 KiB messages, far more than socket buffers hold
        let mut message = vec![0x30, 0xec, 0x07, 0x00, 0x01, b't', 0x00];
        message.resize(message.len() + 1000, b'x');
        let backlog = message.repeat(64 * 1024);
        timeout(Duration::from_secs(2), socket.write_all(&backlog))
            .await
            .is_err()
    });

    let _client = MqttClient::connect(ConnectOptions::new("127.0.0.1", port).keep_alive(0))
        .await
        .unwrap();
    assert!(
        broker.await.unwrap(),
        "the client buffered everything the broker sent"
    );
}
