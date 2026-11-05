use std::time::Duration;

use mercurio_server::{
    error::ServerError,
    server::{run_with_config, ServerConfig},
    tls::TlsConfig,
};
use tokio::{net::TcpListener, time::timeout};

#[tokio::test]
async fn unusable_tls_configuration_fails_startup() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = ServerConfig {
        tls: Some(TlsConfig::new(
            "/nonexistent/cert.pem",
            "/nonexistent/key.pem",
        )),
        ..Default::default()
    };

    let result = timeout(
        Duration::from_secs(2),
        run_with_config(listener, config, std::future::pending::<()>()),
    )
    .await
    .expect("server started serving anyway");
    assert!(matches!(result, Err(ServerError::Tls(_))), "{result:?}");
}
