use std::{net::SocketAddr, sync::Arc, time::Duration};

use mercurio_server::{
    admin::{Admin, BanKey, BanKind, Paging},
    plugin::{BoxFuture, ClientInfo, Credentials, Decision, Plugin},
    server::{Limits, ListenerConfig, LoginFailures, Server, ServerConfig, Transport},
};
use mercurio_storage::memory::MemoryStore;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

/// Lets in any user with the password `secret`.
struct Passwords;

impl Plugin for Passwords {
    fn name(&self) -> &str {
        "passwords"
    }

    fn authenticate<'a>(
        &'a self,
        _: &'a ClientInfo,
        credentials: &'a Credentials<'a>,
    ) -> BoxFuture<'a, Decision> {
        let allowed = credentials.password == b"secret";
        Box::pin(async move {
            match allowed {
                true => Decision::Allow,
                false => Decision::Deny,
            }
        })
    }
}

/// A broker banning an address after two failed logins, with a listener
/// behind a PROXY header so clients can claim addresses other than
/// loopback. Returns the main and the proxied listener's addresses.
async fn start_broker() -> (SocketAddr, SocketAddr, Admin) {
    let main = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxied = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addrs = (main.local_addr().unwrap(), proxied.local_addr().unwrap());
    let mut config = ServerConfig::default();
    config.plugins = vec![Arc::new(Passwords)];
    config.limits = {
        let mut value = Limits::default();
        value.login_failures = Some(LoginFailures::new(
            2,
            Duration::from_secs(60),
            Duration::from_secs(300),
        ));
        value
    };
    config.listeners = vec![{
        let mut value = ListenerConfig::new(proxied, Transport::Mqtt);
        value.proxy_protocol = true;
        value.proxy_from = vec!["127.0.0.1".parse().unwrap()];
        value
    }];
    let server = Server::new(main, config, Arc::new(MemoryStore::new()))
        .await
        .unwrap();
    let admin = server.admin();
    tokio::spawn(server.run(std::future::pending::<()>()));
    (addrs.0, addrs.1, admin)
}

/// Logs in as `alice` with `password`, from `source` behind the proxy when
/// given; the CONNACK reason code, or None when closed unanswered.
async fn login(addr: SocketAddr, source: Option<&str>, password: &[u8]) -> Option<u8> {
    let mut body = b"\x00\x04MQTT\x05\xc2\x00\x3c\x00\x00\x01c\x00\x05alice".to_vec();
    body.extend_from_slice(&(password.len() as u16).to_be_bytes());
    body.extend_from_slice(password);
    let mut bytes = Vec::new();
    if let Some(source) = source {
        bytes.extend_from_slice(format!("PROXY TCP4 {source} 127.0.0.1 5000 1883\r\n").as_bytes());
    }
    bytes.extend_from_slice(&[0x10, body.len() as u8]);
    bytes.extend(body);
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let _ = stream.write_all(&bytes).await;
    let mut received = Vec::new();
    timeout(Duration::from_secs(2), async {
        while received.len() < 4 {
            let mut chunk = [0u8; 64];
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => received.extend_from_slice(&chunk[..n]),
            }
        }
    })
    .await
    .expect("neither answered nor closed");
    (received.len() >= 4).then(|| received[3])
}

#[tokio::test]
async fn failed_logins_ban_the_address_for_a_while() {
    let (main, proxied, admin) = start_broker().await;
    let attacker = Some("192.0.2.7");

    assert_eq!(login(proxied, attacker, b"guess").await, Some(0x86));
    assert_eq!(login(proxied, attacker, b"guess").await, Some(0x86));
    // Banned now, even with the right password
    assert_eq!(login(proxied, attacker, b"secret").await, None);
    // Others are not, loopback never is
    assert_eq!(
        login(proxied, Some("192.0.2.8"), b"secret").await,
        Some(0x00)
    );
    for _ in 0..3 {
        assert_eq!(login(main, None, b"guess").await, Some(0x86));
    }
    assert_eq!(login(main, None, b"secret").await, Some(0x00));

    // An automatic ban, listed with its expiry, that the admin API lifts
    let bans = admin.bans(Paging::default()).await.items;
    assert_eq!(bans.len(), 1);
    assert_eq!(bans[0].value, "192.0.2.7");
    assert!(bans[0].reason.as_deref().unwrap().starts_with("automatic"));
    assert!(bans[0].expires_at.is_some());
    let key = BanKey::new(BanKind::Address, bans[0].value.clone());
    admin.remove_ban(&key).await.unwrap();
    assert_eq!(login(proxied, attacker, b"secret").await, Some(0x00));
}
