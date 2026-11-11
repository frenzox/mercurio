#[cfg(unix)]
use std::sync::Arc;
use std::time::Duration;

use mercurio_server::{
    error::ServerError,
    server::{run_with_config, ListenerConfig, ServerConfig, Transport},
    tls::TlsConfig,
};
#[cfg(unix)]
use tokio::net::{unix::UCred, UnixListener};
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

/// Starts a broker with `extra` beside its main listener and returns what
/// startup gave, which must come before it serves.
async fn start_with(extra: ListenerConfig) -> Result<(), ServerError> {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut config = ServerConfig::default();
    config.listeners = vec![extra];
    timeout(
        Duration::from_secs(2),
        run_with_config(listener, config, std::future::pending::<()>()),
    )
    .await
    .expect("server started serving anyway")
}

#[tokio::test]
async fn trusted_tcp_listeners_fail_startup() {
    let mut value = ListenerConfig::new(
        TcpListener::bind("127.0.0.1:0").await.unwrap(),
        Transport::Mqtt,
    );
    value.trusted = true;
    let result = start_with(value).await;
    assert!(matches!(result, Err(ServerError::Config(_))), "{result:?}");
}

/// A PROXY header can claim any address, so trusting every peer to send
/// one has to be asked for by name.
#[tokio::test]
async fn proxy_protocol_without_proxy_from_fails_startup() {
    let mut value = ListenerConfig::new(
        TcpListener::bind("127.0.0.1:0").await.unwrap(),
        Transport::Mqtt,
    );
    value.proxy_protocol = true;
    let result = start_with(value).await;
    let Err(ServerError::Config(message)) = result else {
        panic!("{result:?}");
    };
    assert!(message.contains(r#"["0.0.0.0/0", "::/0"]"#), "{message}");
}

#[cfg(unix)]
#[tokio::test]
async fn peer_filters_on_tcp_listeners_fail_startup() {
    let mut value = ListenerConfig::new(
        TcpListener::bind("127.0.0.1:0").await.unwrap(),
        Transport::Mqtt,
    );
    value.peer_filter = Some(Arc::new(|_: &UCred| true));
    let result = start_with(value).await;
    assert!(matches!(result, Err(ServerError::Config(_))), "{result:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn trusted_listeners_take_no_plugins() {
    let path = std::env::temp_dir().join(format!(
        "mercurio-trusted-plugins-{}.sock",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let mut value = ListenerConfig::new(UnixListener::bind(&path).unwrap(), Transport::Mqtt);
    value.trusted = true;
    value.plugins = Some(Vec::new());
    let result = start_with(value).await;
    let _ = std::fs::remove_file(&path);
    assert!(matches!(result, Err(ServerError::Config(_))), "{result:?}");
}

#[cfg(unix)]
async fn trusted_socket(name: &str, transport: Transport) -> (ListenerConfig, std::path::PathBuf) {
    let path = std::env::temp_dir().join(format!("mercurio-{name}-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut value = ListenerConfig::new(UnixListener::bind(&path).unwrap(), transport);
    value.trusted = true;
    (value, path)
}

#[cfg(unix)]
#[tokio::test]
async fn trusted_listeners_refuse_the_proxy_protocol() {
    let (mut value, path) = trusted_socket("trusted-proxy", Transport::Mqtt).await;
    value.proxy_protocol = true;
    let result = start_with(value).await;
    let _ = std::fs::remove_file(&path);
    assert!(matches!(result, Err(ServerError::Config(_))), "{result:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn trusted_listeners_refuse_the_websocket_transport() {
    let (value, path) = trusted_socket("trusted-ws", Transport::WebSocket).await;
    let result = start_with(value).await;
    let _ = std::fs::remove_file(&path);
    assert!(matches!(result, Err(ServerError::Config(_))), "{result:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn trusted_listeners_take_no_auth() {
    let (mut value, path) = trusted_socket("trusted-auth", Transport::Mqtt).await;
    value.auth = Some(Default::default());
    let result = start_with(value).await;
    let _ = std::fs::remove_file(&path);
    assert!(matches!(result, Err(ServerError::Config(_))), "{result:?}");
}
