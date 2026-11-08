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
    Authenticator, ClientError, ConnectOptions, DisconnectReason, Event, MqttClient,
};
use mercurio_core::reason::ReasonCode;
use mercurio_server::{
    auth::{AuthExchange, AuthManager, AuthMethod, AuthResult},
    server::ServerConfig,
};
use tokio::{net::TcpListener, time::timeout};

/// Asks for "hello", challenges with a nonce, and accepts the nonce signed.
struct ChallengeMethod;

struct ChallengeExchange {
    challenged: bool,
}

impl AuthMethod for ChallengeMethod {
    fn name(&self) -> &str {
        "CHALLENGE"
    }

    fn begin(&self) -> Box<dyn AuthExchange> {
        Box::new(ChallengeExchange { challenged: false })
    }
}

impl AuthExchange for ChallengeExchange {
    fn step(&mut self, data: Option<&Bytes>) -> AuthResult {
        match (std::mem::replace(&mut self.challenged, true), data) {
            (false, Some(d)) if d == "hello" => AuthResult::Continue(Bytes::from_static(b"nonce")),
            (true, Some(d)) if d == "nonce-signed" => AuthResult::Success {
                username: Some("u".into()),
            },
            _ => AuthResult::Failed(ReasonCode::NotAuthorized),
        }
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

    fn start(&mut self) -> Option<Bytes> {
        Some(Bytes::from_static(b"hello"))
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
    let mut manager = AuthManager::new();
    manager.register(Box::new(ChallengeMethod));
    let mut config = ServerConfig::default();
    config.auth.auth_manager = Some(Arc::new(manager));
    tokio::spawn(mercurio_server::server::run_with_config(
        listener,
        config,
        std::future::pending::<()>(),
    ));
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

    fn start(&mut self) -> Option<Bytes> {
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
