use std::time::Duration;

use mercurio_client::{ClientError, ConnectOptions, MqttClient, QoS};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    time::timeout,
};

#[tokio::test]
async fn requests_to_a_stopped_client_fail_as_closed() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut connect = [0u8; 256];
        let _ = socket.read(&mut connect).await.unwrap();
        socket
            .write_all(&[0x20, 0x03, 0x00, 0x00, 0x00])
            .await
            .unwrap();
        let mut rest = Vec::new();
        let _ = socket.read_to_end(&mut rest).await;
    });

    let client = MqttClient::connect(ConnectOptions::new("127.0.0.1", port).keep_alive(0))
        .await
        .unwrap();
    client.disconnect().await.unwrap();
    // The event stream ends once the client has stopped
    while timeout(Duration::from_secs(2), client.recv())
        .await
        .unwrap()
        .is_some()
    {}

    let closed = |result: Result<(), ClientError>| {
        let Err(error) = result else {
            panic!("a stopped client took a request");
        };
        assert!(matches!(error, ClientError::Closed), "{error:?}");
        assert!(!error.is_transient());
    };
    closed(client.publish("a", "x", QoS::AtMostOnce).await);
    closed(client.subscribe(&[("a", QoS::AtMostOnce)]).await.map(drop));
    closed(client.unsubscribe(&["a"]).await.map(drop));
    closed(client.reauthenticate().await);
    closed(client.disconnect().await);
}
