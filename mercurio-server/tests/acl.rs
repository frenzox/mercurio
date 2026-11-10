use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};

use bytes::{Bytes, BytesMut};
use mercurio_core::{codec::Encoder, protocol::ProtocolVersion, qos::QoS, reason::ReasonCode};
use mercurio_packets::{
    connect::{ConnectFlags, ConnectPacket, ConnectPayload, ConnectProperties},
    publish::PublishPacket,
    subscribe::{SubscribePacket, SubscribePayload, SubscriptionOptions},
    ControlPacket,
};
use mercurio_server::{acl::AclFile, auth::SimpleCredentialValidator, server::ServerConfig};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

const ACL: &str = "
topic read public/#
topic deny public/secret

user alice
topic readwrite alice/#
topic write public/#
";

/// With `verify_users`, user names are checked against a password.
async fn start_broker(verify_users: bool) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut config = ServerConfig::default();
    config.auth.require_auth = verify_users;
    config.auth.allow_anonymous = true;
    config.plugins = vec![
        Arc::new(SimpleCredentialValidator::new(HashMap::from([(
            "alice".to_string(),
            "secret".to_string(),
        )]))),
        Arc::new(AclFile::parse(ACL).unwrap()),
    ];
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
    async fn connect(
        addr: SocketAddr,
        client_id: &str,
        credentials: Option<(&str, &str)>,
        will: Option<&str>,
    ) -> (Self, ReasonCode) {
        let connect = ConnectPacket {
            protocol_version: ProtocolVersion::V5,
            flags: ConnectFlags {
                clean_start: true,
                user_name: credentials.is_some(),
                password: credentials.is_some(),
                will_flag: will.is_some(),
                ..Default::default()
            },
            keepalive: 60,
            properties: Some(ConnectProperties::default()),
            payload: ConnectPayload {
                client_id: client_id.into(),
                user_name: credentials.map(|(user, _)| user.into()),
                password: credentials.map(|(_, password)| Bytes::from(password.to_string())),
                will_properties: will.map(|_| Default::default()),
                will_topic: will.map(Into::into),
                will_payload: will.map(|_| Bytes::from_static(b"bye")),
            },
        };
        let mut client = Client {
            stream: TcpStream::connect(addr).await.unwrap(),
            buffer: BytesMut::new(),
        };
        client.send(ControlPacket::Connect(connect)).await;
        let ControlPacket::ConnAck(connack) = client.receive().await.unwrap() else {
            panic!("Expected CONNACK");
        };
        (client, connack.reason_code)
    }

    async fn send(&mut self, packet: ControlPacket) {
        let mut encoded = BytesMut::new();
        packet.encode(&mut encoded);
        self.stream.write_all(&encoded).await.unwrap();
    }

    async fn receive(&mut self) -> Option<ControlPacket> {
        loop {
            if ControlPacket::check(&self.buffer).is_ok() {
                let packet =
                    ControlPacket::parse_with_version(&mut self.buffer, ProtocolVersion::V5);
                return Some(packet.unwrap());
            }
            match timeout(
                Duration::from_millis(300),
                self.stream.read_buf(&mut self.buffer),
            )
            .await
            {
                Ok(read) => assert_ne!(read.unwrap(), 0, "connection closed"),
                Err(_) => return None,
            }
        }
    }

    async fn subscribe(&mut self, filter: &str) -> ReasonCode {
        self.send(ControlPacket::Subscribe(SubscribePacket {
            packet_id: 1,
            properties: None,
            payload: vec![SubscribePayload {
                topic_filter: filter.into(),
                subs_opt: SubscriptionOptions::default(),
            }],
        }))
        .await;
        let Some(ControlPacket::SubAck(suback)) = self.receive().await else {
            panic!("Expected SUBACK");
        };
        suback.payload[0].reason_code
    }

    /// Publishes at QoS 1 and returns the PUBACK reason.
    async fn publish(&mut self, topic: &str) -> ReasonCode {
        self.send(ControlPacket::Publish(PublishPacket {
            qos_level: QoS::AtLeastOnce,
            topic_name: topic.into(),
            packet_id: Some(1),
            payload: Some(Bytes::from(topic.to_string())),
            ..Default::default()
        }))
        .await;
        let Some(ControlPacket::PubAck(puback)) = self.receive().await else {
            panic!("Expected PUBACK");
        };
        puback.reason
    }
}

#[tokio::test]
async fn topics_follow_the_acl() {
    let addr = start_broker(true).await;
    let (mut reader, _) = Client::connect(addr, "reader", None, None).await;
    let (mut alice, _) = Client::connect(addr, "alice", Some(("alice", "secret")), None).await;

    assert_eq!(reader.subscribe("alice/#").await, ReasonCode::NotAuthorized);
    // Granted QoS 0
    assert_eq!(reader.subscribe("public/#").await.get_code(), 0x00);

    // Accepted; nobody happens to be subscribed
    assert_eq!(
        alice.publish("alice/notes").await,
        ReasonCode::NoMatchingSubscribers
    );
    assert_eq!(
        reader.publish("public/news").await,
        ReasonCode::NotAuthorized
    );

    // The subscription covers public/secret, but reading it is denied
    alice.publish("public/secret").await;
    alice.publish("public/news").await;
    let Some(ControlPacket::Publish(delivered)) = reader.receive().await else {
        panic!("Expected Publish");
    };
    assert_eq!(delivered.topic_name, "public/news");
    assert!(reader.receive().await.is_none());
}

#[tokio::test]
async fn shared_subscriptions_are_checked_by_their_topic_filter() {
    let addr = start_broker(true).await;
    let (mut reader, _) = Client::connect(addr, "reader", None, None).await;

    // Granted QoS 0, as for public/# itself
    assert_eq!(reader.subscribe("$share/g/public/#").await.get_code(), 0x00);
    assert_eq!(
        reader.subscribe("$share/g/alice/#").await,
        ReasonCode::NotAuthorized
    );
}

#[tokio::test]
async fn will_topic_needs_write_access() {
    let addr = start_broker(true).await;

    let (_, refused) = Client::connect(addr, "c", None, Some("public/will")).await;
    assert_eq!(refused, ReasonCode::NotAuthorized);

    let (_, accepted) =
        Client::connect(addr, "a", Some(("alice", "secret")), Some("alice/will")).await;
    assert_eq!(accepted, ReasonCode::Success);
}

#[tokio::test]
async fn credentials_are_verified_without_required_authentication() {
    let addr = start_broker(false).await;
    let (_, refused) = Client::connect(addr, "i", Some(("alice", "anything")), None).await;
    assert_eq!(refused, ReasonCode::BadUserNameOrPassword);

    let (mut alice, accepted) = Client::connect(addr, "a", Some(("alice", "secret")), None).await;
    assert_eq!(accepted, ReasonCode::Success);
    assert_eq!(alice.subscribe("alice/#").await.get_code(), 0x00);
}
