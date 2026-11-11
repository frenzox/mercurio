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

#[cfg(feature = "tls")]
mod channel_binding {
    use std::time::Duration;

    use mercurio_client::Event;
    use mercurio_server::{
        server::{ListenerConfig, Transport},
        tls::{Tls, TlsConfig},
    };
    use tokio::{io::AsyncReadExt, sync::Notify, time::timeout};

    use super::*;

    const CA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/ca.pem");
    const CERT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/server.pem");
    const KEY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/server-key.pem");

    /// A broker requiring SCRAM-SHA-256-PLUS on its TLS listener, behind a
    /// relay whose connections all drop when the returned `Notify` fires.
    async fn start_tls_broker() -> (u16, Arc<Notify>) {
        let plain = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let secure = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let broker = secure.local_addr().unwrap();
        let mut config = ServerConfig::default();
        config.plugins = vec![Arc::new(
            ScramAuth::new(users())
                .decoy_iterations(4096)
                .require_channel_binding(true),
        )];
        config.auth.require_auth = true;
        let tls = Tls::new(TlsConfig::new(CERT, KEY)).unwrap();
        tokio::spawn(
            common::broker(plain, config)
                .listener(ListenerConfig::new(secure, Transport::Mqtt).with_tls(tls))
                .run(std::future::pending::<()>()),
        );

        let relay = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = relay.local_addr().unwrap().port();
        let cut = Arc::new(Notify::new());
        let cutting = Arc::clone(&cut);
        tokio::spawn(async move {
            loop {
                let (mut client, _) = relay.accept().await.unwrap();
                let cut = Arc::clone(&cutting);
                tokio::spawn(async move {
                    let mut server = tokio::net::TcpStream::connect(broker).await.unwrap();
                    tokio::select! {
                        _ = tokio::io::copy_bidirectional(&mut client, &mut server) => {}
                        _ = cut.notified() => {}
                    }
                });
            }
        });
        (port, cut)
    }

    fn bound() -> ScramSha256 {
        ScramSha256::new("alice", "secret").channel_binding(true)
    }

    #[tokio::test]
    async fn a_bound_client_connects_reconnects_and_reauthenticates_over_tls() {
        let (port, cut) = start_tls_broker().await;
        let client = MqttClient::connect(
            ConnectOptions::new("localhost", port)
                .ca_path(CA)
                .client_id("c")
                .reconnect(Duration::from_millis(50))
                .authenticator(bound()),
        )
        .await
        .unwrap();
        client.reauthenticate().await.unwrap();

        cut.notify_waiters();
        let event = timeout(Duration::from_secs(10), async {
            loop {
                match client.recv().await {
                    Some(Event::Reconnected { .. }) | None => break,
                    _ => {}
                }
            }
        })
        .await;
        assert!(event.is_ok(), "no reconnection");
        client.reauthenticate().await.unwrap();
    }

    #[tokio::test]
    async fn a_bound_client_sends_nothing_over_plain_tcp() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let broker = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = [0u8; 64];
            timeout(Duration::from_secs(5), stream.read(&mut buffer))
                .await
                .unwrap()
                .unwrap()
        });
        let result = MqttClient::connect(
            ConnectOptions::new("127.0.0.1", addr.port())
                .client_id("c")
                .authenticator(bound()),
        )
        .await;
        let Err(ClientError::AuthenticationFailed(reason)) = result else {
            panic!("{:?}", result.err());
        };
        assert_eq!(reason, "channel binding needs a TLS 1.3 connection");
        assert_eq!(broker.await.unwrap(), 0, "the client sent CONNECT");
    }
}
