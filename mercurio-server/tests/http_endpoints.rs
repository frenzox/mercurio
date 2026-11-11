mod common;

use std::{net::SocketAddr, time::Duration};

use mercurio_server::{
    error::ServerError,
    server::{ListenerConfig, Server, ServerConfig, Transport},
};
use mercurio_storage::memory::MemoryStore;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

/// A broker with a WebSocket listener configured by `configure`, whose
/// address is returned.
async fn serve(configure: impl FnOnce(&mut ListenerConfig)) -> (SocketAddr, SocketAddr) {
    let mqtt = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mqtt_addr = mqtt.local_addr().unwrap();
    let websocket = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = websocket.local_addr().unwrap();
    let mut listener = ListenerConfig::new(websocket, Transport::WebSocket);
    configure(&mut listener);
    let mut config = ServerConfig::default();
    config.listeners = vec![listener];
    tokio::spawn(mercurio_server::server::run_with_config(
        mqtt,
        config,
        std::future::pending::<()>(),
    ));
    (addr, mqtt_addr)
}

async fn get(addr: SocketAddr, target: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let request = format!("GET {target} HTTP/1.1\r\nHost: x\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    timeout(Duration::from_secs(5), stream.read_to_end(&mut response))
        .await
        .expect("response not finished")
        .unwrap();
    String::from_utf8(response).unwrap()
}

#[tokio::test]
async fn health_and_metrics_are_served_without_http_dir() {
    let (addr, mqtt_addr) = serve(|listener| {
        listener.health = true;
        listener.metrics = true;
    })
    .await;
    let mut client = TcpStream::connect(mqtt_addr).await.unwrap();
    // CONNECT, MQTT 5, client id "secret-device"
    let mut connect = vec![0x10, 0x1a, 0x00, 0x04];
    connect.extend_from_slice(b"MQTT");
    connect.extend_from_slice(&[0x05, 0x02, 0x00, 0x3c, 0x00, 0x00, 0x0d]);
    connect.extend_from_slice(b"secret-device");
    client.write_all(&connect).await.unwrap();
    common::read_connack(&mut client).await;

    let health = get(addr, "/healthz").await;
    assert!(health.starts_with("HTTP/1.1 200 OK\r\n"), "{health}");

    // The client counts as connected just after its CONNACK
    let mut metrics = get(addr, "/metrics").await;
    for _ in 0..100 {
        if metrics.contains("\nmercurio_clients_connected 1\n") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        metrics = get(addr, "/metrics").await;
    }
    assert!(metrics.starts_with("HTTP/1.1 200 OK\r\n"), "{metrics}");
    assert!(
        metrics.contains("\r\nContent-Type: text/plain; version=0.0.4; charset=utf-8\r\n"),
        "{metrics}"
    );
    let body = &metrics[metrics.find("\r\n\r\n").unwrap() + 4..];
    for line in [
        "# TYPE mercurio_clients_connected gauge",
        "mercurio_clients_connected 1",
        "# HELP mercurio_messages_received_total ",
        "# TYPE mercurio_messages_received_total counter",
        "# TYPE mercurio_bytes_sent_total counter",
        "# TYPE mercurio_sessions gauge",
        "# TYPE mercurio_subscriptions gauge",
        "# TYPE mercurio_retained_messages gauge",
        "# TYPE mercurio_messages_dropped_total counter",
        "mercurio_messages_dropped_total{cause=\"queue_full\"} 0",
        "mercurio_messages_dropped_total{cause=\"no_share_member\"} 0",
        "# TYPE mercurio_storage_batches_dropped_total counter",
        "# TYPE mercurio_connections_total counter",
        "# TYPE mercurio_uptime_seconds gauge",
    ] {
        assert!(body.lines().any(|l| l.starts_with(line)), "{line}:\n{body}");
    }
    // Each metric is described once, its samples right after
    let types = body.lines().filter(|l| l.starts_with("# TYPE ")).count();
    let helps = body.lines().filter(|l| l.starts_with("# HELP ")).count();
    assert_eq!(types, helps);
    assert_eq!(
        body.matches("# TYPE mercurio_messages_dropped_total")
            .count(),
        1
    );
    assert!(!body.contains("secret-device"));
    assert!(body.ends_with('\n'));
}

#[tokio::test]
async fn endpoints_are_off_unless_enabled() {
    let (addr, _) = serve(|listener| listener.health = true).await;
    assert!(get(addr, "/healthz").await.starts_with("HTTP/1.1 200"));
    let metrics = get(addr, "/metrics").await;
    assert!(metrics.starts_with("HTTP/1.1 404"), "{metrics}");

    let dir = std::env::temp_dir().join(format!("mercurio-endpoints-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (addr, _) = serve(|listener| listener.http_dir = Some(dir.clone())).await;
    assert!(get(addr, "/healthz").await.starts_with("HTTP/1.1 404"));
    assert!(get(addr, "/metrics").await.starts_with("HTTP/1.1 404"));
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn endpoints_need_a_websocket_listener() {
    let mut listener = ListenerConfig::new(
        TcpListener::bind("127.0.0.1:0").await.unwrap(),
        Transport::Mqtt,
    );
    listener.metrics = true;
    let mut config = ServerConfig::default();
    config.listeners = vec![listener];
    let main = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let result = Server::new(main, config, std::sync::Arc::new(MemoryStore::new())).await;
    assert!(matches!(result, Err(ServerError::Config(_))));
}
