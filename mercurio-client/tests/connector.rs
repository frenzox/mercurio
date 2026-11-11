use std::time::Duration;

use mercurio_client::{ClientError, ConnectOptions, Event, MqttClient, QoS};
use mercurio_server::server::{LocalConnector, ServerConfig};
use tokio::{net::TcpListener, time::timeout};

#[tokio::test]
async fn connector_replaces_tcp() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (connector, local_listener) = LocalConnector::new();
    let mut config = ServerConfig::default();
    config.local_listener = Some(local_listener);
    tokio::spawn(mercurio_server::server::run_with_config(
        listener,
        config,
        std::future::pending::<()>(),
    ));

    // Nothing listens on this host and port; only the connector can work.
    // The stream comes from a future, as an async handshake would give it.
    let options = ConnectOptions::new("unused.invalid", 1).connector(move || {
        let connector = connector.clone();
        async move {
            tokio::task::yield_now().await;
            connector.connect()
        }
    });
    let client = MqttClient::connect(options).await.unwrap();
    client.subscribe(&[("t", QoS::AtLeastOnce)]).await.unwrap();
    client.publish("t", "hi", QoS::AtLeastOnce).await.unwrap();

    let event = timeout(Duration::from_secs(2), client.recv())
        .await
        .unwrap()
        .unwrap();
    let Event::Message { payload, .. } = event else {
        panic!("expected a message, got {event:?}");
    };
    assert_eq!(&payload[..], b"hi");
}

#[tokio::test]
async fn a_connector_that_never_finishes_times_out() {
    let options = ConnectOptions::new("unused.invalid", 1)
        .connect_timeout(Duration::from_millis(100))
        .connector(std::future::pending::<std::io::Result<tokio::io::DuplexStream>>);
    let result = timeout(Duration::from_secs(2), MqttClient::connect(options))
        .await
        .expect("the connect timeout applies to the connector");
    assert!(
        matches!(result, Err(ClientError::Timeout)),
        "{:?}",
        result.err()
    );
}

#[tokio::test]
async fn a_failing_connector_keeps_its_error() {
    let options = ConnectOptions::new("unused.invalid", 1).connector(|| async {
        Err::<tokio::io::DuplexStream, _>(std::io::Error::from(
            std::io::ErrorKind::ConnectionRefused,
        ))
    });
    let result = MqttClient::connect(options).await;
    let Err(ClientError::ConnectionFailed(error)) = result else {
        panic!("{:?}", result.err());
    };
    assert_eq!(error.kind(), std::io::ErrorKind::ConnectionRefused);
}
