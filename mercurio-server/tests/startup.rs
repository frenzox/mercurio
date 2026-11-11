#[cfg(unix)]
use std::sync::Arc;
use std::time::Duration;

use mercurio_server::{
    error::{ConfigErrorKind, ServerError},
    server::{run_with_config, ListenerConfig, ServerConfig, Transport},
};
#[cfg(unix)]
use tokio::net::{unix::UCred, UnixListener};
use tokio::{net::TcpListener, time::timeout};

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

/// Asserts that startup failed on the extra listener, for `kind`.
#[track_caller]
fn assert_kind(result: Result<(), ServerError>, kind: ConfigErrorKind) {
    let Err(ServerError::Config(error)) = result else {
        panic!("{result:?}");
    };
    assert_eq!(error.kind, kind, "{error}");
    assert_eq!(error.listener, 1, "{error}");
}

#[tokio::test]
async fn trusted_tcp_listeners_fail_startup() {
    let mut value = ListenerConfig::new(
        TcpListener::bind("127.0.0.1:0").await.unwrap(),
        Transport::Mqtt,
    );
    value.trusted = true;
    let result = start_with(value).await;
    assert_kind(result, ConfigErrorKind::TrustedTcp);
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
    let Err(ServerError::Config(error)) = result else {
        panic!("{result:?}");
    };
    assert_eq!(error.kind, ConfigErrorKind::ProxyFromRequired);
    assert_eq!(error.listener, 1);
    let message = error.to_string();
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
    assert_kind(result, ConfigErrorKind::PeerFilterOnTcp);
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
    assert_kind(result, ConfigErrorKind::TrustedWithAuth);
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
    assert_kind(result, ConfigErrorKind::TrustedBehindProxy);
}

#[cfg(unix)]
#[tokio::test]
async fn trusted_listeners_refuse_the_websocket_transport() {
    let (value, path) = trusted_socket("trusted-ws", Transport::WebSocket).await;
    let result = start_with(value).await;
    let _ = std::fs::remove_file(&path);
    assert_kind(result, ConfigErrorKind::TrustedBehindProxy);
}

#[cfg(unix)]
#[tokio::test]
async fn trusted_listeners_take_no_auth() {
    let (mut value, path) = trusted_socket("trusted-auth", Transport::Mqtt).await;
    value.auth = Some(Default::default());
    let result = start_with(value).await;
    let _ = std::fs::remove_file(&path);
    assert_kind(result, ConfigErrorKind::TrustedWithAuth);
}

async fn tcp_listener(transport: Transport) -> ListenerConfig {
    ListenerConfig::new(TcpListener::bind("127.0.0.1:0").await.unwrap(), transport)
}

/// A wildcard in a mount point would make the listener's topic filters
/// match other mount points' topics, and its topic names invalid.
#[tokio::test]
async fn mount_points_with_wildcards_fail_startup() {
    for mount_point in ["tenant/+/", "tenant/#", "+/"] {
        let mut value = tcp_listener(Transport::Mqtt).await;
        value.mount_point = Some(mount_point.into());
        let result = start_with(value).await;
        assert_kind(
            result,
            ConfigErrorKind::MountPointWildcard(mount_point.into()),
        );
    }
}

/// Only WebSocket listeners speak HTTP, so these settings would do nothing.
#[tokio::test]
async fn websocket_settings_on_mqtt_listeners_fail_startup() {
    let mut value = tcp_listener(Transport::Mqtt).await;
    value.http_dir = Some(std::env::temp_dir());
    let result = start_with(value).await;
    assert_kind(result, ConfigErrorKind::WebSocketOnly("http_dir"));

    let mut value = tcp_listener(Transport::Mqtt).await;
    value.allowed_origins = Some(vec!["https://app.example".into()]);
    let result = start_with(value).await;
    assert_kind(result, ConfigErrorKind::WebSocketOnly("allowed_origins"));
}

/// A limit too large to count is no limit, not a panic.
#[tokio::test]
async fn huge_connection_limits_serve() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let extra_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = extra_listener.local_addr().unwrap();
    let mut extra = ListenerConfig::new(extra_listener, Transport::Mqtt);
    extra.max_connections = Some(usize::MAX);
    let mut config = ServerConfig::default();
    config.limits.max_connections = Some(usize::MAX);
    config.listeners = vec![extra];
    let server = tokio::spawn(run_with_config(
        listener,
        config,
        std::future::pending::<()>(),
    ));

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(&[
            0x10, 0x0e, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x05, 0x02, 0x00, 0x3c, 0x00, 0x00,
            0x01, b'h',
        ])
        .await
        .unwrap();
    let mut first = [0u8; 1];
    let read = timeout(Duration::from_secs(2), stream.read_exact(&mut first)).await;
    assert!(
        matches!(read, Ok(Ok(_))) && first[0] == 0x20,
        "no CONNACK; server stopped: {}",
        server.is_finished()
    );
}
