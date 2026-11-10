use std::{net::SocketAddr, sync::Arc, time::Duration};

use bytes::{Bytes, BytesMut};
use mercurio_core::{
    properties::{CorrelationData, ResponseTopic},
    protocol::ProtocolVersion,
    qos::QoS,
    reason::ReasonCode,
};
use mercurio_packets::{
    connect::{ConnectFlags, ConnectPacket, ConnectPayload, ConnectProperties},
    publish::{PublishPacket, PublishProperties},
    subscribe::{SubscribePacket, SubscribePayload, SubscriptionOptions},
    ControlPacket,
};
use mercurio_server::{
    acl::AclFile,
    admin::{Admin, AdminError, AdminExtension, ErrorCode},
    plugin::{ready, Access, BoxFuture, ClientInfo, Decision, Plugin},
    server::{ListenerConfig, LocalConnector, Server, ServerConfig, Transport},
};
use mercurio_storage::memory::MemoryStore;
use serde_json::{json, Value};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

fn config(plugins: Vec<Arc<dyn Plugin>>) -> ServerConfig {
    let mut config = ServerConfig::default();
    config.plugins = plugins;
    config.sys_interval = None;
    config
}

async fn start(config: ServerConfig) -> (SocketAddr, Admin) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = Server::new(listener, config, Arc::new(MemoryStore::new()))
        .await
        .unwrap();
    let admin = server.admin();
    tokio::spawn(server.run(std::future::pending::<()>()));
    (addr, admin)
}

/// Allows everything, `$CONTROL` included.
struct AllowAll;

impl Plugin for AllowAll {
    fn name(&self) -> &str {
        "allow-all"
    }

    fn authorize<'a>(
        &'a self,
        _: &'a ClientInfo,
        _: &'a str,
        _: Access,
    ) -> BoxFuture<'a, Decision> {
        ready(Decision::Allow)
    }
}

/// MQTT 5, clean start, keep alive 60.
fn connect_packet(client_id: &str) -> ConnectPacket {
    ConnectPacket {
        protocol_version: ProtocolVersion::V5,
        flags: ConnectFlags {
            clean_start: true,
            ..Default::default()
        },
        keepalive: 60,
        properties: Some(ConnectProperties::default()),
        payload: ConnectPayload {
            client_id: client_id.into(),
            ..Default::default()
        },
    }
}

struct Client<S = TcpStream> {
    stream: S,
    buffer: BytesMut,
    version: ProtocolVersion,
    client_id: String,
}

impl Client {
    async fn connect(addr: SocketAddr, client_id: &str) -> Client {
        Client::connect_with(addr, connect_packet(client_id)).await
    }

    async fn connect_with(addr: SocketAddr, packet: ConnectPacket) -> Client {
        Client::handshake(TcpStream::connect(addr).await.unwrap(), packet).await
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> Client<S> {
    async fn handshake(stream: S, packet: ConnectPacket) -> Client<S> {
        let mut client = Client {
            stream,
            buffer: BytesMut::new(),
            version: packet.protocol_version,
            client_id: packet.payload.client_id.clone(),
        };
        client.send(ControlPacket::Connect(packet)).await;
        let Some(ControlPacket::ConnAck(connack)) = client.receive().await else {
            panic!("Expected CONNACK");
        };
        assert_eq!(connack.reason_code, ReasonCode::Success);
        client
    }

    async fn send(&mut self, packet: ControlPacket) {
        let mut encoded = BytesMut::new();
        packet.encode_with_version(&mut encoded, self.version);
        self.stream.write_all(&encoded).await.unwrap();
    }

    async fn receive(&mut self) -> Option<ControlPacket> {
        loop {
            if let Ok(packet) = ControlPacket::parse_with_version(&mut self.buffer, self.version) {
                return Some(packet);
            }
            let read = timeout(
                Duration::from_millis(500),
                self.stream.read_buf(&mut self.buffer),
            )
            .await;
            match read {
                Ok(Ok(n)) if n > 0 => continue,
                _ => return None,
            }
        }
    }

    async fn subscribe(&mut self, filter: &str) {
        self.send(ControlPacket::Subscribe(SubscribePacket {
            packet_id: 1,
            properties: None,
            payload: vec![SubscribePayload {
                topic_filter: filter.into(),
                subs_opt: SubscriptionOptions {
                    qos: QoS::AtLeastOnce,
                    ..Default::default()
                },
            }],
        }))
        .await;
        assert!(matches!(
            self.receive().await,
            Some(ControlPacket::SubAck(_))
        ));
    }

    /// Sends a request to the broker API, answered on
    /// `replies/<client id>`, and returns the parsed answer.
    async fn request(&mut self, payload: Bytes) -> Value {
        let properties = PublishProperties {
            response_topic: Some(ResponseTopic::new(format!("replies/{}", self.client_id))),
            correlation_data: Some(CorrelationData::new(Bytes::from_static(b"admin"))),
            ..Default::default()
        };
        self.send(ControlPacket::Publish(PublishPacket {
            topic_name: "$CONTROL/broker/v1".into(),
            payload: Some(payload),
            properties: Some(properties),
            ..Default::default()
        }))
        .await;
        let Some(ControlPacket::Publish(answer)) = self.receive().await else {
            panic!("Expected the response");
        };
        serde_json::from_slice(answer.payload.as_deref().unwrap()).unwrap()
    }

    /// Runs one command and returns its response.
    async fn run(&mut self, command: Value) -> Value {
        let envelope = json!({ "commands": [command] }).to_string();
        let reply = self.request(Bytes::from(envelope)).await;
        reply["responses"][0].clone()
    }
}

/// A client named `admin`, subscribed to its replies.
async fn admin_client(addr: SocketAddr) -> Client {
    let mut admin = Client::connect(addr, "admin").await;
    admin.subscribe("replies/admin").await;
    admin
}

#[tokio::test]
async fn commands_are_answered_in_order_with_their_correlation_data() {
    let (addr, _) = start(config(vec![Arc::new(AllowAll)])).await;
    let mut admin = admin_client(addr).await;
    let envelope = json!({"commands": [
        {"command": "getBrokerInfo", "correlationData": "1"},
        {"command": "listListeners", "correlationData": 2},
    ]});
    let reply = admin.request(Bytes::from(envelope.to_string())).await;
    let responses = reply["responses"].as_array().unwrap();
    assert_eq!(responses.len(), 2);
    assert_eq!(responses[0]["command"], "getBrokerInfo");
    assert_eq!(responses[0]["correlationData"], "1");
    assert_eq!(responses[0]["data"]["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(responses[0]["data"]["storage"], "memory");
    assert_eq!(responses[0]["data"]["stats"]["clients/connected"], 1);
    assert!(responses[0].get("error").is_none());
    assert_eq!(responses[1]["command"], "listListeners");
    assert_eq!(responses[1]["correlationData"], 2);
    assert_eq!(responses[1]["data"]["totalCount"], 1);
    assert_eq!(
        responses[1]["data"]["listeners"][0]["address"],
        addr.to_string()
    );
    assert!(responses[1]["data"]["listeners"][0].get("path").is_none());
}

#[tokio::test]
async fn malformed_requests_fail_with_invalid_argument() {
    let (addr, _) = start(config(vec![Arc::new(AllowAll)])).await;
    let mut admin = admin_client(addr).await;

    let reply = admin.request(Bytes::from_static(b"not json")).await;
    let responses = reply["responses"].as_array().unwrap();
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0]["command"], "");
    assert_eq!(responses[0]["code"], "invalidArgument");

    let unknown = admin.run(json!({"command": "fooBar"})).await;
    assert_eq!(unknown["code"], "invalidArgument");
    assert!(unknown["error"].as_str().unwrap().contains("fooBar"));
    let unnamed = admin.run(json!({"correlationData": "x"})).await;
    assert_eq!(unnamed["command"], "");
    assert_eq!(unnamed["code"], "invalidArgument");
    assert_eq!(unnamed["correlationData"], "x");
    let wildcard = admin.run(json!({"command": "list/#"})).await;
    assert_eq!(wildcard["code"], "invalidArgument");
    let too_many = admin
        .run(json!({"command": "listListeners", "count": 1001}))
        .await;
    assert_eq!(too_many["code"], "invalidArgument");
    let wrong_type = admin
        .run(json!({"command": "listListeners", "offset": "x"}))
        .await;
    assert_eq!(wrong_type["code"], "invalidArgument");
}

const ACL: &str = "
topic write $CONTROL/broker/v1
topic write $CONTROL/broker/v1/listListeners
topic read replies/#
";

#[tokio::test]
async fn each_command_needs_its_own_permission() {
    let (addr, _) = start(config(vec![Arc::new(AclFile::parse(ACL).unwrap())])).await;
    let mut admin = admin_client(addr).await;
    let envelope = json!({"commands": [
        {"command": "listListeners"},
        {"command": "getBrokerInfo"},
    ]});
    let reply = admin.request(Bytes::from(envelope.to_string())).await;
    assert!(reply["responses"][0].get("error").is_none());
    assert_eq!(reply["responses"][1]["code"], "notAuthorized");
}

#[tokio::test]
async fn in_process_clients_run_every_command() {
    let (connector, local_listener) = LocalConnector::new();
    let mut config = config(Vec::new());
    config.local_listener = Some(local_listener);
    start(config).await;
    let stream = connector.connect().unwrap();
    let mut bridge = Client::handshake(stream, connect_packet("bridge")).await;
    bridge.subscribe("replies/bridge").await;
    let info = bridge.run(json!({"command": "getBrokerInfo"})).await;
    assert_eq!(info["data"]["version"], env!("CARGO_PKG_VERSION"));
}

#[tokio::test]
async fn mqtt311_clients_get_responses_on_the_fixed_topic() {
    let (addr, _) = start(config(vec![Arc::new(AllowAll)])).await;
    let mut packet = connect_packet("old");
    packet.protocol_version = ProtocolVersion::V3_1_1;
    packet.properties = None;
    let mut old = Client::connect_with(addr, packet).await;
    old.subscribe("$CONTROL/broker/v1/response").await;
    let envelope = json!({"commands": [{"command": "getBrokerInfo"}]}).to_string();
    old.send(ControlPacket::Publish(PublishPacket {
        topic_name: "$CONTROL/broker/v1".into(),
        payload: Some(Bytes::from(envelope)),
        ..Default::default()
    }))
    .await;
    let Some(ControlPacket::Publish(answer)) = old.receive().await else {
        panic!("Expected the response");
    };
    assert_eq!(answer.topic_name, "$CONTROL/broker/v1/response");
    let reply: Value = serde_json::from_slice(answer.payload.as_deref().unwrap()).unwrap();
    assert_eq!(reply["responses"][0]["command"], "getBrokerInfo");
}

/// Serves `getLogLevel` and fails `reload`, as mercuriod would.
struct Daemon;

impl AdminExtension for Daemon {
    fn handle<'a>(
        &'a self,
        command: &'a str,
        _: &'a Value,
    ) -> BoxFuture<'a, Option<Result<Value, AdminError>>> {
        ready(match command {
            "getLogLevel" => Some(Ok(json!({"level": "info"}))),
            "reload" => Some(Err(AdminError::new(ErrorCode::Conflict, "busy"))),
            _ => None,
        })
    }
}

#[tokio::test]
async fn daemon_commands_are_unavailable_without_an_extension() {
    let (addr, _) = start(config(vec![Arc::new(AllowAll)])).await;
    let mut admin = admin_client(addr).await;
    for command in [
        "listBridges",
        "getConfig",
        "getLogLevel",
        "setLogLevel",
        "reload",
    ] {
        let response = admin.run(json!({ "command": command })).await;
        assert_eq!(response["code"], "unavailable", "{command}");
    }
}

#[tokio::test]
async fn unknown_commands_go_to_the_extension() {
    let mut config = config(vec![Arc::new(AllowAll)]);
    config.admin_extension = Some(Arc::new(Daemon));
    let (addr, _) = start(config).await;
    let mut admin = admin_client(addr).await;
    let level = admin.run(json!({"command": "getLogLevel"})).await;
    assert_eq!(level["data"]["level"], "info");
    assert!(level.get("error").is_none());
    let reload = admin.run(json!({"command": "reload"})).await;
    assert_eq!(reload["code"], "conflict");
    assert_eq!(reload["error"], "busy");
    let bridges = admin.run(json!({"command": "listBridges"})).await;
    assert_eq!(bridges["code"], "unavailable");
    let unknown = admin.run(json!({"command": "fooBar"})).await;
    assert_eq!(unknown["code"], "invalidArgument");
}

#[tokio::test]
async fn info_reports_the_broker_and_its_counters() {
    let (_, admin) = start(config(Vec::new())).await;
    let info = admin.info().await;
    assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(info.storage, "memory");
    assert!(info.uptime < 60);
    assert!(info.started_at > 1_700_000_000);
    assert_eq!(info.stats["clients/connected"], 0);
    assert_eq!(info.stats["subscriptions/count"], 0);
}

#[tokio::test]
async fn listeners_are_listed_with_their_connections() {
    let ws = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_addr = ws.local_addr().unwrap();
    let mut websocket = ListenerConfig::new(ws, Transport::WebSocket);
    websocket.mount_point = Some("tenant/".into());
    let mut config = config(Vec::new());
    config.listeners = vec![websocket];
    let (addr, admin) = start(config).await;

    let _socket = TcpStream::connect(addr).await.unwrap();
    // Counted once the accept loop picks the connection up
    let mut listeners = admin.listeners().await;
    for _ in 0..100 {
        if listeners[0].connections == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        listeners = admin.listeners().await;
    }
    assert_eq!(listeners.len(), 2);
    assert_eq!(
        listeners[0].address.as_deref(),
        Some(addr.to_string().as_str())
    );
    assert_eq!(listeners[0].path, None);
    assert_eq!(listeners[0].protocol, "mqtt");
    assert!(!listeners[0].tls);
    assert_eq!(listeners[0].connections, 1);
    assert_eq!(
        listeners[1].address.as_deref(),
        Some(ws_addr.to_string().as_str())
    );
    assert_eq!(listeners[1].protocol, "websocket");
    assert_eq!(listeners[1].mount_point.as_deref(), Some("tenant/"));
    assert_eq!(listeners[1].connections, 0);
}
