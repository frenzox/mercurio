//! SCRAM-SHA-256 between the client's authenticator and the broker's
//! built-in mechanism.

mod common;

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use bytes::Bytes;
use mercurio_client::{ClientError, ConnectOptions, MqttClient, ReasonCode, ScramSha256};
use mercurio_server::{
    auth::{ScramAuth, StoredCredentials},
    plugin::{AuthExchange, AuthResult, BoxFuture, ClientInfo, Plugin},
    server::ServerConfig,
};
use tokio::net::TcpListener;

fn users() -> Arc<HashMap<String, StoredCredentials>> {
    let mut users = HashMap::new();
    users.insert(
        "alice".to_string(),
        StoredCredentials::generate("secret", 4096).unwrap(),
    );
    Arc::new(users)
}

/// The broker's mechanism, with the server signature it sends replaced
/// while `tamper` is set.
struct Tampering {
    scram: ScramAuth<HashMap<String, StoredCredentials>>,
    tamper: Arc<AtomicBool>,
}

struct TamperingExchange {
    inner: Box<dyn AuthExchange>,
    tamper: Arc<AtomicBool>,
}

impl Plugin for Tampering {
    fn name(&self) -> &str {
        "tampering"
    }

    fn begin_auth(&self, client: &ClientInfo, method: &str) -> Option<Box<dyn AuthExchange>> {
        let inner = self.scram.begin_auth(client, method)?;
        Some(Box::new(TamperingExchange {
            inner,
            tamper: Arc::clone(&self.tamper),
        }))
    }
}

impl AuthExchange for TamperingExchange {
    fn step<'a>(&'a mut self, data: Option<&'a Bytes>) -> BoxFuture<'a, AuthResult> {
        Box::pin(async move {
            match self.inner.step(data).await {
                AuthResult::Success { username, data } if self.tamper.load(Ordering::SeqCst) => {
                    let forged = data.map(|_| {
                        Bytes::from_static(b"v=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
                    });
                    AuthResult::Success {
                        username,
                        data: forged,
                    }
                }
                result => result,
            }
        })
    }
}

async fn start_broker(tamper: Arc<AtomicBool>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut config = ServerConfig::default();
    config.plugins = vec![Arc::new(Tampering {
        scram: ScramAuth::new(users()).decoy_iterations(4096),
        tamper,
    })];
    config.auth.require_auth = true;
    tokio::spawn(common::broker(listener, config).run(std::future::pending::<()>()));
    addr
}

fn options(addr: SocketAddr, username: &str, password: &str) -> ConnectOptions {
    ConnectOptions::new("127.0.0.1", addr.port())
        .client_id("c")
        .authenticator(ScramSha256::new(username, password))
}

#[tokio::test]
async fn the_right_password_connects_and_reauthenticates() {
    let addr = start_broker(Arc::default()).await;
    let client = MqttClient::connect(options(addr, "alice", "secret"))
        .await
        .unwrap();
    client.reauthenticate().await.unwrap();
    client.reauthenticate().await.unwrap();
}

#[tokio::test]
async fn a_wrong_password_or_unknown_user_is_refused() {
    let addr = start_broker(Arc::default()).await;
    for (username, password) in [("alice", "wrong"), ("mallory", "secret")] {
        let result = MqttClient::connect(options(addr, username, password)).await;
        assert!(
            matches!(
                result,
                Err(ClientError::ConnectionRefused(
                    ReasonCode::BadUserNameOrPassword
                ))
            ),
            "{username}: {:?}",
            result.err()
        );
    }
}

#[tokio::test]
async fn a_tampered_server_signature_fails_the_connection() {
    let tamper = Arc::new(AtomicBool::new(true));
    let addr = start_broker(Arc::clone(&tamper)).await;
    let result = MqttClient::connect(options(addr, "alice", "secret")).await;
    assert!(
        matches!(&result, Err(ClientError::AuthenticationFailed(_))),
        "{:?}",
        result.err()
    );

    // Re-authentication checks it too
    tamper.store(false, Ordering::SeqCst);
    let client = MqttClient::connect(options(addr, "alice", "secret"))
        .await
        .unwrap();
    tamper.store(true, Ordering::SeqCst);
    assert!(matches!(
        client.reauthenticate().await,
        Err(ClientError::AuthenticationFailed(_))
    ));
}
