use std::{net::SocketAddr, sync::Arc, time::Duration};

use bytes::{Bytes, BytesMut};
use mercurio_core::{
    codec::Encoder,
    properties::{CorrelationData, ResponseTopic, UserProperty},
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
    plugin::{ready, BoxFuture, ClientInfo, MessageAction, Plugin, PluginMessage},
    server::ServerConfig,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

async fn start_broker(plugins: Vec<Arc<dyn Plugin>>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut config = ServerConfig::default();
    config.plugins = plugins;
    config.sys_interval = None;
    tokio::spawn(mercurio_server::server::run_with_config(
        listener,
        config,
        std::future::pending::<()>(),
    ));
    addr
}

struct Client {
    stream: TcpStream,
    buffer: BytesMut,
}

impl Client {
    async fn connect(addr: SocketAddr, client_id: &str) -> Client {
        let mut client = Client {
            stream: TcpStream::connect(addr).await.unwrap(),
            buffer: BytesMut::new(),
        };
        client
            .send(ControlPacket::Connect(ConnectPacket {
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
            }))
            .await;
        let Some(ControlPacket::ConnAck(connack)) = client.receive().await else {
            panic!("Expected CONNACK");
        };
        assert_eq!(connack.reason_code, ReasonCode::Success);
        client
    }

    async fn send(&mut self, packet: ControlPacket) {
        let mut encoded = BytesMut::new();
        packet.encode(&mut encoded);
        self.stream.write_all(&encoded).await.unwrap();
    }

    async fn receive(&mut self) -> Option<ControlPacket> {
        loop {
            if let Ok(packet) =
                ControlPacket::parse_with_version(&mut self.buffer, ProtocolVersion::V5)
            {
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
    async fn publish(&mut self, topic: &str, payload: &'static [u8]) -> ReasonCode {
        self.send(ControlPacket::Publish(PublishPacket {
            topic_name: topic.into(),
            packet_id: Some(7),
            qos_level: QoS::AtLeastOnce,
            payload: Some(Bytes::from_static(payload)),
            ..Default::default()
        }))
        .await;
        match self.receive().await {
            Some(ControlPacket::PubAck(ack)) => ack.reason,
            other => panic!("Expected PUBACK, got {other:?}"),
        }
    }

    /// Publishes a request at QoS 0, so no PUBACK races the answer.
    async fn request(
        &mut self,
        topic: &str,
        payload: &'static [u8],
        properties: PublishProperties,
    ) {
        self.send(ControlPacket::Publish(PublishPacket {
            topic_name: topic.into(),
            payload: Some(Bytes::from_static(payload)),
            properties: Some(properties),
            ..Default::default()
        }))
        .await;
    }
}

/// Tags every message, and drops empty ones.
struct Rewrite;

impl Plugin for Rewrite {
    fn name(&self) -> &str {
        "rewrite"
    }

    fn on_message<'a>(
        &'a self,
        client: &'a ClientInfo,
        message: &'a mut PluginMessage,
    ) -> BoxFuture<'a, MessageAction> {
        if message.payload.as_ref() == b"drop" {
            return ready(MessageAction::Drop);
        }
        message.topic = format!("rewritten/{}", message.topic);
        message.properties.user_property = Some(vec![UserProperty::new(
            "publisher".into(),
            client.client_id.clone(),
        )]);
        ready(MessageAction::Continue)
    }
}

#[tokio::test]
async fn message_hook_rewrites_and_drops() {
    let addr = start_broker(vec![Arc::new(Rewrite)]).await;
    let mut subscriber = Client::connect(addr, "sub").await;
    subscriber.subscribe("rewritten/#").await;
    let mut publisher = Client::connect(addr, "pub").await;

    assert_eq!(publisher.publish("a", b"hello").await, ReasonCode::Success);
    let Some(ControlPacket::Publish(delivered)) = subscriber.receive().await else {
        panic!("Expected the rewritten message");
    };
    assert_eq!(delivered.topic_name, "rewritten/a");
    let tag = delivered.properties.unwrap().user_property.unwrap();
    assert_eq!(
        (tag[0].key.as_str(), tag[0].value.as_str()),
        ("publisher", "pub")
    );

    // Dropped, yet acknowledged as accepted
    assert_eq!(publisher.publish("a", b"drop").await, ReasonCode::Success);
    assert!(subscriber.receive().await.is_none());
}

/// Records the events it hears.
#[derive(Default)]
struct Recorder(std::sync::Mutex<Vec<String>>);

impl Plugin for Recorder {
    fn name(&self) -> &str {
        "recorder"
    }

    fn on_event<'a>(&'a self, event: &'a mercurio_server::plugin::Event) -> BoxFuture<'a, ()> {
        use mercurio_server::plugin::Event::*;
        let line = match event {
            Connected { client, .. } => format!("connected {}", client.client_id),
            Disconnected { client, .. } => format!("disconnected {}", client.client_id),
            Subscribed { client, filter, .. } => {
                format!("subscribed {} {filter}", client.client_id)
            }
            Unsubscribed { client, filter, .. } => {
                format!("unsubscribed {} {filter}", client.client_id)
            }
            SessionExpired { client_id, .. } => format!("expired {client_id}"),
            _ => "other".into(),
        };
        self.0.lock().unwrap().push(line);
        ready(())
    }
}

#[tokio::test]
async fn lifecycle_events_reach_plugins() {
    let recorder = Arc::new(Recorder::default());
    let addr = start_broker(vec![recorder.clone()]).await;
    let mut client = Client::connect(addr, "c1").await;
    client.subscribe("a/#").await;
    client
        .send(ControlPacket::Unsubscribe(
            mercurio_packets::unsubscribe::UnsubscribePacket {
                packet_id: 2,
                properties: None,
                payload: vec![mercurio_packets::unsubscribe::UnsubscribePayload {
                    topic_filter: "a/#".into(),
                }],
            },
        ))
        .await;
    assert!(matches!(
        client.receive().await,
        Some(ControlPacket::UnsubAck(_))
    ));
    drop(client);
    for _ in 0..200 {
        if recorder.0.lock().unwrap().len() >= 4 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert_eq!(
        *recorder.0.lock().unwrap(),
        [
            "connected c1",
            "subscribed c1 a/#",
            "unsubscribed c1 a/#",
            "disconnected c1"
        ]
    );
}

/// Answers `$CONTROL/echo/...` with the payload, upper-cased, and counts ticks.
#[derive(Default)]
struct Echo(std::sync::atomic::AtomicU32);

impl Plugin for Echo {
    fn name(&self) -> &str {
        "echo"
    }

    fn authorize<'a>(
        &'a self,
        _: &'a ClientInfo,
        _: &'a str,
        _: mercurio_server::plugin::Access,
    ) -> BoxFuture<'a, mercurio_server::plugin::Decision> {
        ready(mercurio_server::plugin::Decision::Allow)
    }

    fn authorize_control<'a>(
        &'a self,
        _: &'a ClientInfo,
        _: &'a str,
    ) -> BoxFuture<'a, mercurio_server::plugin::Decision> {
        ready(mercurio_server::plugin::Decision::Allow)
    }

    fn on_control<'a>(
        &'a self,
        _: &'a ClientInfo,
        path: &'a str,
        payload: Bytes,
    ) -> BoxFuture<'a, Option<Bytes>> {
        assert_eq!(path, "v1");
        ready(Some(Bytes::from(payload.to_ascii_uppercase())))
    }

    fn on_tick(&self) -> BoxFuture<'_, ()> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        ready(())
    }
}

#[tokio::test]
async fn control_requests_are_answered_to_the_requester_only() {
    let addr = start_broker(vec![Arc::new(Echo::default())]).await;
    let mut listener = Client::connect(addr, "listener").await;
    listener.subscribe("$CONTROL/echo/#").await;
    let mut admin = Client::connect(addr, "admin").await;
    admin.subscribe("$CONTROL/echo/#").await;

    admin
        .request("$CONTROL/echo/v1", b"ping", PublishProperties::default())
        .await;
    let Some(ControlPacket::Publish(answer)) = admin.receive().await else {
        panic!("Expected the response");
    };
    assert_eq!(answer.topic_name, "$CONTROL/echo/v1/response");
    assert_eq!(answer.payload.as_deref(), Some(&b"PING"[..]));
    // Neither the request nor the answer reaches anyone else
    assert!(listener.receive().await.is_none());
}

#[tokio::test]
async fn control_responses_follow_the_response_topic() {
    let addr = start_broker(vec![Arc::new(Echo::default())]).await;
    let mut admin = Client::connect(addr, "admin").await;
    admin.subscribe("replies/admin").await;

    admin
        .request(
            "$CONTROL/echo/v1",
            b"ping",
            PublishProperties {
                response_topic: Some(ResponseTopic::new("replies/admin".into())),
                correlation_data: Some(CorrelationData::new(Bytes::from_static(b"42"))),
                ..Default::default()
            },
        )
        .await;
    let Some(ControlPacket::Publish(answer)) = admin.receive().await else {
        panic!("Expected the response");
    };
    assert_eq!(answer.topic_name, "replies/admin");
    assert_eq!(answer.payload.as_deref(), Some(&b"PING"[..]));
    let correlation = answer.properties.and_then(|p| p.correlation_data);
    assert_eq!(
        correlation.map(|c| c.value),
        Some(Bytes::from_static(b"42"))
    );
}

#[tokio::test]
async fn control_responses_reach_requesters_in_share_groups() {
    let addr = start_broker(vec![Arc::new(Echo::default())]).await;
    let mut admin = Client::connect(addr, "admin").await;
    admin.subscribe("$share/g/replies/#").await;
    let mut other = Client::connect(addr, "other").await;
    other.subscribe("$share/g/replies/#").await;

    for _ in 0..3 {
        admin
            .request(
                "$CONTROL/echo/v1",
                b"ping",
                PublishProperties {
                    response_topic: Some(ResponseTopic::new("replies/admin".into())),
                    ..Default::default()
                },
            )
            .await;
    }
    for _ in 0..3 {
        let Some(ControlPacket::Publish(answer)) = admin.receive().await else {
            panic!("Expected the response");
        };
        assert_eq!(answer.topic_name, "replies/admin");
    }
    assert!(other.receive().await.is_none());
}

#[tokio::test]
async fn control_responses_need_a_matching_subscription() {
    let addr = start_broker(vec![Arc::new(Echo::default())]).await;
    let mut admin = Client::connect(addr, "admin").await;

    admin
        .request("$CONTROL/echo/v1", b"ping", PublishProperties::default())
        .await;
    assert!(admin.receive().await.is_none());
}

#[tokio::test]
async fn plugins_tick() {
    let echo = Arc::new(Echo::default());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut config = ServerConfig::default();
    config.plugins = vec![echo.clone()];
    config.plugin_tick_interval = Duration::from_millis(20);
    tokio::spawn(mercurio_server::server::run_with_config(
        listener,
        config,
        std::future::pending::<()>(),
    ));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(echo.0.load(std::sync::atomic::Ordering::Relaxed) >= 3);
}

/// Moves messages to the topic in their payload, and counts the control
/// requests it gets.
#[derive(Default)]
struct Redirect(std::sync::atomic::AtomicU32);

impl Plugin for Redirect {
    fn name(&self) -> &str {
        "x"
    }

    fn on_message<'a>(
        &'a self,
        _: &'a ClientInfo,
        message: &'a mut PluginMessage,
    ) -> BoxFuture<'a, MessageAction> {
        message.topic = String::from_utf8_lossy(&message.payload).into_owned();
        ready(MessageAction::Continue)
    }

    fn on_control<'a>(
        &'a self,
        _: &'a ClientInfo,
        _: &'a str,
        _: Bytes,
    ) -> BoxFuture<'a, Option<Bytes>> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        ready(Some(Bytes::from_static(b"done")))
    }
}

#[tokio::test]
async fn message_hook_cant_move_messages_to_reserved_topics() {
    let redirect = Arc::new(Redirect::default());
    let addr = start_broker(vec![redirect.clone()]).await;
    let mut subscriber = Client::connect(addr, "sub").await;
    subscriber.subscribe("$CONTROL/x/#").await;
    subscriber.subscribe("$SYS/x").await;
    subscriber.subscribe("plain/#").await;
    let mut publisher = Client::connect(addr, "pub").await;

    // Dropped, yet acknowledged as accepted
    assert_eq!(
        publisher.publish("a", b"$CONTROL/x/v1").await,
        ReasonCode::Success
    );
    assert_eq!(publisher.publish("a", b"$SYS/x").await, ReasonCode::Success);
    assert_eq!(
        publisher.publish("a", b"plain/x").await,
        ReasonCode::Success
    );
    let Some(ControlPacket::Publish(delivered)) = subscriber.receive().await else {
        panic!("Expected the message moved to a plain topic");
    };
    assert_eq!(delivered.topic_name, "plain/x");
    assert!(subscriber.receive().await.is_none());
    assert_eq!(redirect.0.load(std::sync::atomic::Ordering::Relaxed), 0);
}
