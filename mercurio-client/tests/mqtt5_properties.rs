use std::{net::SocketAddr, time::Duration};

use bytes::Bytes;
use mercurio_client::{
    ConnectOptions, Event, MessageProperties, MqttClient, QoS, SubscriptionOptions, Will,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    time::timeout,
};

async fn start_broker() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(mercurio_server::server::run(
        listener,
        std::future::pending::<()>(),
    ));
    addr
}

fn options(addr: SocketAddr, id: &str) -> ConnectOptions {
    ConnectOptions::new("127.0.0.1", addr.port()).client_id(id)
}

async fn next_event(client: &MqttClient) -> Option<Event> {
    timeout(Duration::from_millis(500), client.recv())
        .await
        .ok()
        .flatten()
}

#[tokio::test]
async fn message_properties_and_subscription_ids_reach_the_subscriber() {
    let addr = start_broker().await;
    let subscriber = MqttClient::connect(options(addr, "sub")).await.unwrap();
    let qos1 = SubscriptionOptions {
        qos: QoS::AtLeastOnce,
        ..Default::default()
    };
    subscriber
        .subscribe_with(&[("req", qos1)], Some(7))
        .await
        .unwrap();

    let properties = MessageProperties::default()
        .payload_is_utf8(true)
        .message_expiry(60)
        .content_type("text/plain")
        .response_topic("reply")
        .correlation_data(Bytes::from_static(b"42"))
        .user_property("k", "v");
    let publisher = MqttClient::connect(options(addr, "pub")).await.unwrap();
    publisher
        .publish_with_properties("req", "hi", QoS::AtLeastOnce, false, properties.clone())
        .await
        .unwrap();

    let Some(Event::Message {
        properties: received,
        subscription_ids,
        ..
    }) = next_event(&subscriber).await
    else {
        panic!("no message");
    };
    assert_eq!(subscription_ids, [7]);
    // The broker counts the expiry down while the message waits
    assert!(received.message_expiry.is_some_and(|e| e <= 60));
    let mut expected = received.clone();
    expected.message_expiry = Some(60);
    assert_eq!(expected, properties);
}

#[tokio::test]
async fn no_local_subscriptions_skip_own_messages() {
    let addr = start_broker().await;
    let client = MqttClient::connect(options(addr, "c")).await.unwrap();
    let no_local = SubscriptionOptions {
        no_local: true,
        ..Default::default()
    };
    client
        .subscribe_with(&[("t", no_local)], None)
        .await
        .unwrap();
    client.publish("t", "mine", QoS::AtMostOnce).await.unwrap();

    assert!(next_event(&client).await.is_none());
}

#[tokio::test]
async fn session_expiry_keeps_the_session_across_connections() {
    let addr = start_broker().await;
    let persistent = || options(addr, "p").clean_start(false).session_expiry(60);

    let client = MqttClient::connect(persistent()).await.unwrap();
    client.subscribe(&[("t", QoS::AtLeastOnce)]).await.unwrap();
    client.disconnect().await.unwrap();

    let publisher = MqttClient::connect(options(addr, "pub")).await.unwrap();
    publisher
        .publish("t", "queued", QoS::AtLeastOnce)
        .await
        .unwrap();

    let client = MqttClient::connect(persistent()).await.unwrap();
    let Some(Event::Message { payload, .. }) = next_event(&client).await else {
        panic!("session not kept");
    };
    assert_eq!(&payload[..], b"queued");
}

#[tokio::test]
async fn will_delay_and_properties_are_sent() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let broker = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut connect = vec![0u8; 256];
        let n = socket.read(&mut connect).await.unwrap();
        socket
            .write_all(&[0x20, 0x03, 0x00, 0x00, 0x00])
            .await
            .unwrap();
        connect.truncate(n);
        connect
    });

    let will = Will::new("w", "bye")
        .delay(5)
        .properties(MessageProperties::default().content_type("text/plain"));
    let _client = MqttClient::connect(
        ConnectOptions::new("127.0.0.1", port)
            .client_id("c")
            .will(will),
    )
    .await
    .unwrap();

    let connect = broker.await.unwrap();
    let contains = |needle: &[u8]| connect.windows(needle.len()).any(|w| w == needle);
    // Will Delay Interval of 5 seconds, then the content type
    assert!(contains(&[0x18, 0x00, 0x00, 0x00, 0x05]), "{connect:02x?}");
    assert!(contains(b"\x03\x00\x0atext/plain"), "{connect:02x?}");
}
