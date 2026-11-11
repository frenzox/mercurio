//! TLS listeners, against the test PKI of `tests/fixtures/pki`.

use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use mercurio_server::{
    auth::SimpleCredentialValidator,
    error::TlsError,
    plugin::{ready, BoxFuture, ClientInfo, Event, Plugin},
    server::{run_with_config, ServerConfig},
    tls::{CertificateName, Tls, TlsConfig},
};
use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer, ServerName};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};
use tokio_rustls::{client::TlsStream, TlsConnector};

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/pki/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn certs(name: &str) -> Vec<CertificateDer<'static>> {
    CertificateDer::pem_file_iter(fixture(name))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// A directory of its own for a test to write files in.
fn scratch(test: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mercurio-tls-{test}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Serves `config` on a new port and returns its address.
async fn start(config: ServerConfig) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(run_with_config(
        listener,
        config,
        std::future::pending::<()>(),
    ));
    addr
}

/// Connects trusting the test root CA, presenting the certificate and key
/// of `identity` when given.
async fn handshake(
    addr: SocketAddr,
    identity: Option<&str>,
) -> std::io::Result<TlsStream<TcpStream>> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    roots.add_parsable_certificates(certs("root.pem"));
    let builder = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots);
    let config = match identity {
        Some(name) => {
            let key = PrivateKeyDer::from_pem_file(fixture(&format!("{name}.key"))).unwrap();
            builder
                .with_client_auth_cert(certs(&format!("{name}.pem")), key)
                .unwrap()
        }
        None => builder.with_no_client_auth(),
    };
    let socket = TcpStream::connect(addr).await?;
    let name = ServerName::try_from("localhost").unwrap();
    TlsConnector::from(Arc::new(config))
        .connect(name, socket)
        .await
}

fn connect_packet(client_id: &str, login: Option<(&str, &str)>) -> Vec<u8> {
    let field = |out: &mut Vec<u8>, value: &[u8]| {
        out.extend_from_slice(&(value.len() as u16).to_be_bytes());
        out.extend_from_slice(value);
    };
    let mut body = vec![0, 4, b'M', b'Q', b'T', b'T', 5];
    body.push(if login.is_some() { 0xc2 } else { 0x02 });
    body.extend_from_slice(&[0, 60, 0]);
    field(&mut body, client_id.as_bytes());
    if let Some((username, password)) = login {
        field(&mut body, username.as_bytes());
        field(&mut body, password.as_bytes());
    }
    let mut packet = vec![0x10, body.len() as u8];
    packet.extend(body);
    packet
}

/// Sends CONNECT and returns the CONNACK's reason code, or None if the
/// broker closed the connection instead.
async fn mqtt_connect(
    stream: &mut TlsStream<TcpStream>,
    client_id: &str,
    login: Option<(&str, &str)>,
) -> Option<u8> {
    let exchange = async {
        stream
            .write_all(&connect_packet(client_id, login))
            .await
            .ok()?;
        let mut header = [0u8; 2];
        stream.read_exact(&mut header).await.ok()?;
        assert_eq!(header[0], 0x20, "expected CONNACK");
        let mut rest = vec![0u8; header[1] as usize];
        stream.read_exact(&mut rest).await.ok()?;
        Some(rest[1])
    };
    timeout(Duration::from_secs(5), exchange)
        .await
        .expect("neither CONNACK nor close")
}

async fn ping(stream: &mut TlsStream<TcpStream>) {
    stream.write_all(&[0xc0, 0]).await.unwrap();
    let mut pingresp = [0u8; 2];
    timeout(Duration::from_secs(2), stream.read_exact(&mut pingresp))
        .await
        .expect("no PINGRESP")
        .unwrap();
    assert_eq!(pingresp, [0xd0, 0]);
}

fn served_certificate(stream: &TlsStream<TcpStream>) -> CertificateDer<'static> {
    stream.get_ref().1.peer_certificates().unwrap()[0]
        .clone()
        .into_owned()
}

#[tokio::test]
async fn reload_serves_new_connections_the_new_certificate() {
    let dir = scratch("reload");
    let (cert, key) = (dir.join("cert.pem"), dir.join("key.pem"));
    std::fs::copy(fixture("server.pem"), &cert).unwrap();
    std::fs::copy(fixture("server.key"), &key).unwrap();
    let tls = Tls::new(TlsConfig::new(
        cert.to_str().unwrap(),
        key.to_str().unwrap(),
    ))
    .unwrap();
    let mut config = ServerConfig::default();
    config.tls = Some(tls.clone());
    let addr = start(config).await;

    let mut established = handshake(addr, None).await.unwrap();
    assert_eq!(served_certificate(&established), certs("server.pem")[0]);
    assert_eq!(
        mqtt_connect(&mut established, "established", None).await,
        Some(0)
    );

    std::fs::copy(fixture("server2.pem"), &cert).unwrap();
    std::fs::copy(fixture("server2.key"), &key).unwrap();
    tls.reload().unwrap();

    let renewed = handshake(addr, None).await.unwrap();
    assert_eq!(served_certificate(&renewed), certs("server2.pem")[0]);
    ping(&mut established).await;

    // A key that doesn't match the certificate keeps the previous pair
    std::fs::copy(fixture("server.key"), &key).unwrap();
    assert!(tls.reload().is_err());
    std::fs::remove_file(&cert).unwrap();
    assert!(tls.reload().is_err());
    let kept = handshake(addr, None).await.unwrap();
    assert_eq!(served_certificate(&kept), certs("server2.pem")[0]);

    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn unusable_files_are_refused_up_front() {
    let missing = TlsConfig::new("/nonexistent/cert.pem", "/nonexistent/key.pem");
    let error = Tls::new(missing).unwrap_err();
    assert!(
        matches!(&error, TlsError::ReadCertificates { path, .. } if path == "/nonexistent/cert.pem"),
        "{error}"
    );
    let mismatched = TlsConfig::new(fixture("server.pem"), fixture("server2.key"));
    let error = Tls::new(mismatched).unwrap_err();
    assert!(matches!(error, TlsError::Configure(_)), "{error}");
}

/// A listener asking for client certificates signed by the test root CA,
/// naming clients by their certificate's Common Name, with `user` and
/// password `secret` for clients without one.
async fn start_with_client_certificates(files: TlsConfig) -> SocketAddr {
    let mut files = files.with_client_auth(fixture("root.pem"));
    files.username_from_certificate = Some(CertificateName::CommonName);
    let mut config = ServerConfig::default();
    config.tls = Some(Tls::new(files).unwrap());
    config.auth.require_auth = true;
    let users = [("user".to_string(), "secret".to_string())].into();
    config.plugins = vec![Arc::new(SimpleCredentialValidator::new(users))];
    start(config).await
}

fn server_files() -> TlsConfig {
    TlsConfig::new(fixture("server.pem"), fixture("server.key"))
}

/// Whether the broker refuses the client, at the handshake or by closing
/// the connection right after it, as TLS 1.3 servers do.
async fn refused(addr: SocketAddr, identity: Option<&str>) -> bool {
    match handshake(addr, identity).await {
        Ok(mut stream) => mqtt_connect(&mut stream, "refused", None).await.is_none(),
        Err(_) => true,
    }
}

#[tokio::test]
async fn required_client_certificates_refuse_clients_without_one() {
    let addr = start_with_client_certificates(server_files()).await;

    assert!(refused(addr, None).await);
    assert!(refused(addr, Some("mallory")).await);
    let mut alice = handshake(addr, Some("alice")).await.unwrap();
    assert_eq!(mqtt_connect(&mut alice, "alice", None).await, Some(0));
}

#[tokio::test]
async fn optional_client_certificates_let_clients_log_in_by_password() {
    let mut files = server_files();
    files.require_certificate = false;
    let addr = start_with_client_certificates(files).await;

    let mut password = handshake(addr, None).await.unwrap();
    let login = Some(("user", "secret"));
    assert_eq!(
        mqtt_connect(&mut password, "password", login).await,
        Some(0)
    );

    // Without a certificate the user name is not taken from one
    let mut anonymous = handshake(addr, None).await.unwrap();
    let refusal = mqtt_connect(&mut anonymous, "anonymous", None).await;
    assert!(matches!(refusal, Some(code) if code >= 0x80), "{refusal:?}");

    let mut alice = handshake(addr, Some("alice")).await.unwrap();
    assert_eq!(mqtt_connect(&mut alice, "alice", None).await, Some(0));

    assert!(refused(addr, Some("mallory")).await);
}

/// Keeps the client of every `Connected` event.
#[derive(Default)]
struct Connections(std::sync::Mutex<Vec<ClientInfo>>);

impl Plugin for Connections {
    fn name(&self) -> &str {
        "connections"
    }

    fn on_event<'a>(&'a self, event: &'a Event) -> BoxFuture<'a, ()> {
        if let Event::Connected { client, .. } = event {
            self.0.lock().unwrap().push(client.clone());
        }
        ready(())
    }
}

#[tokio::test]
async fn plugins_see_tls_and_the_certificate_name() {
    let connections = Arc::new(Connections::default());
    let mut files = server_files().with_client_auth(fixture("root.pem"));
    files.username_from_certificate = Some(CertificateName::CommonName);
    let mut config = ServerConfig::default();
    config.tls = Some(Tls::new(files).unwrap());
    config.plugins = vec![connections.clone()];
    let addr = start(config).await;

    let mut alice = handshake(addr, Some("alice")).await.unwrap();
    assert_eq!(mqtt_connect(&mut alice, "alice", None).await, Some(0));
    let seen = async {
        loop {
            if let Some(client) = connections.0.lock().unwrap().first() {
                return client.clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    let client = timeout(Duration::from_secs(2), seen).await.unwrap();
    assert_eq!(client.username.as_deref(), Some("alice"));
    let connection = client.connection.expect("the connection");
    assert!(connection.tls);
    assert_eq!(connection.certificate.as_deref(), Some("alice"));
}

#[test]
fn optional_client_certificates_need_a_ca() {
    let mut files = server_files();
    files.require_certificate = false;
    assert!(Tls::new(files).is_err());
}

/// A rustls configuration serving `server` that requires client
/// certificates signed by the test root CA.
fn rustls_config(server: &str) -> Arc<rustls::ServerConfig> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    roots.add_parsable_certificates(certs("root.pem"));
    let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
        Arc::new(roots),
        Arc::clone(&provider),
    )
    .build()
    .unwrap();
    let key = PrivateKeyDer::from_pem_file(fixture(&format!("{server}.key"))).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_client_cert_verifier(verifier)
        .with_single_cert(certs(&format!("{server}.pem")), key)
        .unwrap();
    Arc::new(config)
}

#[tokio::test]
async fn embedders_can_serve_their_own_rustls_configuration() {
    let tls = Tls::from_rustls(rustls_config("server2"), Some(CertificateName::CommonName));
    let mut config = ServerConfig::default();
    config.tls = Some(tls.clone());
    config.auth.require_auth = true;
    let addr = start(config).await;

    assert!(refused(addr, None).await);
    let mut alice = handshake(addr, Some("alice")).await.unwrap();
    assert_eq!(served_certificate(&alice), certs("server2.pem")[0]);
    // Named by its certificate, as no password was sent
    assert_eq!(mqtt_connect(&mut alice, "alice", None).await, Some(0));

    // There are no files to reread; the embedder swaps the configuration
    assert!(tls.reload().is_err());
    tls.set_server_config(rustls_config("server"));
    let renewed = handshake(addr, Some("alice")).await.unwrap();
    assert_eq!(served_certificate(&renewed), certs("server.pem")[0]);
}

/// A listener checking client certificates against the test root CA and
/// the CRLs of `crls`, with `tune` applied, open to anonymous clients.
async fn start_with_crls(test: &str, crls: &[&str], tune: fn(&mut TlsConfig)) -> SocketAddr {
    let path = scratch(test).join("crls.pem");
    let pem: String = crls
        .iter()
        .map(|crl| std::fs::read_to_string(fixture(crl)).unwrap())
        .collect();
    std::fs::write(&path, pem).unwrap();
    let mut files = server_files().with_client_auth(fixture("root.pem"));
    files.crl_path = Some(path.to_str().unwrap().to_string());
    tune(&mut files);
    let mut config = ServerConfig::default();
    config.tls = Some(Tls::new(files).unwrap());
    start(config).await
}

async fn accepted(addr: SocketAddr, identity: &str) -> bool {
    match handshake(addr, Some(identity)).await {
        Ok(mut stream) => mqtt_connect(&mut stream, identity, None).await == Some(0),
        Err(_) => false,
    }
}

/// Which of the test clients `addr` lets in: alice and dave from the
/// root CA, bob and carol from the intermediate; dave and carol are
/// revoked.
async fn admitted(addr: SocketAddr) -> Vec<&'static str> {
    let mut admitted = Vec::new();
    for name in ["alice", "dave", "bob", "carol"] {
        if accepted(addr, name).await {
            admitted.push(name);
        }
    }
    admitted
}

/// By default every certificate of the chain needs a CRL from its issuer
/// saying it isn't revoked: a client of the intermediate CA is refused
/// unless both the root's and the intermediate's CRLs are given.
#[tokio::test]
async fn by_default_every_certificate_of_the_chain_needs_a_crl() {
    let both = start_with_crls("crl-both", &["root.crl", "intermediate.crl"], |_| {}).await;
    assert_eq!(admitted(both).await, ["alice", "bob"]);

    let root = start_with_crls("crl-root", &["root.crl"], |_| {}).await;
    assert_eq!(admitted(root).await, ["alice"]);

    let intermediate = start_with_crls("crl-intermediate", &["intermediate.crl"], |_| {}).await;
    assert_eq!(admitted(intermediate).await, Vec::<&str>::new());
}

#[tokio::test]
async fn crls_can_cover_only_the_client_certificate() {
    let intermediate = start_with_crls("crl-leaf", &["intermediate.crl"], |files| {
        files.crl_end_entity_only = true;
    })
    .await;
    // Bob's intermediate isn't covered, and no longer needs to be; alice,
    // from the root CA, still is not covered
    assert_eq!(admitted(intermediate).await, ["bob"]);
}

#[tokio::test]
async fn crls_can_let_certificates_they_dont_cover_in() {
    let root = start_with_crls("crl-unknown", &["root.crl"], |files| {
        files.crl_allow_unknown = true;
    })
    .await;
    // Only revoked certificates are refused
    assert_eq!(admitted(root).await, ["alice", "bob", "carol"]);
}

#[test]
fn crl_settings_need_a_crl() {
    let mut files = server_files().with_client_auth(fixture("root.pem"));
    files.crl_end_entity_only = true;
    assert!(Tls::new(files).is_err());
    let mut files = server_files().with_client_auth(fixture("root.pem"));
    files.crl_allow_unknown = true;
    assert!(Tls::new(files).is_err());
}
