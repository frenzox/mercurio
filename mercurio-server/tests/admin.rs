mod common;

use std::{net::SocketAddr, sync::Arc, time::Duration};

use bytes::{Bytes, BytesMut};
use mercurio_core::{
    properties::{AuthenticationMethod, CorrelationData, ResponseTopic, SessionExpiryInterval},
    protocol::ProtocolVersion,
    qos::QoS,
    reason::ReasonCode,
};
use mercurio_packets::{
    auth::{AuthPacket, AuthProperties},
    connect::{ConnectPacket, ConnectPayload, ConnectProperties, Will},
    publish::{PublishPacket, PublishProperties},
    subscribe::{SubscribePacket, SubscribePayload, SubscriptionOptions},
    ControlPacket,
};
use mercurio_server::{
    acl::AclFile,
    admin::{Admin, AdminError, AdminExtension, BanKey, BanKind, ErrorCode, NewBan, Paging},
    plugin::{
        ready, Access, AuthExchange, AuthResult, BoxFuture, ClientInfo, Credentials, Decision,
        Plugin,
    },
    server::{ListenerConfig, LocalConnector, ServerBuilder, ServerConfig, Transport},
};
use mercurio_storage::{memory::MemoryStore, sqlite::SqliteStore, BanStore, SessionKey};

use serde_json::{json, Value};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
use tokio::net::{unix::UCred, UnixListener, UnixStream};
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
    start_with(config, |builder| builder).await
}

/// Like `start`, with what `extra` adds to the broker, such as listeners.
async fn start_with(
    config: ServerConfig,
    extra: impl FnOnce(ServerBuilder) -> ServerBuilder,
) -> (SocketAddr, Admin) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = extra(common::broker(listener, config))
        .build()
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

    fn authorize_control<'a>(&'a self, _: &'a ClientInfo, _: &'a str) -> BoxFuture<'a, Decision> {
        ready(Decision::Allow)
    }

    fn authorize<'a>(
        &'a self,
        _: &'a ClientInfo,
        _: &'a str,
        _: Access,
    ) -> BoxFuture<'a, Decision> {
        ready(Decision::Allow)
    }

    fn authenticate<'a>(
        &'a self,
        _: &'a ClientInfo,
        _: &'a Credentials<'a>,
    ) -> BoxFuture<'a, Decision> {
        ready(Decision::Allow)
    }
}

/// Allows every login, counting them.
#[derive(Default)]
struct CountLogins(std::sync::atomic::AtomicUsize);

impl Plugin for CountLogins {
    fn name(&self) -> &str {
        "count-logins"
    }

    fn authenticate<'a>(
        &'a self,
        _: &'a ClientInfo,
        _: &'a Credentials<'a>,
    ) -> BoxFuture<'a, Decision> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        ready(Decision::Allow)
    }
}

/// Bans `late` while the broker checks its will's topic, which comes after
/// the CONNECT ban check and before the client id is claimed.
struct BansDuringConnect(Arc<std::sync::OnceLock<Admin>>);

impl Plugin for BansDuringConnect {
    fn name(&self) -> &str {
        "bans-during-connect"
    }

    fn authorize<'a>(
        &'a self,
        _: &'a ClientInfo,
        topic: &'a str,
        _: Access,
    ) -> BoxFuture<'a, Decision> {
        Box::pin(async move {
            if topic == "wills/late" {
                let admin = self.0.get().expect("set before clients connect");
                let ban = NewBan::new(BanKey::new(BanKind::ClientId, "late"));
                admin.add_ban(ban).await.unwrap();
            }
            Decision::Allow
        })
    }

    fn authorize_control<'a>(&'a self, _: &'a ClientInfo, _: &'a str) -> BoxFuture<'a, Decision> {
        ready(Decision::Allow)
    }
}

/// Authenticates a client as `alice` when it connects and as `mallory` when
/// it authenticates again, in one step.
struct SwitchUser;

struct SwitchExchange(&'static str);

impl Plugin for SwitchUser {
    fn name(&self) -> &str {
        "switch-user"
    }

    fn begin_auth(&self, client: &ClientInfo, _: &str) -> Option<Box<dyn AuthExchange>> {
        let user = if client.username.is_some() {
            "mallory"
        } else {
            "alice"
        };
        Some(Box::new(SwitchExchange(user)))
    }
}

impl AuthExchange for SwitchExchange {
    fn step<'a>(&'a mut self, _: Option<&'a Bytes>) -> BoxFuture<'a, AuthResult> {
        ready(AuthResult::Success {
            username: Some(self.0.into()),
            data: None,
        })
    }
}

/// MQTT 5, clean start, keep alive 60.
fn connect_packet(client_id: &str) -> ConnectPacket {
    ConnectPacket {
        protocol_version: ProtocolVersion::V5,
        clean_start: true,
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
        packet.encode(&mut encoded, self.version).unwrap();
        self.stream.write_all(&encoded).await.unwrap();
    }

    async fn receive(&mut self) -> Option<ControlPacket> {
        loop {
            if let Ok(packet) = ControlPacket::parse(&mut self.buffer, self.version) {
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

    /// Publishes at QoS 1 and returns the PUBACK reason.
    async fn publish(&mut self, topic: &str, payload: &'static [u8], retain: bool) -> ReasonCode {
        self.send(ControlPacket::Publish(PublishPacket {
            topic_name: topic.into(),
            packet_id: Some(7),
            qos_level: QoS::AtLeastOnce,
            retain,
            payload: Some(Bytes::from_static(payload)),
            ..Default::default()
        }))
        .await;
        match self.receive().await {
            Some(ControlPacket::PubAck(ack)) => ack.reason,
            other => panic!("Expected PUBACK, got {other:?}"),
        }
    }

    /// Sends a request to the broker API, answered on
    /// `replies/<client id>`, and returns the parsed answer.
    async fn request(&mut self, payload: Bytes) -> Value {
        self.send_request(payload).await;
        let Some(ControlPacket::Publish(answer)) = self.receive().await else {
            panic!("Expected the response");
        };
        serde_json::from_slice(answer.payload.as_deref().unwrap()).unwrap()
    }

    async fn send_request(&mut self, payload: Bytes) {
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

/// A CONNECT whose session outlives the connection by 300 seconds.
fn persistent(client_id: &str) -> ConnectPacket {
    let mut packet = connect_packet(client_id);
    packet.properties = Some(ConnectProperties {
        session_expiry_interval: Some(SessionExpiryInterval::new(300)),
        ..Default::default()
    });
    packet
}

/// A persistent session with a will on `wills/<client id>`.
fn with_will(client_id: &str) -> ConnectPacket {
    let mut packet = persistent(client_id);
    packet.payload.will = Some(Will {
        properties: Some(Default::default()),
        topic: format!("wills/{client_id}"),
        payload: Bytes::from_static(b"gone"),
        ..Default::default()
    });
    packet
}

/// Runs getClient until `done` holds for its data, for two seconds at most.
async fn wait_for(admin: &mut Client, client_id: &str, done: impl Fn(&Value) -> bool) -> Value {
    for _ in 0..100 {
        let response = admin
            .run(json!({"command": "getClient", "clientid": client_id}))
            .await;
        if done(&response["data"]) {
            return response["data"].clone();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("`{client_id}` never got there");
}

/// The reason code of the CONNACK `packet` gets on a new connection, sent
/// after `header`, such as a PROXY header.
async fn connack_reason(addr: SocketAddr, header: &[u8], packet: ConnectPacket) -> ReasonCode {
    let stream = TcpStream::connect(addr).await.unwrap();
    connack_reason_on(stream, header, packet).await
}

async fn connack_reason_on<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    header: &[u8],
    packet: ConnectPacket,
) -> ReasonCode {
    let version = packet.protocol_version;
    let mut bytes = BytesMut::from(header);
    ControlPacket::Connect(packet)
        .encode(&mut bytes, version)
        .unwrap();
    stream.write_all(&bytes).await.unwrap();
    let mut buffer = BytesMut::new();
    loop {
        if let Ok(packet) = ControlPacket::parse(&mut buffer, version) {
            let ControlPacket::ConnAck(connack) = packet else {
                panic!("Expected CONNACK, got {packet:?}");
            };
            return connack.reason_code;
        }
        let read = timeout(Duration::from_secs(2), stream.read_buf(&mut buffer)).await;
        assert!(
            matches!(read, Ok(Ok(n)) if n > 0),
            "closed without a CONNACK"
        );
    }
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

/// Allows every login and every topic, and says nothing about `$CONTROL`.
struct AllowTopics;

impl Plugin for AllowTopics {
    fn name(&self) -> &str {
        "allow-topics"
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

#[tokio::test]
async fn allowing_every_topic_grants_no_admin_commands() {
    let (addr, _) = start(config(vec![Arc::new(AllowTopics)])).await;
    let mut client = admin_client(addr).await;
    let request = br#"{"commands":[{"command":"listClients"}]}"#;
    assert_eq!(
        client.publish("$CONTROL/broker/v1", request, false).await,
        ReasonCode::NotAuthorized
    );
    assert_eq!(
        client.publish("$CONTROL/other/v1", b"x", false).await,
        ReasonCode::NotAuthorized
    );
}

#[tokio::test]
async fn in_process_clients_run_every_command() {
    let (connector, local_listener) = LocalConnector::new();
    let config = config(Vec::new());
    start_with(config, |builder| builder.local_listener(local_listener)).await;
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
    let config = config(Vec::new());
    let listeners = vec![websocket];
    let (addr, admin) = start_with(config, |builder| builder.listeners(listeners)).await;

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

#[tokio::test]
async fn clients_are_listed_with_their_state() {
    let (addr, _) = start(config(vec![Arc::new(AllowAll)])).await;
    let mut admin = admin_client(addr).await;
    let mut device = Client::connect_with(addr, persistent("device")).await;
    device.subscribe("a/#").await;

    let all = admin.run(json!({"command": "listClients"})).await;
    assert_eq!(all["data"]["totalCount"], 2);
    // Sorted by client id
    assert_eq!(all["data"]["clients"][0]["clientid"], "admin");
    let entry = &all["data"]["clients"][1];
    assert_eq!(entry["clientid"], "device");
    assert_eq!(entry["username"], Value::Null);
    assert_eq!(entry["connected"], true);
    assert!(entry["address"].as_str().unwrap().starts_with("127.0.0.1:"));
    assert_eq!(entry["listener"], addr.to_string());
    assert_eq!(entry["protocolVersion"], 5);
    assert!(entry["connectedSince"].is_u64());
    assert!(entry.get("disconnectedSince").is_none());
    assert_eq!(entry["subscriptions"], 1);
    assert_eq!(entry["queued"], 0);
    assert_eq!(entry["inflight"], 0);

    let total = |response: Value| response["data"]["totalCount"].as_u64().unwrap();
    let by_prefix = json!({"command": "listClients", "clientidPrefix": "dev"});
    assert_eq!(total(admin.run(by_prefix).await), 1);
    let local = json!({"command": "listClients", "address": "127.0.0.0/8"});
    assert_eq!(total(admin.run(local).await), 2);
    let elsewhere = json!({"command": "listClients", "address": "10.0.0.0/8"});
    assert_eq!(total(admin.run(elsewhere).await), 0);
    let offline = json!({"command": "listClients", "connected": false});
    assert_eq!(total(admin.run(offline).await), 0);
    let by_user = json!({"command": "listClients", "username": "nobody"});
    assert_eq!(total(admin.run(by_user).await), 0);
    let page = admin
        .run(json!({"command": "listClients", "offset": 1, "count": 1}))
        .await;
    assert_eq!(page["data"]["clients"].as_array().unwrap().len(), 1);
    assert_eq!(page["data"]["clients"][0]["clientid"], "device");
    let bad = json!({"command": "listClients", "address": "nowhere"});
    assert_eq!(admin.run(bad).await["code"], "invalidArgument");
}

#[tokio::test]
async fn client_details_follow_the_connection() {
    let (addr, _) = start(config(vec![Arc::new(AllowAll)])).await;
    let mut admin = admin_client(addr).await;
    let mut device = Client::connect_with(addr, persistent("device")).await;
    device.subscribe("a/#").await;
    device.publish("t", b"hello", false).await;

    let details = wait_for(&mut admin, "device", |d| d["messagesIn"] == 1).await;
    assert_eq!(details["connected"], true);
    assert_eq!(details["keepAlive"], 60);
    assert_eq!(details["cleanStart"], true);
    assert_eq!(details["sessionExpiry"], 300);
    assert!(details.get("sessionExpiresIn").is_none());
    assert_eq!(details["willPending"], false);
    assert_eq!(details["messagesOut"], 0);
    assert!(details["bytesIn"].as_u64().unwrap() > 0);
    assert!(details["bytesOut"].as_u64().unwrap() > 0);
    assert_eq!(details["subscriptions"][0]["filter"], "a/#");
    assert_eq!(details["subscriptions"][0]["qos"], 1);
    assert_eq!(details["subscriptions"][0]["noLocal"], false);

    drop(device);
    let details = wait_for(&mut admin, "device", |d| d["connected"] == false).await;
    assert!(details["disconnectedSince"].is_u64());
    assert!(details.get("connectedSince").is_none());
    let expires_in = details["sessionExpiresIn"].as_u64().unwrap();
    assert!((290..=300).contains(&expires_in), "{expires_in}");

    let missing = admin
        .run(json!({"command": "getClient", "clientid": "nobody"}))
        .await;
    assert_eq!(missing["code"], "notFound");
}

#[tokio::test]
async fn client_details_show_the_will_and_the_messages_sent() {
    let (addr, _) = start(config(vec![Arc::new(AllowAll)])).await;
    let mut admin = admin_client(addr).await;
    let mut device = Client::connect_with(addr, with_will("device")).await;
    device.subscribe("news").await;
    let mut publisher = Client::connect(addr, "publisher").await;
    publisher.publish("news", b"hello", false).await;
    assert!(matches!(
        device.receive().await,
        Some(ControlPacket::Publish(_))
    ));

    let details = wait_for(&mut admin, "device", |d| d["messagesOut"] == 1).await;
    assert_eq!(details["willPending"], true);
}

#[tokio::test]
async fn messages_lost_to_a_full_queue_are_counted_for_the_client() {
    let mut config = config(vec![Arc::new(AllowAll)]);
    config.limits.queue.max_messages = 2;
    let (addr, server_admin) = start(config).await;
    let mut admin = admin_client(addr).await;
    let mut device = Client::connect_with(addr, persistent("device")).await;
    device.subscribe("jobs").await;
    drop(device);
    wait_for(&mut admin, "device", |d| d["connected"] == false).await;

    let mut publisher = Client::connect(addr, "publisher").await;
    for _ in 0..5 {
        publisher.publish("jobs", b"job", false).await;
    }

    let details = wait_for(&mut admin, "device", |d| d["dropped"] == 3).await;
    assert_eq!(details["queued"], 2);
    let all = admin.run(json!({"command": "listClients"})).await;
    let entry = &all["data"]["clients"][1];
    assert_eq!(entry["clientid"], "device");
    assert_eq!(entry["dropped"], 3);
    let stats = server_admin.info().await.stats;
    assert_eq!(stats["publish/messages/dropped"], 3);
    assert_eq!(stats["publish/messages/dropped/queue_full"], 3);
    assert_eq!(stats["publish/messages/dropped/queue_full_qos0"], 0);
}

/// Allows everything but reading `a/secret`.
struct HideSecret;

impl Plugin for HideSecret {
    fn name(&self) -> &str {
        "hide-secret"
    }

    fn authorize_control<'a>(&'a self, _: &'a ClientInfo, _: &'a str) -> BoxFuture<'a, Decision> {
        ready(Decision::Allow)
    }

    fn authorize<'a>(
        &'a self,
        _: &'a ClientInfo,
        topic: &'a str,
        access: Access,
    ) -> BoxFuture<'a, Decision> {
        ready(match (topic, access) {
            ("a/secret", Access::Read) => Decision::Deny,
            _ => Decision::Allow,
        })
    }
}

#[tokio::test]
async fn messages_the_subscriber_may_not_read_are_counted() {
    let (addr, server_admin) = start(config(vec![Arc::new(HideSecret)])).await;
    let mut admin = admin_client(addr).await;
    let mut device = Client::connect(addr, "device").await;
    device.subscribe("a/#").await;
    let mut publisher = Client::connect(addr, "publisher").await;
    publisher.publish("a/secret", b"hidden", false).await;
    publisher.publish("a/public", b"shown", false).await;
    let Some(ControlPacket::Publish(shown)) = device.receive().await else {
        panic!("Expected the readable message");
    };
    assert_eq!(shown.topic_name, "a/public");

    let details = wait_for(&mut admin, "device", |d| d["dropped"] == 1).await;
    assert_eq!(details["messagesOut"], 1);
    let stats = server_admin.info().await.stats;
    assert_eq!(stats["publish/messages/dropped/not_authorized"], 1);
}

#[tokio::test]
async fn disconnect_client_sends_administrative_action_and_the_will() {
    let (addr, _) = start(config(vec![Arc::new(AllowAll)])).await;
    let mut admin = admin_client(addr).await;
    let mut watcher = Client::connect(addr, "watcher").await;
    watcher.subscribe("wills/#").await;
    let mut device = Client::connect_with(addr, with_will("device")).await;

    let response = admin
        .run(json!({"command": "disconnectClient", "clientid": "device"}))
        .await;
    assert!(response.get("error").is_none(), "{response}");
    let Some(ControlPacket::Disconnect(disconnect)) = device.receive().await else {
        panic!("Expected DISCONNECT");
    };
    assert_eq!(disconnect.reason, ReasonCode::AdministrativeAction);
    let Some(ControlPacket::Publish(will)) = watcher.receive().await else {
        panic!("Expected the will");
    };
    assert_eq!(will.topic_name, "wills/device");

    // The session stays
    let details = wait_for(&mut admin, "device", |d| d["connected"] == false).await;
    assert_eq!(details["clientid"], "device");
    let again = admin
        .run(json!({"command": "disconnectClient", "clientid": "device"}))
        .await;
    assert_eq!(again["code"], "notFound");
}

#[tokio::test]
async fn disconnect_client_can_drop_the_will_and_pick_the_reason() {
    let (addr, _) = start(config(vec![Arc::new(AllowAll)])).await;
    let mut admin = admin_client(addr).await;
    let mut watcher = Client::connect(addr, "watcher").await;
    watcher.subscribe("wills/#").await;
    let mut device = Client::connect_with(addr, with_will("device")).await;

    let request = json!({
        "command": "disconnectClient", "clientid": "device", "sendWill": false, "reasonCode": 0x89,
    });
    assert!(admin.run(request).await.get("error").is_none());
    let Some(ControlPacket::Disconnect(disconnect)) = device.receive().await else {
        panic!("Expected DISCONNECT");
    };
    assert_eq!(disconnect.reason, ReasonCode::ServerBusy);
    assert!(watcher.receive().await.is_none());
    let details = wait_for(&mut admin, "device", |d| d["connected"] == false).await;
    assert_eq!(details["willPending"], false);

    // 0x8A is a reason code, but not one DISCONNECT may carry
    for code in [0x00, 0x05, 0x8a, 0xff] {
        let request =
            json!({"command": "disconnectClient", "clientid": "watcher", "reasonCode": code});
        assert_eq!(
            admin.run(request).await["code"],
            "invalidArgument",
            "{code}"
        );
    }
}

#[tokio::test]
async fn delete_session_conflicts_until_asked_to_disconnect() {
    let (addr, _) = start(config(vec![Arc::new(AllowAll)])).await;
    let mut admin = admin_client(addr).await;
    let mut device = Client::connect_with(addr, persistent("device")).await;
    device.subscribe("a/#").await;
    let subscriptions = |info: &Value| info["data"]["stats"]["subscriptions/count"].clone();
    let info = admin.run(json!({"command": "getBrokerInfo"})).await;
    assert_eq!(subscriptions(&info), 2);

    let refused = admin
        .run(json!({"command": "deleteSession", "clientid": "device"}))
        .await;
    assert_eq!(refused["code"], "conflict");
    let deleted = admin
        .run(json!({"command": "deleteSession", "clientid": "device", "disconnect": true}))
        .await;
    assert!(deleted.get("error").is_none(), "{deleted}");
    let Some(ControlPacket::Disconnect(disconnect)) = device.receive().await else {
        panic!("Expected DISCONNECT");
    };
    assert_eq!(disconnect.reason, ReasonCode::AdministrativeAction);

    let missing = admin
        .run(json!({"command": "getClient", "clientid": "device"}))
        .await;
    assert_eq!(missing["code"], "notFound");
    let info = admin.run(json!({"command": "getBrokerInfo"})).await;
    assert_eq!(subscriptions(&info), 1);
    let again = admin
        .run(json!({"command": "deleteSession", "clientid": "device"}))
        .await;
    assert_eq!(again["code"], "notFound");
}

#[tokio::test]
async fn a_client_cannot_delete_its_own_session() {
    let (addr, _) = start(config(vec![Arc::new(AllowAll)])).await;
    let mut admin = admin_client(addr).await;

    let refused = admin
        .run(json!({"command": "deleteSession", "clientid": "admin", "disconnect": true}))
        .await;
    assert_eq!(refused["code"], "conflict");
    let info = admin.run(json!({"command": "getBrokerInfo"})).await;
    assert!(info.get("error").is_none(), "{info}");
}

/// Reads until the broker closes the connection, for `wait` at most.
async fn closed_within(client: &mut Client, wait: Duration) {
    let closed = timeout(wait, async {
        let mut rest = BytesMut::new();
        while matches!(client.stream.read_buf(&mut rest).await, Ok(n) if n > 0) {}
    })
    .await;
    assert!(closed.is_ok(), "`{}` was never closed", client.client_id);
}

#[tokio::test]
async fn clients_deleting_each_other_leave_both_client_ids_usable() {
    let (addr, _) = start(config(vec![Arc::new(AllowAll)])).await;
    let mut one = Client::connect(addr, "one").await;
    one.subscribe("replies/one").await;
    let mut two = Client::connect(addr, "two").await;
    two.subscribe("replies/two").await;

    let delete = |target: &str| {
        let command = json!({"command": "deleteSession", "clientid": target, "disconnect": true});
        Bytes::from(json!({ "commands": [command] }).to_string())
    };
    one.send_request(delete("two")).await;
    two.send_request(delete("one")).await;

    // Each waits for the other at most 5 seconds before answering, then
    // closes as asked
    closed_within(&mut one, Duration::from_secs(7)).await;
    closed_within(&mut two, Duration::from_secs(7)).await;
    Client::connect(addr, "one").await;
    Client::connect(addr, "two").await;
}

#[tokio::test]
async fn delete_session_removes_a_disconnected_session() {
    let (addr, _) = start(config(vec![Arc::new(AllowAll)])).await;
    let mut admin = admin_client(addr).await;
    let device = Client::connect_with(addr, persistent("device")).await;
    drop(device);
    wait_for(&mut admin, "device", |d| d["connected"] == false).await;

    let deleted = admin
        .run(json!({"command": "deleteSession", "clientid": "device"}))
        .await;
    assert!(deleted.get("error").is_none(), "{deleted}");
    let missing = admin
        .run(json!({"command": "getClient", "clientid": "device"}))
        .await;
    assert_eq!(missing["code"], "notFound");
}

#[tokio::test]
async fn refused_delete_leaves_the_connected_client_alone() {
    let (addr, admin) = start(config(vec![Arc::new(AllowAll)])).await;
    let mut device = Client::connect_with(addr, persistent("device")).await;

    for _ in 0..3 {
        let refused = admin
            .delete_session(&SessionKey::unmounted("device"), false)
            .await;
        assert_eq!(refused.unwrap_err().code, ErrorCode::Conflict);
    }
    assert!(device.receive().await.is_none());
    assert!(
        admin
            .client(&SessionKey::unmounted("device"))
            .await
            .unwrap()
            .connected
    );
}

/// Hears of the sessions that expire.
#[derive(Default)]
struct ExpiryRecorder(std::sync::Mutex<Vec<String>>);

impl Plugin for ExpiryRecorder {
    fn name(&self) -> &str {
        "expiry-recorder"
    }

    fn on_event<'a>(&'a self, event: &'a mercurio_server::plugin::Event) -> BoxFuture<'a, ()> {
        if let mercurio_server::plugin::Event::SessionExpired { client_id, .. } = event {
            self.0.lock().unwrap().push(client_id.clone());
        }
        ready(())
    }
}

#[tokio::test]
async fn a_deleted_session_does_not_expire_later() {
    let recorder = Arc::new(ExpiryRecorder::default());
    let (addr, admin) = start(config(vec![Arc::new(AllowAll), recorder.clone()])).await;
    let mut packet = persistent("device");
    packet.properties = Some(ConnectProperties {
        session_expiry_interval: Some(SessionExpiryInterval::new(1)),
        ..Default::default()
    });
    let device = Client::connect_with(addr, packet).await;
    drop(device);
    let mut watcher = admin_client(addr).await;
    wait_for(&mut watcher, "device", |d| d["connected"] == false).await;

    admin
        .delete_session(&SessionKey::unmounted("device"), false)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;

    assert!(recorder.0.lock().unwrap().is_empty());
    assert_eq!(admin.info().await.stats["clients/expired"], 0);
}

#[tokio::test]
async fn deleting_a_connected_session_does_not_expire_it_later() {
    let recorder = Arc::new(ExpiryRecorder::default());
    let (addr, admin) = start(config(vec![Arc::new(AllowAll), recorder.clone()])).await;
    let mut packet = persistent("device");
    packet.properties = Some(ConnectProperties {
        session_expiry_interval: Some(SessionExpiryInterval::new(1)),
        ..Default::default()
    });
    let mut device = Client::connect_with(addr, packet).await;

    admin
        .delete_session(&SessionKey::unmounted("device"), true)
        .await
        .unwrap();
    assert!(matches!(
        device.receive().await,
        Some(ControlPacket::Disconnect(_))
    ));
    tokio::time::sleep(Duration::from_millis(1500)).await;

    assert!(recorder.0.lock().unwrap().is_empty());
    assert_eq!(admin.info().await.stats["clients/expired"], 0);
    assert!(admin
        .client(&SessionKey::unmounted("device"))
        .await
        .is_err());
}

#[tokio::test]
async fn disconnect_client_closes_mqtt311_connections() {
    let (addr, admin) = start(config(vec![Arc::new(AllowAll)])).await;
    let mut packet = connect_packet("old");
    packet.protocol_version = ProtocolVersion::V3_1_1;
    packet.properties = None;
    packet.clean_start = false;
    let mut old = Client::connect_with(addr, packet).await;

    admin
        .disconnect(&SessionKey::unmounted("old"), Default::default())
        .await
        .unwrap();

    let mut rest = BytesMut::new();
    let read = timeout(Duration::from_secs(2), old.stream.read_buf(&mut rest))
        .await
        .expect("the connection should close");
    assert_eq!(read.unwrap(), 0, "no packet before the close");
    assert!(
        !admin
            .client(&SessionKey::unmounted("old"))
            .await
            .unwrap()
            .connected
    );
}

#[tokio::test]
async fn sys_messages_are_left_to_sys_filters() {
    let mut config = config(vec![Arc::new(AllowAll)]);
    config.sys_interval = Some(Duration::from_secs(60));
    let (addr, _) = start(config).await;
    let mut admin = admin_client(addr).await;
    let mut publisher = Client::connect(addr, "publisher").await;
    publisher.publish("r/1", b"hello", true).await;
    let version = json!({"command": "listRetained", "filter": "$SYS/broker/version"});
    let mut published = false;
    for _ in 0..100 {
        if admin.run(version.clone()).await["data"]["totalCount"] == 1 {
            published = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(published, "the broker never published its version");

    let all = admin.run(json!({"command": "listRetained"})).await;
    assert_eq!(all["data"]["totalCount"], 1, "{all}");
    assert_eq!(all["data"]["messages"][0]["topic"], "r/1");
    let deleted = admin
        .run(json!({"command": "deleteRetained", "filter": "#"}))
        .await;
    assert_eq!(deleted["data"]["deleted"], 1);
    let sys = admin
        .run(json!({"command": "listRetained", "filter": "$SYS/#"}))
        .await;
    let topics: Vec<&str> = sys["data"]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["topic"].as_str().unwrap())
        .collect();
    assert!(topics.contains(&"$SYS/broker/version"), "{topics:?}");
}

#[tokio::test]
async fn retained_messages_are_listed_read_and_deleted() {
    let (addr, _) = start(config(vec![Arc::new(AllowAll)])).await;
    let mut admin = admin_client(addr).await;
    let mut publisher = Client::connect(addr, "publisher").await;
    publisher.publish("r/1", b"hello", true).await;
    publisher.publish("r/2", &[0xff, 0xfe], true).await;
    publisher.publish("s/1", b"other", true).await;

    let listed = admin
        .run(json!({"command": "listRetained", "filter": "r/#"}))
        .await;
    assert_eq!(listed["data"]["totalCount"], 2);
    let messages = &listed["data"]["messages"];
    assert_eq!(messages[0]["topic"], "r/1");
    assert_eq!(messages[0]["qos"], 1);
    assert_eq!(messages[0]["size"], 5);
    assert_eq!(messages[0]["preview"], "hello");
    assert_eq!(messages[0]["expiresIn"], Value::Null);
    assert_eq!(messages[1]["topic"], "r/2");
    assert_eq!(messages[1]["preview"], Value::Null);

    let text = admin
        .run(json!({"command": "getRetained", "topic": "r/1"}))
        .await;
    assert_eq!(text["data"]["payload"], "hello");
    assert!(text["data"].get("payloadBase64").is_none());
    assert!(text["data"].get("expiresIn").is_some_and(Value::is_null));
    let binary = admin
        .run(json!({"command": "getRetained", "topic": "r/2"}))
        .await;
    assert_eq!(binary["data"]["payloadBase64"], "//4=");
    assert!(binary["data"].get("payload").is_none());
    let missing = admin
        .run(json!({"command": "getRetained", "topic": "r/3"}))
        .await;
    assert_eq!(missing["code"], "notFound");
    let wildcard = admin
        .run(json!({"command": "getRetained", "topic": "r/#"}))
        .await;
    assert_eq!(wildcard["code"], "invalidArgument");

    let deleted = admin
        .run(json!({"command": "deleteRetained", "filter": "r/#"}))
        .await;
    assert_eq!(deleted["data"]["deleted"], 2);
    let rest = admin.run(json!({"command": "listRetained"})).await;
    assert_eq!(rest["data"]["totalCount"], 1);
    assert_eq!(rest["data"]["messages"][0]["topic"], "s/1");

    // New subscribers get none of the deleted ones
    let mut late = Client::connect(addr, "late").await;
    late.subscribe("r/#").await;
    assert!(late.receive().await.is_none());

    let shared = admin
        .run(json!({"command": "deleteRetained", "filter": "$share/g/r/#"}))
        .await;
    assert_eq!(shared["code"], "invalidArgument");
}

#[tokio::test]
async fn subscriptions_are_listed_with_their_subscribers() {
    let (addr, _) = start(config(vec![Arc::new(AllowAll)])).await;
    let mut admin = admin_client(addr).await;
    let mut one = Client::connect(addr, "one").await;
    one.subscribe("a/#").await;
    one.subscribe("$share/g/b").await;
    let mut two = Client::connect(addr, "two").await;
    two.subscribe("a/#").await;

    let all = admin.run(json!({"command": "listSubscriptions"})).await;
    assert_eq!(all["data"]["totalCount"], 3);
    assert_eq!(
        all["data"]["subscriptions"],
        json!([
            {"filter": "$share/g/b", "subscribers": 1},
            {"filter": "a/#", "subscribers": 2},
            {"filter": "replies/admin", "subscribers": 1},
        ])
    );
    let prefixed = admin
        .run(json!({"command": "listSubscriptions", "filterPrefix": "a/"}))
        .await;
    assert_eq!(prefixed["data"]["totalCount"], 1);
    assert_eq!(prefixed["data"]["subscriptions"][0]["filter"], "a/#");
    let paged = admin
        .run(json!({"command": "listSubscriptions", "offset": 1, "count": 1}))
        .await;
    assert_eq!(paged["data"]["totalCount"], 3);
    assert_eq!(
        paged["data"]["subscriptions"],
        json!([{"filter": "a/#", "subscribers": 2}])
    );

    let subscribers = admin
        .run(json!({"command": "getSubscribers", "filter": "a/#"}))
        .await;
    let list = &subscribers["data"]["subscribers"];
    assert_eq!(list[0]["clientid"], "one");
    assert_eq!(list[0]["qos"], 1);
    assert_eq!(list[0]["noLocal"], false);
    assert_eq!(list[1]["clientid"], "two");
    let shared = admin
        .run(json!({"command": "getSubscribers", "filter": "$share/g/b"}))
        .await;
    assert_eq!(shared["data"]["subscribers"][0]["clientid"], "one");
    let none = admin
        .run(json!({"command": "getSubscribers", "filter": "zzz"}))
        .await;
    assert_eq!(none["data"]["subscribers"], json!([]));
    let invalid = admin
        .run(json!({"command": "getSubscribers", "filter": "a/#/b"}))
        .await;
    assert_eq!(invalid["code"], "invalidArgument");
}

/// Reports the default status.
struct Custom;

impl Plugin for Custom {
    fn name(&self) -> &str {
        "custom"
    }
}

#[tokio::test]
async fn plugins_are_listed_in_chain_order() {
    let acl = "topic readwrite replies/#\ntopic write $CONTROL/broker/v1/#\n";
    let plugins: Vec<Arc<dyn Plugin>> =
        vec![Arc::new(AclFile::parse(acl).unwrap()), Arc::new(Custom)];
    let (addr, _) = start(config(plugins)).await;
    let mut admin = admin_client(addr).await;
    let listed = admin.run(json!({"command": "listPlugins"})).await;
    assert_eq!(listed["data"]["totalCount"], 2);
    assert_eq!(
        listed["data"]["plugins"],
        json!([
            {"name": "acl-file", "kind": "builtin", "hooks": ["authorize", "authorize_control"]},
            {"name": "custom", "kind": "rust", "hooks": []},
        ])
    );
}

#[tokio::test]
async fn bans_are_added_listed_and_removed() {
    let (addr, _) = start(config(vec![Arc::new(AllowAll)])).await;
    let mut admin = admin_client(addr).await;
    let ok = |response: Value| assert!(response.get("error").is_none(), "{response}");
    ok(admin
        .run(json!({
            "command": "addBan", "type": "clientid", "value": "spammer",
            "duration": 3600, "reason": "spam",
        }))
        .await);
    ok(admin
        .run(json!({"command": "addBan", "type": "username", "value": "guest", "prefix": true}))
        .await);
    ok(admin
        .run(json!({"command": "addBan", "type": "address", "value": "10.1.2.3/8"}))
        .await);

    let listed = admin.run(json!({"command": "listBans"})).await;
    assert_eq!(listed["data"]["totalCount"], 3);
    let bans = listed["data"]["bans"].as_array().unwrap();
    assert_eq!(bans[0]["type"], "clientid");
    assert_eq!(bans[0]["value"], "spammer");
    assert_eq!(bans[0]["prefix"], false);
    assert_eq!(bans[0]["reason"], "spam");
    let created = bans[0]["createdAt"].as_u64().unwrap();
    assert!(created > 1_700_000_000);
    assert_eq!(bans[0]["expiresAt"].as_u64(), Some(created + 3600));
    assert_eq!(bans[1]["type"], "username");
    assert_eq!(bans[1]["prefix"], true);
    assert_eq!(bans[1]["reason"], Value::Null);
    assert_eq!(bans[1]["expiresAt"], Value::Null);
    // An address ban is kept as the block it names
    assert_eq!(bans[2]["type"], "address");
    assert_eq!(bans[2]["value"], "10.0.0.0/8");

    // The same type, value and prefix replace the ban, created when it first was
    ok(admin
        .run(json!({"command": "addBan", "type": "clientid", "value": "spammer"}))
        .await);
    let listed = admin.run(json!({"command": "listBans", "count": 1})).await;
    assert_eq!(listed["data"]["totalCount"], 3);
    let spammer = &listed["data"]["bans"][0];
    assert_eq!(spammer["reason"], Value::Null);
    assert_eq!(spammer["expiresAt"], Value::Null);
    assert_eq!(spammer["createdAt"].as_u64(), Some(created));

    // The prefix flag is part of what names a ban
    let missing = admin
        .run(json!({"command": "removeBan", "type": "username", "value": "guest"}))
        .await;
    assert_eq!(missing["code"], "notFound");
    ok(admin
        .run(json!({"command": "removeBan", "type": "username", "value": "guest", "prefix": true}))
        .await);
    ok(admin
        .run(json!({"command": "removeBan", "type": "address", "value": "10.9.9.9/8"}))
        .await);
    let listed = admin.run(json!({"command": "listBans"})).await;
    assert_eq!(listed["data"]["totalCount"], 1);
    assert_eq!(listed["data"]["bans"][0]["value"], "spammer");
}

#[tokio::test]
async fn unknown_arguments_are_refused_by_name() {
    let (addr, _) = start(config(vec![Arc::new(AllowAll)])).await;
    let mut admin = admin_client(addr).await;
    for (request, argument) in [
        // Would otherwise ban for ever
        (
            json!({"command": "addBan", "type": "clientid", "value": "c", "durration": 60}),
            "durration",
        ),
        (json!({"command": "listClients", "cuont": 1}), "cuont"),
        (json!({"command": "listRetained", "ofset": 1}), "ofset"),
        (
            json!({"command": "getBrokerInfo", "verbose": true}),
            "verbose",
        ),
    ] {
        let response = admin.run(request.clone()).await;
        assert_eq!(response["code"], "invalidArgument", "{request}");
        let error = response["error"].as_str().unwrap();
        assert!(error.contains(argument), "{error}");
    }
    let listed = admin.run(json!({"command": "listBans"})).await;
    assert_eq!(listed["data"]["totalCount"], 0);
}

#[tokio::test]
async fn too_many_commands_are_refused_together() {
    let (addr, _) = start(config(vec![Arc::new(AllowAll)])).await;
    let mut admin = admin_client(addr).await;
    let ban = |n: usize| json!({"command": "addBan", "type": "clientid", "value": format!("c{n}")});
    let envelope = json!({ "commands": (0..101).map(ban).collect::<Vec<_>>() });
    let reply = admin.request(Bytes::from(envelope.to_string())).await;
    let responses = reply["responses"].as_array().unwrap();
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0]["code"], "invalidArgument");
    assert!(responses[0]["error"].as_str().unwrap().contains("100"));
    let listed = admin.run(json!({"command": "listBans"})).await;
    assert_eq!(listed["data"]["totalCount"], 0);

    let envelope = json!({ "commands": (0..100).map(ban).collect::<Vec<_>>() });
    let reply = admin.request(Bytes::from(envelope.to_string())).await;
    assert_eq!(reply["responses"].as_array().unwrap().len(), 100);
}

#[tokio::test]
async fn malformed_bans_are_invalid_arguments() {
    let (addr, _) = start(config(vec![Arc::new(AllowAll)])).await;
    let mut admin = admin_client(addr).await;
    for request in [
        json!({"command": "addBan", "type": "address", "value": "10.0.0.0/8", "prefix": true}),
        json!({"command": "addBan", "type": "address", "value": "example.com"}),
        json!({"command": "addBan", "type": "ip", "value": "10.0.0.1"}),
        json!({"command": "addBan", "type": "clientid", "value": ""}),
        json!({"command": "addBan", "type": "clientid", "value": "c", "duration": 0}),
        json!({"command": "addBan", "type": "clientid", "value": "c", "duration": -1}),
        json!({"command": "addBan", "type": "clientid", "value": "c", "duration": 4_294_967_296u64}),
        json!({"command": "addBan", "type": "clientid"}),
        json!({"command": "removeBan", "type": "address", "value": "nope"}),
        json!({"command": "listBans", "count": 1001}),
    ] {
        let response = admin.run(request.clone()).await;
        assert_eq!(response["code"], "invalidArgument", "{request}");
    }
    let listed = admin.run(json!({"command": "listBans"})).await;
    assert_eq!(listed["data"]["totalCount"], 0);
}

#[tokio::test]
async fn banned_clients_are_refused_at_connect() {
    let (addr, admin) = start(config(vec![Arc::new(AllowAll)])).await;
    admin
        .add_ban(NewBan::new(BanKey::new(BanKind::ClientId, "bad")))
        .await
        .unwrap();

    assert_eq!(
        connack_reason(addr, b"", connect_packet("bad")).await,
        ReasonCode::Banned
    );
    // MQTT 3.1.1 has no Banned: Not Authorized, 0x05
    let mut old = connect_packet("bad");
    old.protocol_version = ProtocolVersion::V3_1_1;
    old.properties = None;
    assert_eq!(
        connack_reason(addr, b"", old).await,
        ReasonCode::NotAuthorized
    );
    // An exact ban matches nothing longer
    assert_eq!(
        connack_reason(addr, b"", connect_packet("bad2")).await,
        ReasonCode::Success
    );

    let mut key = BanKey::new(BanKind::ClientId, "bad");
    key.prefix = true;
    admin.add_ban(NewBan::new(key)).await.unwrap();
    assert_eq!(
        connack_reason(addr, b"", connect_packet("bad2")).await,
        ReasonCode::Banned
    );
}

#[tokio::test]
async fn user_name_bans_match_the_verified_user() {
    let mut config = config(vec![Arc::new(AllowAll)]);
    config.auth.require_auth = true;
    config.auth.allow_anonymous = true;
    let (addr, admin) = start(config).await;
    let mut key = BanKey::new(BanKind::Username, "guest");
    key.prefix = true;
    admin.add_ban(NewBan::new(key)).await.unwrap();

    let as_user = |client_id: &str, user: &str| {
        let mut packet = connect_packet(client_id);
        packet.payload.user_name = Some(user.into());
        packet.payload.password = Some(Bytes::from_static(b"secret"));
        packet
    };
    assert_eq!(
        connack_reason(addr, b"", as_user("a", "guest-7")).await,
        ReasonCode::Banned
    );
    assert_eq!(
        connack_reason(addr, b"", as_user("b", "alice")).await,
        ReasonCode::Success
    );
    // An anonymous client has no user name to match
    assert_eq!(
        connack_reason(addr, b"", connect_packet("guest-7")).await,
        ReasonCode::Success
    );
}

/// Sends `header` and `packet` on a new connection and checks that the
/// broker closes it without answering anything.
async fn closed_unanswered(addr: SocketAddr, header: &[u8], packet: ConnectPacket) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let mut bytes = BytesMut::from(header);
    ControlPacket::Connect(packet)
        .encode(&mut bytes, ProtocolVersion::V5)
        .unwrap();
    // The broker may have closed already, which is what is checked below
    let _ = stream.write_all(&bytes).await;
    let mut answer = Vec::new();
    let read = timeout(Duration::from_secs(2), stream.read_to_end(&mut answer)).await;
    assert!(read.is_ok(), "the connection was left open");
    assert_eq!(answer, b"", "the broker answered");
}

#[tokio::test]
async fn address_bans_refuse_before_authentication() {
    let logins = Arc::new(CountLogins::default());
    let mut config = config(vec![logins.clone()]);
    config.auth.require_auth = true;
    let (addr, admin) = start(config).await;
    let key = BanKey::new(BanKind::Address, "127.0.0.0/8");
    admin.add_ban(NewBan::new(key.clone())).await.unwrap();

    let as_alice = || {
        let mut packet = connect_packet("a");
        packet.payload.user_name = Some("alice".into());
        packet.payload.password = Some(Bytes::from_static(b"secret"));
        packet
    };
    closed_unanswered(addr, b"", as_alice()).await;
    assert_eq!(logins.0.load(std::sync::atomic::Ordering::Relaxed), 0);

    admin.remove_ban(&key).await.unwrap();
    assert_eq!(
        connack_reason(addr, b"", as_alice()).await,
        ReasonCode::Success
    );
    assert_eq!(logins.0.load(std::sync::atomic::Ordering::Relaxed), 1);
}

#[tokio::test]
async fn address_bans_match_the_address_a_proxy_reports() {
    let proxied = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxied_addr = proxied.local_addr().unwrap();
    let config = config(vec![Arc::new(AllowAll)]);
    let listeners = vec![{
        let mut listener = ListenerConfig::new(proxied, Transport::Mqtt);
        listener.proxy_protocol = true;
        listener.proxy_from = vec!["127.0.0.1".parse().unwrap()];
        listener
    }];
    let (_, admin) = start_with(config, |builder| builder.listeners(listeners)).await;
    admin
        .add_ban(NewBan::new(BanKey::new(BanKind::Address, "192.0.2.0/24")))
        .await
        .unwrap();

    let from = |ip: &str| format!("PROXY TCP4 {ip} 127.0.0.1 56324 1883\r\n");
    closed_unanswered(
        proxied_addr,
        from("192.0.2.9").as_bytes(),
        connect_packet("a"),
    )
    .await;
    assert_eq!(
        connack_reason(
            proxied_addr,
            from("198.51.100.1").as_bytes(),
            connect_packet("b")
        )
        .await,
        ReasonCode::Success
    );
}

#[tokio::test]
async fn bans_survive_a_restart_on_sqlite() {
    let db = std::env::temp_dir().join(format!("mercurio-server-bans-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&db);
    let open = || Arc::new(SqliteStore::new(&db).unwrap());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = common::broker(listener, config(Vec::new()))
        .storage(open())
        .build()
        .await
        .unwrap();
    let mut key = BanKey::new(BanKind::ClientId, "bad");
    key.prefix = true;
    let mut ban = NewBan::new(key);
    ban.reason = Some("spam".into());
    server.admin().add_ban(ban).await.unwrap();
    drop(server);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = common::broker(listener, config(Vec::new()))
        .storage(open())
        .build()
        .await
        .unwrap();
    let bans = server.admin().bans(Paging::default()).await;
    tokio::spawn(server.run(std::future::pending::<()>()));
    assert_eq!(bans.total_count, 1);
    assert_eq!(bans.items[0].value, "bad");
    assert!(bans.items[0].prefix);
    assert_eq!(bans.items[0].reason.as_deref(), Some("spam"));
    assert_eq!(
        connack_reason(addr, b"", connect_packet("bad-1")).await,
        ReasonCode::Banned
    );
    std::fs::remove_file(&db).unwrap();
}

#[tokio::test]
async fn banned_clients_publish_no_will_and_start_no_session() {
    let (addr, admin) = start(config(vec![Arc::new(AllowAll)])).await;
    let mut watcher = Client::connect(addr, "watcher").await;
    watcher.subscribe("wills/#").await;
    let mut checker = admin_client(addr).await;
    admin
        .add_ban(NewBan::new(BanKey::new(BanKind::ClientId, "device")))
        .await
        .unwrap();

    assert_eq!(
        connack_reason(addr, b"", with_will("device")).await,
        ReasonCode::Banned
    );
    assert!(watcher.receive().await.is_none());
    let details = checker
        .run(json!({"command": "getClient", "clientid": "device"}))
        .await;
    assert_eq!(details["code"], "notFound");
}

#[tokio::test]
async fn address_bans_on_mapped_blocks_apply_to_ipv4_clients() {
    let proxied = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxied_addr = proxied.local_addr().unwrap();
    let config = config(vec![Arc::new(AllowAll)]);
    let listeners = vec![{
        let mut listener = ListenerConfig::new(proxied, Transport::Mqtt);
        listener.proxy_protocol = true;
        listener.proxy_from = vec!["127.0.0.1".parse().unwrap()];
        listener
    }];
    let (_, admin) = start_with(config, |builder| builder.listeners(listeners)).await;
    admin
        .add_ban(NewBan::new(BanKey::new(
            BanKind::Address,
            "::ffff:192.0.2.0/120",
        )))
        .await
        .unwrap();

    assert_eq!(
        admin.bans(Paging::default()).await.items[0].value,
        "192.0.2.0/24"
    );
    closed_unanswered(
        proxied_addr,
        b"PROXY TCP4 192.0.2.9 127.0.0.1 56324 1883\r\n",
        connect_packet("a"),
    )
    .await;
}

#[tokio::test]
async fn a_ban_closes_matching_clients_without_their_will() {
    let (addr, _) = start(config(vec![Arc::new(AllowAll)])).await;
    let mut admin = admin_client(addr).await;
    let mut watcher = Client::connect(addr, "watcher").await;
    watcher.subscribe("wills/#").await;
    let mut device = Client::connect_with(addr, with_will("device-1")).await;
    let mut other = Client::connect(addr, "other").await;

    let response = admin
        .run(json!({"command": "addBan", "type": "clientid", "value": "device-", "prefix": true}))
        .await;
    assert!(response.get("error").is_none(), "{response}");
    let Some(ControlPacket::Disconnect(disconnect)) = device.receive().await else {
        panic!("Expected DISCONNECT");
    };
    assert_eq!(disconnect.reason, ReasonCode::AdministrativeAction);
    assert!(watcher.receive().await.is_none(), "the will is dropped");
    // A client the ban doesn't match stays connected
    other.subscribe("a").await;
    let details = wait_for(&mut admin, "device-1", |d| d["connected"] == false).await;
    assert_eq!(details["willPending"], false);
}

#[tokio::test]
async fn a_client_banned_while_it_connects_is_closed_once_up() {
    let slot = Arc::new(std::sync::OnceLock::new());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let plugin = Arc::new(BansDuringConnect(Arc::clone(&slot)));
    let server = common::broker(listener, config(vec![plugin]))
        .build()
        .await
        .unwrap();
    assert!(slot.set(server.admin()).is_ok());
    tokio::spawn(server.run(std::future::pending::<()>()));
    let mut watcher = Client::connect(addr, "watcher").await;
    watcher.subscribe("wills/#").await;

    // Passed the ban check before the ban existed, so it is accepted,
    // then closed at once
    let mut late = Client::connect_with(addr, with_will("late")).await;
    let Some(ControlPacket::Disconnect(disconnect)) = late.receive().await else {
        panic!("Expected DISCONNECT");
    };
    assert_eq!(disconnect.reason, ReasonCode::AdministrativeAction);
    assert!(watcher.receive().await.is_none(), "the will is dropped");
    let mut admin = admin_client(addr).await;
    let details = wait_for(&mut admin, "late", |d| d["connected"] == false).await;
    assert_eq!(details["willPending"], false);
}

#[tokio::test]
async fn re_authenticating_as_a_banned_user_closes_the_client() {
    let (addr, admin) = start(config(vec![Arc::new(AllowAll), Arc::new(SwitchUser)])).await;
    let mut watcher = Client::connect(addr, "watcher").await;
    watcher.subscribe("wills/#").await;
    let mut packet = with_will("device");
    packet.properties = Some(ConnectProperties {
        authentication_method: Some(AuthenticationMethod::new("SWITCH".into())),
        ..Default::default()
    });
    let mut device = Client::connect_with(addr, packet).await;

    // The ban matches nobody connected yet
    let ban = NewBan::new(BanKey::new(BanKind::Username, "mallory"));
    admin.add_ban(ban).await.unwrap();
    device
        .send(ControlPacket::Auth(AuthPacket {
            reason: ReasonCode::ReAuthenticate,
            properties: AuthProperties {
                auth_method: Some(AuthenticationMethod::new("SWITCH".into())),
                ..Default::default()
            },
        }))
        .await;

    let Some(ControlPacket::Disconnect(disconnect)) = device.receive().await else {
        panic!("Expected DISCONNECT");
    };
    assert_eq!(disconnect.reason, ReasonCode::AdministrativeAction);
    assert!(watcher.receive().await.is_none(), "the will is dropped");
}

#[tokio::test]
async fn expired_bans_are_dropped_from_the_list_and_storage() {
    let store = Arc::new(MemoryStore::new());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    // config() publishes no $SYS statistics: bans are swept regardless
    let server = common::broker(listener, config(Vec::new()))
        .storage(Arc::clone(&store))
        .build()
        .await
        .unwrap();
    let admin = server.admin();
    tokio::spawn(server.run(std::future::pending::<()>()));

    let mut ban = NewBan::new(BanKey::new(BanKind::ClientId, "brief"));
    ban.duration = Some(1);
    admin.add_ban(ban).await.unwrap();
    assert_eq!(
        connack_reason(addr, b"", connect_packet("brief")).await,
        ReasonCode::Banned
    );
    assert_eq!(store.load_bans().await.unwrap().len(), 1);

    // It holds one to two seconds, and is swept within a second after
    let mut swept = false;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if store.load_bans().await.unwrap().is_empty() {
            swept = true;
            break;
        }
    }
    assert!(swept, "the expired ban is still stored");
    assert_eq!(admin.bans(Paging::default()).await.total_count, 0);
    assert_eq!(
        connack_reason(addr, b"", connect_packet("brief")).await,
        ReasonCode::Success
    );
}

/// A Unix socket listener in the temp directory, its path unique to `name`
/// and this test run.
#[cfg(unix)]
fn unix_listener(name: &str) -> (UnixListener, std::path::PathBuf) {
    let path = std::env::temp_dir().join(format!("mercurio-{name}-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    (UnixListener::bind(&path).unwrap(), path)
}

#[cfg(unix)]
#[tokio::test]
async fn trusted_unix_listeners_skip_authentication_and_access_control() {
    let (socket, path) = unix_listener("trusted");
    let mut trusted = ListenerConfig::new(socket, Transport::Mqtt);
    trusted.trusted = true;
    // Every client must log in, and the ACL grants nothing they could use
    let acl = AclFile::parse("topic read nothing/#\n").unwrap();
    let mut config = config(vec![Arc::new(acl)]);
    config.auth.require_auth = true;
    let listeners = vec![trusted];
    let (addr, _) = start_with(config, |builder| builder.listeners(listeners)).await;

    assert_eq!(
        connack_reason(addr, b"", connect_packet("outsider")).await,
        ReasonCode::BadUserNameOrPassword
    );
    let stream = UnixStream::connect(&path).await.unwrap();
    let mut admin = Client::handshake(stream, connect_packet("admin")).await;
    admin.subscribe("replies/admin").await;
    let info = admin.run(json!({"command": "getBrokerInfo"})).await;
    assert_eq!(info["data"]["version"], env!("CARGO_PKG_VERSION"));
    let _ = std::fs::remove_file(&path);
}

#[cfg(unix)]
#[tokio::test]
async fn the_admin_socket_admits_the_brokers_own_user() {
    let (socket, path) = unix_listener("own-uid");
    // The socket file belongs to the user this test, and so the broker,
    // runs as
    let own_uid = std::fs::metadata(&path).unwrap().uid();
    let mut admin_socket = ListenerConfig::new(socket, Transport::Mqtt);
    admin_socket.trusted = true;
    admin_socket.peer_filter = Some(Arc::new(move |peer: &UCred| peer.uid() == own_uid));
    let config = config(Vec::new());
    let listeners = vec![admin_socket];
    start_with(config, |builder| builder.listeners(listeners)).await;

    let stream = UnixStream::connect(&path).await.unwrap();
    let mut admin = Client::handshake(stream, connect_packet("admin")).await;
    admin.subscribe("replies/admin").await;
    let info = admin.run(json!({"command": "getBrokerInfo"})).await;
    assert!(info.get("error").is_none(), "{info}");
    let _ = std::fs::remove_file(&path);
}

#[cfg(unix)]
#[tokio::test]
async fn clients_the_peer_filter_refuses_are_closed_unanswered() {
    let (socket, path) = unix_listener("refused-peer");
    let mut admin_socket = ListenerConfig::new(socket, Transport::Mqtt);
    admin_socket.trusted = true;
    admin_socket.peer_filter = Some(Arc::new(|_: &UCred| false));
    let config = config(Vec::new());
    let listeners = vec![admin_socket];
    start_with(config, |builder| builder.listeners(listeners)).await;

    let mut stream = UnixStream::connect(&path).await.unwrap();
    let mut bytes = BytesMut::new();
    ControlPacket::Connect(connect_packet("intruder"))
        .encode(&mut bytes, ProtocolVersion::V5)
        .unwrap();
    // The broker may have closed it already
    let _ = stream.write_all(&bytes).await;
    let mut buffer = [0u8; 16];
    let read = timeout(Duration::from_secs(2), stream.read(&mut buffer))
        .await
        .expect("the connection was left open");
    assert!(matches!(read, Ok(0) | Err(_)), "{read:?}");
    let _ = std::fs::remove_file(&path);
}

#[cfg(unix)]
#[tokio::test]
async fn trusted_unix_listeners_ignore_limits() {
    let (socket, path) = unix_listener("trusted-full");
    let mut trusted = ListenerConfig::new(socket, Transport::Mqtt);
    trusted.trusted = true;
    let mut config = config(Vec::new());
    config.limits.max_connections = Some(1);
    let listeners = vec![trusted];
    let (addr, _) = start_with(config, |builder| builder.listeners(listeners)).await;

    // Once its CONNACK is back the client holds the only slot
    let _full = Client::connect(addr, "full").await;
    let mut refused = TcpStream::connect(addr).await.unwrap();
    let mut buffer = [0u8; 16];
    let read = timeout(Duration::from_secs(2), refused.read(&mut buffer))
        .await
        .expect("the connection was left open");
    assert!(matches!(read, Ok(0) | Err(_)), "{read:?}");

    let stream = UnixStream::connect(&path).await.unwrap();
    let mut admin = Client::handshake(stream, connect_packet("admin")).await;
    admin.subscribe("replies/admin").await;
    let info = admin.run(json!({"command": "getBrokerInfo"})).await;
    assert!(info.get("error").is_none(), "{info}");
    let _ = std::fs::remove_file(&path);
}

#[cfg(unix)]
#[tokio::test]
async fn trusted_unix_listeners_ignore_redirects_and_client_id_prefixes() {
    let (socket, path) = unix_listener("trusted-redirect");
    let mut trusted = ListenerConfig::new(socket, Transport::Mqtt);
    trusted.trusted = true;
    let mut config = config(Vec::new());
    config.policy.redirect = Some(mercurio_server::server::Redirect::new("other:1883", true));
    config.policy.client_id_prefixes = vec!["app-".into()];
    let listeners = vec![trusted];
    let (_, _) = start_with(config, |builder| builder.listeners(listeners)).await;

    let stream = UnixStream::connect(&path).await.unwrap();
    let mut admin = Client::handshake(stream, connect_packet("admin")).await;
    admin.subscribe("replies/admin").await;
    let info = admin.run(json!({"command": "getBrokerInfo"})).await;
    assert!(info.get("error").is_none(), "{info}");
    let _ = std::fs::remove_file(&path);
}

#[cfg(unix)]
#[tokio::test]
async fn bans_still_apply_on_trusted_unix_listeners() {
    let (socket, path) = unix_listener("trusted-ban");
    let mut trusted = ListenerConfig::new(socket, Transport::Mqtt);
    trusted.trusted = true;
    let config = config(Vec::new());
    let listeners = vec![trusted];
    let (_, admin) = start_with(config, |builder| builder.listeners(listeners)).await;
    admin
        .add_ban(NewBan::new(BanKey::new(BanKind::ClientId, "bad")))
        .await
        .unwrap();

    let stream = UnixStream::connect(&path).await.unwrap();
    assert_eq!(
        connack_reason_on(stream, b"", connect_packet("bad")).await,
        ReasonCode::Banned
    );
    let _ = std::fs::remove_file(&path);
}

/// A broker with two more listeners, under the mount points `a/` and `b/`;
/// returns the addresses of those.
async fn start_mounted() -> (SocketAddr, SocketAddr, SocketAddr, Admin) {
    let mut listeners = Vec::new();
    let mut addrs = Vec::new();
    for mount_point in ["a/", "b/"] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        addrs.push(listener.local_addr().unwrap());
        let mut listener = ListenerConfig::new(listener, Transport::Mqtt);
        listener.mount_point = Some(mount_point.into());
        listeners.push(listener);
    }
    let config = config(vec![Arc::new(AllowAll)]);
    let (addr, admin) = start_with(config, |builder| builder.listeners(listeners)).await;
    (addr, addrs[0], addrs[1], admin)
}

/// Commands naming a client id with sessions on more than one mount point
/// need the mount point, and act on that session only.
#[tokio::test]
async fn commands_name_the_mount_point_of_a_shared_client_id() {
    let (addr, a, b, admin) = start_mounted().await;
    let mut tool = admin_client(addr).await;
    let mut on_a = Client::connect_with(a, persistent("dev")).await;
    let mut on_b = Client::connect_with(b, persistent("dev")).await;

    let listed = tool
        .run(json!({"command": "listClients", "clientidPrefix": "dev"}))
        .await;
    let mount_points: Vec<&Value> = listed["data"]["clients"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| &c["mountpoint"])
        .collect();
    assert_eq!(mount_points, [&json!("a/"), &json!("b/")], "{listed}");
    let admin_entry = tool
        .run(json!({"command": "getClient", "clientid": "admin"}))
        .await;
    assert_eq!(admin_entry["data"]["mountpoint"], Value::Null);

    for command in ["getClient", "disconnectClient", "deleteSession"] {
        let ambiguous = tool
            .run(json!({"command": command, "clientid": "dev"}))
            .await;
        assert_eq!(ambiguous["code"], "invalidArgument", "{ambiguous}");
        let error = ambiguous["error"].as_str().unwrap();
        assert!(
            error.contains("\"a/\", \"b/\"") && error.contains("mountpoint"),
            "{error}"
        );
    }
    let on_a_details = tool
        .run(json!({"command": "getClient", "clientid": "dev", "mountpoint": "a/"}))
        .await;
    assert_eq!(on_a_details["data"]["mountpoint"], "a/", "{on_a_details}");
    let none = tool
        .run(json!({"command": "getClient", "clientid": "dev", "mountpoint": ""}))
        .await;
    assert_eq!(none["code"], "notFound", "{none}");

    // Disconnecting one leaves the other connected
    let disconnected = tool
        .run(json!({"command": "disconnectClient", "clientid": "dev", "mountpoint": "a/"}))
        .await;
    assert!(disconnected.get("error").is_none(), "{disconnected}");
    assert!(matches!(
        on_a.receive().await,
        Some(ControlPacket::Disconnect(_))
    ));
    let key = |m: &str| SessionKey::new(Some(m.into()), "dev");
    for _ in 0..100 {
        if !admin.client(&key("a/")).await.unwrap().connected {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!admin.client(&key("a/")).await.unwrap().connected);
    assert!(admin.client(&key("b/")).await.unwrap().connected);

    // Deleting one leaves the other's session
    let deleted = tool
        .run(json!({
            "command": "deleteSession", "clientid": "dev", "mountpoint": "b/", "disconnect": true
        }))
        .await;
    assert!(deleted.get("error").is_none(), "{deleted}");
    assert!(matches!(
        on_b.receive().await,
        Some(ControlPacket::Disconnect(_))
    ));
    assert_eq!(
        admin.client(&key("b/")).await.unwrap_err().code,
        ErrorCode::NotFound
    );
    assert!(admin.client(&key("a/")).await.is_ok());
    // With one mount point left, the client id alone names it
    let alone = tool
        .run(json!({"command": "getClient", "clientid": "dev"}))
        .await;
    assert_eq!(alone["data"]["mountpoint"], "a/", "{alone}");
    assert_eq!(admin.session_key("dev", None).await.unwrap(), key("a/"));
}

/// `listClients` lists the clients of one mount point, `""` for none.
#[tokio::test]
async fn clients_are_listed_by_mount_point() {
    let (addr, a, b, _) = start_mounted().await;
    let mut tool = admin_client(addr).await;
    let _on_a = Client::connect_with(a, persistent("dev")).await;
    let _also_on_a = Client::connect_with(a, persistent("other")).await;
    let _on_b = Client::connect_with(b, persistent("dev")).await;

    let listed = |response: Value| -> Vec<(Value, Value)> {
        response["data"]["clients"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| (c["clientid"].clone(), c["mountpoint"].clone()))
            .collect()
    };
    let on_a = tool
        .run(json!({"command": "listClients", "mountpoint": "a/"}))
        .await;
    assert_eq!(
        listed(on_a),
        [(json!("dev"), json!("a/")), (json!("other"), json!("a/"))]
    );
    let unmounted = tool
        .run(json!({"command": "listClients", "mountpoint": ""}))
        .await;
    assert_eq!(listed(unmounted), [(json!("admin"), Value::Null)]);
    let none = tool
        .run(json!({"command": "listClients", "mountpoint": "c/"}))
        .await;
    assert_eq!(none["data"]["totalCount"], 0, "{none}");
}

/// A ban on a client id holds on every mount point.
#[tokio::test]
async fn client_id_bans_hold_on_every_mount_point() {
    let (_, a, b, admin) = start_mounted().await;
    let mut on_a = Client::connect_with(a, persistent("dev")).await;
    let mut on_b = Client::connect_with(b, persistent("dev")).await;
    admin
        .add_ban(NewBan::new(BanKey::new(BanKind::ClientId, "dev")))
        .await
        .unwrap();
    for client in [&mut on_a, &mut on_b] {
        assert!(matches!(
            client.receive().await,
            Some(ControlPacket::Disconnect(_))
        ));
    }
    assert_eq!(
        connack_reason(a, b"", persistent("dev")).await,
        ReasonCode::Banned
    );
}
