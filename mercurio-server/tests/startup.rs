use std::time::Duration;

use mercurio_server::{
    error::ServerError,
    server::{run_with_config, ListenerConfig, ServerConfig, Transport},
    tls::TlsConfig,
};
use tokio::{net::TcpListener, time::timeout};

#[tokio::test]
async fn unusable_tls_configuration_fails_startup() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut config = ServerConfig::default();
    config.tls = Some(TlsConfig::new(
        "/nonexistent/cert.pem",
        "/nonexistent/key.pem",
    ));

    let result = timeout(
        Duration::from_secs(2),
        run_with_config(listener, config, std::future::pending::<()>()),
    )
    .await
    .expect("server started serving anyway");
    assert!(matches!(result, Err(ServerError::Tls(_))), "{result:?}");
}

#[tokio::test]
async fn unusable_tls_on_extra_listener_fails_startup() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut config = ServerConfig::default();
    config.listeners = vec![{
        let mut value = ListenerConfig::new(
            TcpListener::bind("127.0.0.1:0").await.unwrap(),
            Transport::WebSocket,
        );
        value.tls = Some(TlsConfig::new(
            "/nonexistent/cert.pem",
            "/nonexistent/key.pem",
        ));
        value
    }];

    let result = timeout(
        Duration::from_secs(2),
        run_with_config(listener, config, std::future::pending::<()>()),
    )
    .await
    .expect("server started serving anyway");
    assert!(matches!(result, Err(ServerError::Tls(_))), "{result:?}");
}
