//! TLS listeners, against the test PKI of `tests/fixtures/pki`.

use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use mercurio_server::{
    server::{run_with_config, ServerConfig},
    tls::{Tls, TlsConfig},
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
    assert!(Tls::new(missing).is_err());
    let mismatched = TlsConfig::new(fixture("server.pem"), fixture("server2.key"));
    assert!(Tls::new(mismatched).is_err());
}
