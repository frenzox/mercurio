use std::{net::SocketAddr, sync::Arc, time::Duration};

use bytes::{Bytes, BytesMut};
use mercurio_core::{codec::Encoder, protocol::ProtocolVersion, qos::QoS, reason::ReasonCode};
use mercurio_packets::{
    connect::{ConnectFlags, ConnectPacket, ConnectPayload, ConnectProperties},
    publish::PublishPacket,
    subscribe::{SubscribePacket, SubscribePayload, SubscriptionOptions},
    ControlPacket,
};
use mercurio_server::{
    acl::AclFile,
    server::{AuthConfig, ServerConfig},
};
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

async fn start_broker() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = ServerConfig {
        auth: AuthConfig {
            authorizer: Some(Arc::new(AclFile::parse(ACL).unwrap())),
            ..Default::default()
        },
        ..Default::default()
    };
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
        user: Option<&str>,
        will: Option<&str>,
    ) -> (Self, ReasonCode) {
        let connect = ConnectPacket {
            protocol_version: ProtocolVersion::V5,
            flags: ConnectFlags {
                clean_start: true,
                user_name: user.is_some(),
                will_flag: will.is_some(),
                ..Default::default()
            },
            keepalive: 60,
            properties: Some(ConnectProperties::default()),
            payload: ConnectPayload {
                client_id: client_id.into(),
                user_name: user.map(Into::into),
                will_properties: will.map(|_| Default::default()),
                will_topic: will.map(Into::into),
                will_payload: will.map(|_| Bytes::from_static(b"bye")),
                ..Default::default()
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
            if ControlPacket::check(&mut self.buffer).is_ok() {
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
    let addr = start_broker().await;
    let (mut reader, _) = Client::connect(addr, "reader", None, None).await;
    let (mut alice, _) = Client::connect(addr, "alice", Some("alice"), None).await;

    assert_eq!(reader.subscribe("alice/#").await, ReasonCode::NotAuthorized);
    // Granted QoS 0
    assert_eq!(reader.subscribe("public/#").await.get_code(), 0x00);

    assert_eq!(alice.publish("alice/notes").await, ReasonCode::Success);
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
async fn will_topic_needs_write_access() {
    let addr = start_broker().await;

    let (_, refused) = Client::connect(addr, "c", None, Some("public/will")).await;
    assert_eq!(refused, ReasonCode::NotAuthorized);

    let (_, accepted) = Client::connect(addr, "a", Some("alice"), Some("alice/will")).await;
    assert_eq!(accepted, ReasonCode::Success);
}
