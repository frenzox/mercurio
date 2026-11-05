use std::{net::SocketAddr, sync::Arc, time::Duration};

use bytes::{Bytes, BytesMut};
use mercurio_core::{
    codec::Encoder,
    properties::{AuthenticationData, AuthenticationMethod},
    protocol::ProtocolVersion,
    reason::ReasonCode,
};
use mercurio_packets::{
    auth::{AuthPacket, AuthProperties},
    connect::{ConnectFlags, ConnectPacket, ConnectPayload, ConnectProperties},
    ControlPacket,
};
use mercurio_server::{
    auth::{AuthManager, AuthMethod, AuthResult},
    server::{AuthConfig, ServerConfig},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

/// Challenges with "nonce" and expects "nonce-signed" back.
struct Challenge;

impl AuthMethod for Challenge {
    fn name(&self) -> &str {
        "CHALLENGE"
    }

    fn auth_start(&self, _initial_data: Option<&Bytes>) -> AuthResult {
        AuthResult::Continue(Bytes::from_static(b"nonce"))
    }

    fn auth_continue(&self, response_data: &Bytes) -> AuthResult {
        if response_data.as_ref() == b"nonce-signed" {
            AuthResult::Success
        } else {
            AuthResult::Failed(ReasonCode::NotAuthorized)
        }
    }
}

async fn start_broker() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut manager = AuthManager::new();
    manager.register(Box::new(Challenge));
    let config = ServerConfig {
        auth: AuthConfig {
            auth_manager: Some(Arc::new(manager)),
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

async fn send(stream: &mut TcpStream, packet: ControlPacket) {
    let mut buffer = BytesMut::new();
    packet.encode(&mut buffer);
    stream.write_all(&buffer).await.unwrap();
}

async fn receive(stream: &mut TcpStream, buffer: &mut BytesMut) -> ControlPacket {
    loop {
        if ControlPacket::check(buffer).is_ok() {
            return ControlPacket::parse_with_version(buffer, ProtocolVersion::V5).unwrap();
        }
        let read = timeout(Duration::from_secs(2), stream.read_buf(buffer))
            .await
            .expect("timed out waiting for broker")
            .unwrap();
        assert_ne!(read, 0, "connection closed");
    }
}

fn auth(reason: ReasonCode, method: &str, data: &'static [u8]) -> ControlPacket {
    ControlPacket::Auth(AuthPacket {
        reason,
        properties: AuthProperties {
            auth_method: Some(AuthenticationMethod::new(method.into())),
            auth_data: Some(AuthenticationData::new(Bytes::from_static(data))),
            ..Default::default()
        },
    })
}

async fn connect_with(addr: SocketAddr, method: &str) -> (TcpStream, BytesMut) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let connect = ConnectPacket {
        protocol_version: ProtocolVersion::V5,
        flags: ConnectFlags {
            clean_start: true,
            ..Default::default()
        },
        keepalive: 60,
        properties: Some(ConnectProperties {
            authentication_method: Some(AuthenticationMethod::new(method.into())),
            ..Default::default()
        }),
        payload: ConnectPayload {
            client_id: "c".into(),
            ..Default::default()
        },
    };
    send(&mut stream, ControlPacket::Connect(connect)).await;
    (stream, BytesMut::new())
}

async fn answer_challenge(stream: &mut TcpStream, buffer: &mut BytesMut, answer: &'static [u8]) {
    let ControlPacket::Auth(challenge) = receive(stream, buffer).await else {
        panic!("Expected AUTH challenge");
    };
    assert_eq!(challenge.reason, ReasonCode::ContinueAuthentication);
    assert_eq!(
        challenge.properties.auth_data.unwrap().value.as_ref(),
        b"nonce"
    );
    send(
        stream,
        auth(ReasonCode::ContinueAuthentication, "CHALLENGE", answer),
    )
    .await;
}

#[tokio::test]
async fn challenge_response_during_connect() {
    let addr = start_broker().await;
    let (mut stream, mut buffer) = connect_with(addr, "CHALLENGE").await;

    answer_challenge(&mut stream, &mut buffer, b"nonce-signed").await;

    let ControlPacket::ConnAck(connack) = receive(&mut stream, &mut buffer).await else {
        panic!("Expected CONNACK");
    };
    assert_eq!(connack.reason_code, ReasonCode::Success);
    let method = connack.properties.unwrap().authentication_method.unwrap();
    assert_eq!(method.value, "CHALLENGE");

    // Re-authentication goes through the same method
    send(
        &mut stream,
        auth(ReasonCode::ReAuthenticate, "CHALLENGE", b""),
    )
    .await;
    answer_challenge(&mut stream, &mut buffer, b"nonce-signed").await;
    let ControlPacket::Auth(done) = receive(&mut stream, &mut buffer).await else {
        panic!("Expected AUTH");
    };
    assert_eq!(done.reason, ReasonCode::Success);
}

#[tokio::test]
async fn wrong_answer_is_not_authorized() {
    let addr = start_broker().await;
    let (mut stream, mut buffer) = connect_with(addr, "CHALLENGE").await;

    answer_challenge(&mut stream, &mut buffer, b"guess").await;

    let ControlPacket::ConnAck(connack) = receive(&mut stream, &mut buffer).await else {
        panic!("Expected CONNACK");
    };
    assert_eq!(connack.reason_code, ReasonCode::NotAuthorized);
}

#[tokio::test]
async fn unknown_method_is_refused() {
    let addr = start_broker().await;
    let (mut stream, mut buffer) = connect_with(addr, "SCRAM-SHA-1").await;

    let ControlPacket::ConnAck(connack) = receive(&mut stream, &mut buffer).await else {
        panic!("Expected CONNACK");
    };
    assert_eq!(connack.reason_code, ReasonCode::BadAuthenticationMethod);
}
