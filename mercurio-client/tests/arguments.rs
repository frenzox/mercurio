use std::time::Duration;

use mercurio_client::{ClientError, ConnectOptions, MqttClient, QoS, SubscriptionOptions};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    time::timeout,
};

#[tokio::test]
async fn invalid_arguments_are_refused_before_reaching_the_broker() {
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
        let mut sent = Vec::new();
        socket.read_to_end(&mut sent).await.unwrap();
        sent
    });

    let client = MqttClient::connect(ConnectOptions::new("127.0.0.1", port).keep_alive(0))
        .await
        .unwrap();
    let invalid = |result: Result<(), ClientError>| {
        assert!(
            matches!(result, Err(ClientError::InvalidArgument(_))),
            "{result:?}"
        );
    };
    for topic in ["a/#", "a/+/b", "+", ""] {
        invalid(client.publish(topic, "x", QoS::AtMostOnce).await);
    }
    invalid(client.subscribe(&[]).await.map(drop));
    for filter in ["a/#/b", "a+", "", "$share/g"] {
        invalid(
            client
                .subscribe(&[(filter, QoS::AtMostOnce)])
                .await
                .map(drop),
        );
    }
    let options = SubscriptionOptions::default();
    for id in [0, 268_435_456] {
        invalid(
            client
                .subscribe_with(&[("a", options)], Some(id))
                .await
                .map(drop),
        );
    }
    invalid(client.unsubscribe(&[]).await.map(drop));
    invalid(client.unsubscribe(&["a/#/b"]).await.map(drop));
    client.disconnect().await.unwrap();

    // Nothing but the DISCONNECT reached the broker
    let sent = timeout(Duration::from_secs(2), broker)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(sent[0], 0xe0, "sent {sent:02x?}");
    assert_eq!(sent.len(), 2 + sent[1] as usize, "sent {sent:02x?}");
}
