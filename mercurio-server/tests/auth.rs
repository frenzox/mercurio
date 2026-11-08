mod common;

use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};

use mercurio_server::{
    auth::SimpleCredentialValidator,
    plugin::{ready, BoxFuture, ClientInfo, Credentials, Decision, Plugin},
    server::{ListenerConfig, ServerConfig, Transport},
};
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
};

async fn start_broker(allow_anonymous: bool) -> SocketAddr {
    let credentials = HashMap::from([("user".to_string(), "secret".to_string())]);
    start_broker_with(
        vec![Arc::new(SimpleCredentialValidator::new(credentials))],
        |config| {
            config.auth.allow_anonymous = allow_anonymous;
        },
    )
    .await
}

/// Starts a broker that requires authentication from `plugins`, with
/// whatever else `settings` changes.
async fn start_broker_with(
    plugins: Vec<Arc<dyn Plugin>>,
    settings: impl FnOnce(&mut ServerConfig),
) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut config = ServerConfig::default();
    config.auth.require_auth = true;
    config.auth.allow_anonymous = false;
    config.plugins = plugins;
    settings(&mut config);
    tokio::spawn(mercurio_server::server::run_with_config(
        listener,
        config,
        std::future::pending::<()>(),
    ));
    addr
}

/// Authenticates as `behaviour` says: defers, sleeps for a minute, panics,
/// or answers with a fixed decision. Users other than `"user"` are allowed.
struct Authenticator {
    behaviour: Behaviour,
}

#[derive(Clone, Copy)]
enum Behaviour {
    Defer,
    Sleep,
    Panic,
    Decide(Decision),
}

fn authenticator(behaviour: Behaviour) -> Arc<dyn Plugin> {
    Arc::new(Authenticator { behaviour })
}

impl Plugin for Authenticator {
    fn name(&self) -> &str {
        "authenticator"
    }

    fn authenticate<'a>(
        &'a self,
        _: &'a ClientInfo,
        credentials: &'a Credentials<'a>,
    ) -> BoxFuture<'a, Decision> {
        if credentials.username != "user" {
            return ready(Decision::Allow);
        }
        match self.behaviour {
            Behaviour::Defer => ready(Decision::Defer),
            Behaviour::Sleep => Box::pin(async {
                tokio::time::sleep(Duration::from_secs(60)).await;
                Decision::Allow
            }),
            Behaviour::Panic => panic!("broken plugin"),
            Behaviour::Decide(decision) => ready(decision),
        }
    }
}

/// MQTT 3.1.1 CONNECT; returns the CONNACK return code.
async fn connect(addr: SocketAddr, credentials: Option<(&str, &str)>) -> u8 {
    let mut flags = 0x02;
    let mut payload = vec![0x00, 0x01, b'c'];
    if let Some((user, password)) = credentials {
        flags |= 0xc0;
        for field in [user, password] {
            payload.extend_from_slice(&(field.len() as u16).to_be_bytes());
            payload.extend_from_slice(field.as_bytes());
        }
    }
    let mut body = vec![0x00, 0x04, b'M', b'Q', b'T', b'T', 0x04, flags, 0x00, 0x3c];
    body.extend_from_slice(&payload);
    let mut packet = vec![0x10, body.len() as u8];
    packet.extend_from_slice(&body);

    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(&packet).await.unwrap();
    common::read_connack(&mut stream).await[3]
}

#[tokio::test]
async fn anonymous_clients_follow_allow_anonymous() {
    let strict = start_broker(false).await;
    // Bad user name or password
    assert_eq!(connect(strict, None).await, 0x04);
    assert_eq!(connect(strict, Some(("user", "secret"))).await, 0x00);

    let lenient = start_broker(true).await;
    assert_eq!(connect(lenient, None).await, 0x00);
    assert_eq!(connect(lenient, Some(("user", "secret"))).await, 0x00);
    // Credentials that are sent are still checked
    assert_eq!(connect(lenient, Some(("user", "wrong"))).await, 0x04);
}

// Bad user name or password
const REFUSED: u8 = 0x04;

#[tokio::test]
async fn clients_with_credentials_no_plugin_decides_on_are_refused() {
    let addr = start_broker_with(vec![authenticator(Behaviour::Defer)], |_| {}).await;
    assert_eq!(connect(addr, Some(("user", "secret"))).await, REFUSED);
}

#[tokio::test]
async fn authentication_running_out_of_time_refuses() {
    let addr = start_broker_with(vec![authenticator(Behaviour::Sleep)], |config| {
        config.plugin_timeout = Duration::from_millis(100);
    })
    .await;
    assert_eq!(connect(addr, Some(("user", "secret"))).await, REFUSED);
}

#[tokio::test]
async fn authentication_panicking_refuses_and_the_broker_keeps_serving() {
    let addr = start_broker_with(vec![authenticator(Behaviour::Panic)], |_| {}).await;
    assert_eq!(connect(addr, Some(("user", "secret"))).await, REFUSED);
    assert_eq!(connect(addr, Some(("other", "secret"))).await, 0x00);
}

#[tokio::test]
async fn listener_plugins_replace_the_brokers() {
    let extra = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let extra_addr = extra.local_addr().unwrap();
    let main = start_broker_with(
        vec![authenticator(Behaviour::Decide(Decision::Deny))],
        |config| {
            let mut listener = ListenerConfig::new(extra, Transport::Mqtt);
            listener.plugins = Some(vec![authenticator(Behaviour::Decide(Decision::Allow))]);
            config.listeners = vec![listener];
        },
    )
    .await;

    assert_eq!(connect(main, Some(("user", "secret"))).await, REFUSED);
    assert_eq!(connect(extra_addr, Some(("user", "secret"))).await, 0x00);
}
