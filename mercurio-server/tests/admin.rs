use std::{net::SocketAddr, sync::Arc, time::Duration};

use bytes::{Bytes, BytesMut};
use mercurio_core::{
    properties::{CorrelationData, ResponseTopic, SessionExpiryInterval},
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
    packet.flags.will_flag = true;
    packet.payload.will_properties = Some(Default::default());
    packet.payload.will_topic = Some(format!("wills/{client_id}"));
    packet.payload.will_payload = Some(Bytes::from_static(b"gone"));
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
        let refused = admin.delete_session("device", false).await;
        assert_eq!(refused.unwrap_err().code, ErrorCode::Conflict);
    }
    assert!(device.receive().await.is_none());
    assert!(admin.client("device").await.unwrap().connected);
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

    admin.delete_session("device", false).await.unwrap();
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

    admin.delete_session("device", true).await.unwrap();
    assert!(matches!(
        device.receive().await,
        Some(ControlPacket::Disconnect(_))
    ));
    tokio::time::sleep(Duration::from_millis(1500)).await;

    assert!(recorder.0.lock().unwrap().is_empty());
    assert_eq!(admin.info().await.stats["clients/expired"], 0);
    assert!(admin.client("device").await.is_err());
}

#[tokio::test]
async fn disconnect_client_closes_mqtt311_connections() {
    let (addr, admin) = start(config(vec![Arc::new(AllowAll)])).await;
    let mut packet = connect_packet("old");
    packet.protocol_version = ProtocolVersion::V3_1_1;
    packet.properties = None;
    packet.flags.clean_start = false;
    let mut old = Client::connect_with(addr, packet).await;

    admin.disconnect("old", Default::default()).await.unwrap();

    let mut rest = BytesMut::new();
    let read = timeout(Duration::from_secs(2), old.stream.read_buf(&mut rest))
        .await
        .expect("the connection should close");
    assert_eq!(read.unwrap(), 0, "no packet before the close");
    assert!(!admin.client("old").await.unwrap().connected);
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
