use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    time::Duration,
};

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
    auth::{AuthExchange, AuthManager, AuthMethod, AuthResult},
    server::{AuthConfig, ServerConfig},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

/// Challenges each client with a fresh nonce and expects it back signed,
/// which needs state kept between the steps of an exchange.
#[derive(Default)]
struct Challenge {
    issued: AtomicU32,
}

enum ChallengeExchange {
    Start(String),
    Issued(String),
    Done,
}

impl AuthMethod for Challenge {
    fn name(&self) -> &str {
        "CHALLENGE"
    }

    fn begin(&self) -> Box<dyn AuthExchange> {
        let nonce = format!("nonce-{}", self.issued.fetch_add(1, Ordering::Relaxed));
        Box::new(ChallengeExchange::Start(nonce))
    }
}

impl AuthExchange for ChallengeExchange {
    fn step(&mut self, data: Option<&Bytes>) -> AuthResult {
        match std::mem::replace(self, ChallengeExchange::Done) {
            ChallengeExchange::Start(nonce) => {
                let challenge = Bytes::from(nonce.clone());
                *self = ChallengeExchange::Issued(nonce);
                AuthResult::Continue(challenge)
            }
            ChallengeExchange::Issued(nonce)
                if data.is_some_and(|d| d.as_ref() == format!("{nonce}-signed").as_bytes()) =>
            {
                AuthResult::Success {
                    username: Some("challenger".into()),
                }
            }
            _ => AuthResult::Failed(ReasonCode::NotAuthorized),
        }
    }
}

async fn start_broker() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut manager = AuthManager::new();
    manager.register(Box::new(Challenge::default()));
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

fn auth(reason: ReasonCode, data: Bytes) -> ControlPacket {
    ControlPacket::Auth(AuthPacket {
        reason,
        properties: AuthProperties {
            auth_method: Some(AuthenticationMethod::new("CHALLENGE".into())),
            auth_data: Some(AuthenticationData::new(data)),
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

/// Answers the challenge, signing it correctly or not. Returns the nonce.
async fn answer_challenge(stream: &mut TcpStream, buffer: &mut BytesMut, sign: bool) -> Bytes {
    let ControlPacket::Auth(challenge) = receive(stream, buffer).await else {
        panic!("Expected AUTH challenge");
    };
    assert_eq!(challenge.reason, ReasonCode::ContinueAuthentication);
    let nonce = challenge.properties.auth_data.unwrap().value;
    let answer = if sign {
        Bytes::from([nonce.as_ref(), b"-signed"].concat())
    } else {
        Bytes::from_static(b"guess")
    };
    send(stream, auth(ReasonCode::ContinueAuthentication, answer)).await;
    nonce
}

#[tokio::test]
async fn challenge_response_during_connect() {
    let addr = start_broker().await;
    let (mut stream, mut buffer) = connect_with(addr, "CHALLENGE").await;

    let first_nonce = answer_challenge(&mut stream, &mut buffer, true).await;

    let ControlPacket::ConnAck(connack) = receive(&mut stream, &mut buffer).await else {
        panic!("Expected CONNACK");
    };
    assert_eq!(connack.reason_code, ReasonCode::Success);
    let method = connack.properties.unwrap().authentication_method.unwrap();
    assert_eq!(method.value, "CHALLENGE");

    // Re-authentication runs a new exchange, with its own nonce
    send(&mut stream, auth(ReasonCode::ReAuthenticate, Bytes::new())).await;
    let second_nonce = answer_challenge(&mut stream, &mut buffer, true).await;
    assert_ne!(first_nonce, second_nonce);
    let ControlPacket::Auth(done) = receive(&mut stream, &mut buffer).await else {
        panic!("Expected AUTH");
    };
    assert_eq!(done.reason, ReasonCode::Success);
}

#[tokio::test]
async fn wrong_answer_is_not_authorized() {
    let addr = start_broker().await;
    let (mut stream, mut buffer) = connect_with(addr, "CHALLENGE").await;

    answer_challenge(&mut stream, &mut buffer, false).await;

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
