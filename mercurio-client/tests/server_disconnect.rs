use std::time::Duration;

use mercurio_client::{ClientError, ConnectOptions, DisconnectReason, Event, MqttClient};
use mercurio_core::reason::ReasonCode;
use mercurio_server::server::{ConnectPolicy, Redirect, ServerConfig};
use tokio::{net::TcpListener, time::timeout};

async fn start_broker(config: ServerConfig) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(mercurio_server::server::run_with_config(
        listener,
        config,
        std::future::pending::<()>(),
    ));
    port
}

#[tokio::test]
async fn server_disconnect_ends_the_connection_with_its_reason() {
    let port = start_broker(ServerConfig::default()).await;
    let options = ConnectOptions::new("127.0.0.1", port).client_id("same");
    let first = MqttClient::connect(options.clone()).await.unwrap();
    let _second = MqttClient::connect(options).await.unwrap();

    let event = timeout(Duration::from_secs(2), first.recv()).await.unwrap();
    let Some(Event::Disconnected {
        reason:
            DisconnectReason::ServerInitiated {
                reason,
                reason_string,
                ..
            },
    }) = event
    else {
        panic!("Expected a DISCONNECT from the broker, got {:?}", event);
    };
    assert_eq!(reason, ReasonCode::SessionTakenOver);
    assert!(reason_string.is_some());

    // The connection is over: nothing else arrives
    assert!(timeout(Duration::from_secs(1), first.recv())
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn redirection_names_the_other_server() {
    let port = start_broker(ServerConfig {
        policy: ConnectPolicy {
            redirect: Some(Redirect {
                server_reference: "elsewhere:1883".into(),
                permanent: true,
            }),
            ..Default::default()
        },
        ..Default::default()
    })
    .await;

    let result = MqttClient::connect(ConnectOptions::new("127.0.0.1", port)).await;
    assert!(matches!(
        result,
        Err(ClientError::Redirected { reason: ReasonCode::ServerMoved, ref server_reference })
            if server_reference == "elsewhere:1883"
    ));
}
