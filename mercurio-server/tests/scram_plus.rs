//! SCRAM-SHA-256-PLUS: channel binding to the TLS 1.3 connection.

mod common;

use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};

use bytes::{Bytes, BytesMut};
use mercurio_core::{
    properties::{AuthenticationData, AuthenticationMethod},
    protocol::ProtocolVersion,
    reason::ReasonCode,
};
use mercurio_packets::{
    auth::{AuthPacket, AuthProperties},
    connect::{ConnectPacket, ConnectPayload, ConnectProperties},
    ControlPacket,
};
use mercurio_scram::{
    ClientBinding, ScramClient, StoredCredentials, MECHANISM, MECHANISM_PLUS, TLS_EXPORTER_LABEL,
};
use mercurio_server::{
    auth::ScramAuth,
    server::{Limits, ListenerConfig, LoginFailures, ServerConfig, Transport},
    tls::{Tls, TlsConfig},
};
use rustls::pki_types::{pem::PemObject, CertificateDer, ServerName};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};
use tokio_rustls::TlsConnector;

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/pki/{name}", env!("CARGO_MANIFEST_DIR"))
}

/// Brokers with a plain and a TLS listener, the TLS one behind a PROXY
/// header so clients can claim addresses other than loopback, and a ban
/// after two failed logins. Returns the plain and the TLS addresses.
async fn start(require_binding: bool) -> (SocketAddr, SocketAddr) {
    let credentials = StoredCredentials::generate("pencil", 4096).unwrap();
    let users: HashMap<String, StoredCredentials> = [("user".to_string(), credentials)].into();
    let mut config = ServerConfig::default();
    config.plugins = vec![Arc::new(
        ScramAuth::new(Arc::new(users)).require_channel_binding(require_binding),
    )];
    config.limits = {
        let mut value = Limits::default();
        value.login_failures = Some(LoginFailures::new(
            2,
            Duration::from_secs(60),
            Duration::from_secs(300),
        ));
        value
    };
    let plain = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let secure = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addrs = (plain.local_addr().unwrap(), secure.local_addr().unwrap());
    let tls = Tls::new(TlsConfig::new(fixture("server.pem"), fixture("server.key"))).unwrap();
    let mut listener = ListenerConfig::new(secure, Transport::Mqtt).with_tls(tls);
    listener.proxy_protocol = true;
    listener.proxy_from = vec!["127.0.0.1".parse().unwrap()];
    tokio::spawn(
        common::broker(plain, config)
            .listener(listener)
            .run(std::future::pending::<()>()),
    );
    addrs
}

/// A TLS 1.3 connection, claiming to come from `source`, and its exporter;
/// None when the broker closes it during the handshake.
async fn try_tls_connect(
    addr: SocketAddr,
    source: &str,
) -> Option<(tokio_rustls::client::TlsStream<TcpStream>, [u8; 32])> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    roots.add_parsable_certificates(
        CertificateDer::pem_file_iter(fixture("root.pem"))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap(),
    );
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let mut socket = TcpStream::connect(addr).await.unwrap();
    let header = format!("PROXY TCP4 {source} 127.0.0.1 5000 8883\r\n");
    socket.write_all(header.as_bytes()).await.unwrap();
    let stream = TlsConnector::from(Arc::new(config))
        .connect(ServerName::try_from("localhost").unwrap(), socket)
        .await
        .ok()?;
    let exporter = stream
        .get_ref()
        .1
        .export_keying_material([0u8; 32], TLS_EXPORTER_LABEL, Some(&[]))
        .unwrap();
    Some((stream, exporter))
}

async fn tls_connect(
    addr: SocketAddr,
    source: &str,
) -> (tokio_rustls::client::TlsStream<TcpStream>, [u8; 32]) {
    try_tls_connect(addr, source).await.unwrap()
}

async fn send<S: AsyncWrite + Unpin>(stream: &mut S, packet: ControlPacket) {
    let mut buffer = BytesMut::new();
    packet.encode(&mut buffer, ProtocolVersion::V5).unwrap();
    stream.write_all(&buffer).await.unwrap();
}

/// The next packet, or None when the broker closed the connection.
async fn receive<S: AsyncRead + Unpin>(
    stream: &mut S,
    buffer: &mut BytesMut,
) -> Option<ControlPacket> {
    loop {
        if ControlPacket::check(buffer).is_ok() {
            return Some(ControlPacket::parse(buffer, ProtocolVersion::V5).unwrap());
        }
        let read = timeout(Duration::from_secs(2), stream.read_buf(buffer))
            .await
            .expect("timed out waiting for broker")
            .unwrap_or(0);
        if read == 0 {
            return None;
        }
    }
}

fn connect(method: &str, first: String) -> ControlPacket {
    ControlPacket::Connect(ConnectPacket {
        protocol_version: ProtocolVersion::V5,
        clean_start: true,
        keepalive: 60,
        properties: Some(ConnectProperties {
            authentication_method: Some(AuthenticationMethod::new(method.into())),
            authentication_data: Some(AuthenticationData::new(Bytes::from(first))),
            ..Default::default()
        }),
        payload: ConnectPayload {
            client_id: "c".into(),
            ..Default::default()
        },
    })
}

/// Runs an exchange as `client` with `method`, and returns the CONNACK's
/// reason code, or None when the connection was closed. On success the
/// server signature has been checked.
async fn login<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    method: &str,
    mut client: ScramClient,
    first: Option<String>,
) -> Option<ReasonCode> {
    let first = first.unwrap_or_else(|| client.start().unwrap());
    send(stream, connect(method, first)).await;
    let mut buffer = BytesMut::new();
    let mut packet = receive(stream, &mut buffer).await?;
    if let ControlPacket::Auth(auth) = packet {
        let server_first = auth.properties.auth_data.unwrap().value;
        let answer = client.respond(&server_first).unwrap();
        send(
            stream,
            ControlPacket::Auth(AuthPacket {
                reason: ReasonCode::ContinueAuthentication,
                properties: AuthProperties {
                    auth_method: Some(AuthenticationMethod::new(method.into())),
                    auth_data: Some(AuthenticationData::new(Bytes::from(answer))),
                    ..Default::default()
                },
            }),
        )
        .await;
        packet = receive(stream, &mut buffer).await?;
    }
    let ControlPacket::ConnAck(connack) = packet else {
        panic!("Expected CONNACK");
    };
    if connack.reason_code == ReasonCode::Success {
        let data = connack.properties.unwrap().authentication_data.unwrap();
        client.finish(&data.value).unwrap();
    }
    Some(connack.reason_code)
}

fn client(exporter: Option<[u8; 32]>) -> ScramClient {
    let mut client = ScramClient::new("user", "pencil");
    if let Some(exporter) = exporter {
        client.set_binding(ClientBinding::TlsExporter(exporter));
    }
    client
}

#[tokio::test]
async fn plus_over_tls_1_3_succeeds() {
    let (_, secure) = start(false).await;
    let (mut stream, exporter) = tls_connect(secure, "192.0.2.1").await;
    let code = login(&mut stream, MECHANISM_PLUS, client(Some(exporter)), None).await;
    assert_eq!(code, Some(ReasonCode::Success));
}

#[tokio::test]
async fn plus_over_plain_tcp_is_refused() {
    let (plain, _) = start(false).await;
    let mut stream = TcpStream::connect(plain).await.unwrap();
    let code = login(&mut stream, MECHANISM_PLUS, client(Some([7; 32])), None).await;
    assert_eq!(code, Some(ReasonCode::BadAuthenticationMethod));
}

#[tokio::test]
async fn plus_with_the_wrong_exporter_is_a_failed_login() {
    let (_, secure) = start(false).await;
    let attacker = "192.0.2.7";
    for _ in 0..2 {
        let (mut stream, mut exporter) = tls_connect(secure, attacker).await;
        exporter[0] ^= 1;
        let code = login(&mut stream, MECHANISM_PLUS, client(Some(exporter)), None).await;
        assert_ne!(code, Some(ReasonCode::Success));
        assert!(code.is_some());
    }
    // Banned now, even with the right exporter
    if let Some((mut stream, exporter)) = try_tls_connect(secure, attacker).await {
        let code = login(&mut stream, MECHANISM_PLUS, client(Some(exporter)), None).await;
        assert_eq!(code, None);
    }
}

#[tokio::test]
async fn the_method_and_the_binding_flag_must_agree() {
    let (_, secure) = start(false).await;
    let (mut stream, exporter) = tls_connect(secure, "192.0.2.2").await;
    // p=tls-exporter under the method without binding
    let code = login(&mut stream, MECHANISM, client(Some(exporter)), None).await;
    assert_eq!(code, Some(ReasonCode::BadAuthenticationMethod));

    // n under the PLUS method
    let (mut stream, _) = tls_connect(secure, "192.0.2.3").await;
    let code = login(&mut stream, MECHANISM_PLUS, client(None), None).await;
    assert_eq!(code, Some(ReasonCode::BadAuthenticationMethod));
}

#[tokio::test]
async fn a_client_believing_the_server_lacks_binding_is_refused() {
    let (_, secure) = start(false).await;
    let (mut stream, _) = tls_connect(secure, "192.0.2.4").await;
    let first = Some("y,,n=user,r=abcdef".to_string());
    let code = login(&mut stream, MECHANISM, client(None), first).await;
    assert_eq!(code, Some(ReasonCode::BadAuthenticationMethod));
}

#[tokio::test]
async fn required_binding_refuses_plain_scram_over_tls_only() {
    let (plain, secure) = start(true).await;
    let (mut stream, _) = tls_connect(secure, "192.0.2.5").await;
    let code = login(&mut stream, MECHANISM, client(None), None).await;
    assert_eq!(code, Some(ReasonCode::BadAuthenticationMethod));

    let mut stream = TcpStream::connect(plain).await.unwrap();
    let code = login(&mut stream, MECHANISM, client(None), None).await;
    assert_eq!(code, Some(ReasonCode::Success));
}
