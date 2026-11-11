mod common;

use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use bytes::Bytes;
use mercurio_client::{
    Authenticator, ClientError, ConnectOptions, DisconnectReason, Event, MqttClient, ReasonCode,
};
use mercurio_server::{
    plugin::{ready, AuthExchange, AuthResult, BoxFuture, ClientInfo, Plugin},
    server::ServerConfig,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

/// Asks for "hello", challenges with a nonce, and accepts the nonce signed.
struct ChallengeMethod;

struct ChallengeExchange {
    challenged: bool,
}

impl Plugin for ChallengeMethod {
    fn name(&self) -> &str {
        "challenge"
    }

    fn begin_auth(&self, _: &ClientInfo, method: &str) -> Option<Box<dyn AuthExchange>> {
        (method == "CHALLENGE")
            .then(|| Box::new(ChallengeExchange { challenged: false }) as Box<dyn AuthExchange>)
    }
}

impl AuthExchange for ChallengeExchange {
    fn step<'a>(&'a mut self, data: Option<&'a Bytes>) -> BoxFuture<'a, AuthResult> {
        ready(
            match (std::mem::replace(&mut self.challenged, true), data) {
                (false, Some(d)) if d == "hello" => {
                    AuthResult::Continue(Bytes::from_static(b"nonce"))
                }
                (true, Some(d)) if d == "nonce-signed" => AuthResult::Success {
                    username: Some("u".into()),
                    data: None,
                },
                _ => AuthResult::failed(ReasonCode::NotAuthorized),
            },
        )
    }
}

/// Signs challenges, or forges the signature once `forge` is set.
struct Signer {
    forge: Arc<AtomicBool>,
}

impl Authenticator for Signer {
    fn method(&self) -> String {
        "CHALLENGE".into()
    }

    fn start(&mut self) -> Result<Option<Bytes>, String> {
        Ok(Some(Bytes::from_static(b"hello")))
    }

    fn respond(&mut self, challenge: Option<Bytes>) -> Result<Option<Bytes>, String> {
        let challenge = challenge.ok_or("no challenge")?;
        let suffix = if self.forge.load(Ordering::SeqCst) {
            "-forged"
        } else {
            "-signed"
        };
        Ok(Some(Bytes::from(
            [&challenge[..], suffix.as_bytes()].concat(),
        )))
    }
}

async fn start_broker() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut config = ServerConfig::default();
    config.plugins = vec![Arc::new(ChallengeMethod)];
    tokio::spawn(common::broker(listener, config).run(std::future::pending::<()>()));
    addr
}

fn options(addr: SocketAddr, forge: &Arc<AtomicBool>) -> ConnectOptions {
    ConnectOptions::new("127.0.0.1", addr.port())
        .client_id("c")
        .authenticator(Signer {
            forge: Arc::clone(forge),
        })
}

#[tokio::test]
async fn challenge_response_authentication_connects() {
    let addr = start_broker().await;
    let forge = Arc::new(AtomicBool::new(false));

    let client = MqttClient::connect(options(addr, &forge)).await.unwrap();
    client.reauthenticate().await.unwrap();
}

#[tokio::test]
async fn wrong_answer_is_refused() {
    let addr = start_broker().await;
    let forge = Arc::new(AtomicBool::new(true));

    assert!(matches!(
        MqttClient::connect(options(addr, &forge)).await,
        Err(ClientError::ConnectionRefused(ReasonCode::NotAuthorized))
    ));
}

#[tokio::test]
async fn failed_reauthentication_ends_the_connection() {
    let addr = start_broker().await;
    let forge = Arc::new(AtomicBool::new(false));
    let client = MqttClient::connect(options(addr, &forge)).await.unwrap();

    forge.store(true, Ordering::SeqCst);
    assert!(client.reauthenticate().await.is_err());
    let event = timeout(Duration::from_secs(2), client.recv())
        .await
        .unwrap();
    assert!(matches!(
        event,
        Some(Event::Disconnected {
            reason: DisconnectReason::ServerInitiated {
                reason: ReasonCode::NotAuthorized,
                ..
            }
        })
    ));
}

/// Signs correctly, but expects the broker to prove itself on success,
/// which this broker doesn't.
struct Suspicious(Signer);

impl Authenticator for Suspicious {
    fn method(&self) -> String {
        self.0.method()
    }

    fn start(&mut self) -> Result<Option<Bytes>, String> {
        self.0.start()
    }

    fn respond(&mut self, challenge: Option<Bytes>) -> Result<Option<Bytes>, String> {
        self.0.respond(challenge)
    }

    fn finish(&mut self, data: Option<Bytes>) -> Result<(), String> {
        data.map(|_| ())
            .ok_or_else(|| "broker sent no proof".into())
    }
}

#[tokio::test]
async fn client_can_reject_the_broker() {
    let addr = start_broker().await;
    let signer = Signer {
        forge: Arc::new(AtomicBool::new(false)),
    };
    let options = ConnectOptions::new("127.0.0.1", addr.port()).authenticator(Suspicious(signer));

    assert!(matches!(
        MqttClient::connect(options).await,
        Err(ClientError::AuthenticationFailed(_))
    ));
}

/// Reads one packet whose remaining length fits in a byte.
async fn read_packet(socket: &mut TcpStream) -> Vec<u8> {
    let mut header = [0u8; 2];
    socket.read_exact(&mut header).await.unwrap();
    let mut body = vec![0u8; header[1] as usize];
    socket.read_exact(&mut body).await.unwrap();
    [header.to_vec(), body].concat()
}

async fn accept(listener: &TcpListener) -> TcpStream {
    let (mut socket, _) = listener.accept().await.unwrap();
    assert_eq!(read_packet(&mut socket).await[0], 0x10);
    socket
        .write_all(&[0x20, 0x03, 0x00, 0x00, 0x00])
        .await
        .unwrap();
    socket
}

#[tokio::test]
async fn reauthentication_cut_short_by_a_reconnect_fails_and_can_be_retried() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let broker = tokio::spawn(async move {
        // The connection drops while the re-authentication is under way
        let mut socket = accept(&listener).await;
        assert_eq!(read_packet(&mut socket).await[0], 0xf0);
        drop(socket);

        let mut socket = accept(&listener).await;
        assert_eq!(read_packet(&mut socket).await[0], 0xf0);
        socket.write_all(&[0xf0, 0x02, 0x00, 0x00]).await.unwrap();
        socket
    });

    let forge = Arc::new(AtomicBool::new(false));
    let client = MqttClient::connect(
        ConnectOptions::new("127.0.0.1", port)
            .client_id("c")
            .keep_alive(0)
            .reconnect(Duration::from_millis(50))
            .authenticator(Signer { forge }),
    )
    .await
    .unwrap();

    let first = timeout(Duration::from_secs(3), client.reauthenticate())
        .await
        .expect("the interrupted re-authentication never completed");
    assert!(matches!(first, Err(ClientError::Disconnected)), "{first:?}");
    loop {
        match timeout(Duration::from_secs(3), client.recv()).await {
            Ok(Some(Event::Reconnected { .. })) => break,
            Ok(Some(_)) => {}
            other => panic!("no reconnection: {other:?}"),
        }
    }
    timeout(Duration::from_secs(3), client.reauthenticate())
        .await
        .expect("the second re-authentication never completed")
        .unwrap();
    let _socket = broker.await.unwrap();
}
