//! TLS support for the MQTT client.

use std::sync::Arc;

use mercurio_scram::{TLS_EXPORTER_LABEL, TLS_EXPORTER_LEN};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConfig, RootCertStore};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::time::timeout;
use tokio_rustls::client::TlsStream;
use tokio_rustls::TlsConnector;
use tracing::{debug, info, warn};

use crate::error::ClientError;
use crate::options::{ConnectOptions, TlsOptions};

/// Runs the TLS handshake with the broker over `stream`, within the
/// connect timeout, returning the stream with its tls-exporter channel
/// binding data (RFC 9266), which only TLS 1.3 has.
pub(crate) async fn handshake<S>(
    options: &ConnectOptions,
    stream: S,
) -> Result<(TlsStream<S>, Option<[u8; 32]>), ClientError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    info!("Establishing TLS connection");
    let (connector, server_name) = build_tls_connector(&options.tls, &options.host)?;
    let stream = timeout(
        options.connect_timeout,
        connector.connect(server_name, stream),
    )
    .await
    .map_err(|_| ClientError::Timeout)?
    .map_err(handshake_error)?;
    let connection = stream.get_ref().1;
    let exporter = (connection.protocol_version() == Some(rustls::ProtocolVersion::TLSv1_3))
        .then(|| {
            connection
                .export_keying_material([0u8; TLS_EXPORTER_LEN], TLS_EXPORTER_LABEL, Some(&[]))
                .ok()
        })
        .flatten();
    Ok((stream, exporter))
}

/// A TLS handshake that failed over the certificate or the TLS settings,
/// which rustls reports as invalid data, or over the connection itself.
fn handshake_error(error: std::io::Error) -> ClientError {
    if error.kind() == std::io::ErrorKind::InvalidData {
        ClientError::tls("TLS handshake failed", Some(error.into()))
    } else {
        ClientError::ConnectionFailed(error)
    }
}

/// Build a TLS connector from the given options.
pub(crate) fn build_tls_connector(
    options: &TlsOptions,
    host: &str,
) -> Result<(TlsConnector, ServerName<'static>), ClientError> {
    let server_name = ServerName::try_from(host.to_string())
        .map_err(|e| ClientError::tls(format!("invalid server name `{host}`"), Some(e.into())))?;
    if let Some(config) = &options.config {
        return Ok((TlsConnector::from(Arc::clone(config)), server_name));
    }

    let builder = if options.danger_skip_verify {
        config_builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerifier))
    } else if let Some(ref ca_path) = options.ca_path {
        let ca_certs = load_certs(ca_path)?;
        let mut root_store = rustls::RootCertStore::empty();
        for cert in ca_certs {
            root_store
                .add(cert)
                .map_err(|e| ClientError::tls("invalid CA certificate", Some(e.into())))?;
        }
        config_builder().with_root_certificates(root_store)
    } else {
        config_builder().with_root_certificates(platform_roots())
    };

    let mut config = match (&options.cert_path, &options.key_path) {
        (Some(cert_path), Some(key_path)) => {
            let key = PrivateKeyDer::from_pem_file(key_path).map_err(|e| {
                ClientError::tls(format!("reading the key in '{key_path}'"), Some(e.into()))
            })?;
            builder
                .with_client_auth_cert(load_certs(cert_path)?, key)
                .map_err(|e| ClientError::tls("invalid client certificate", Some(e.into())))?
        }
        (None, None) => builder.with_no_client_auth(),
        _ => {
            return Err(ClientError::tls(
                "a client certificate needs both a certificate and a key",
                None,
            ))
        }
    };

    config.alpn_protocols = options.alpn.clone();
    Ok((TlsConnector::from(Arc::new(config)), server_name))
}

/// The certificates the operating system trusts.
fn platform_roots() -> RootCertStore {
    let loaded = rustls_native_certs::load_native_certs();
    for error in &loaded.errors {
        debug!("Reading the system's trusted certificates: {}", error);
    }
    roots_from(loaded.certs)
}

/// A root store holding `certs`, or the Mozilla roots bundled with the
/// client when none of them can be used, so a system without a
/// certificate store can still reach brokers with public certificates.
fn roots_from(certs: Vec<CertificateDer<'static>>) -> RootCertStore {
    let mut roots = RootCertStore::empty();
    let (_, ignored) = roots.add_parsable_certificates(certs);
    if ignored > 0 {
        debug!("Ignored {} unusable certificates of the system", ignored);
    }
    if roots.is_empty() {
        warn!(
            "No trusted certificates in the operating system's store; \
             trusting the Mozilla roots bundled with the client instead"
        );
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    }
    roots
}

/// Named rather than taken from the process default, which rustls can't
/// pick when other crates enable a second provider.
fn config_builder() -> rustls::ConfigBuilder<ClientConfig, rustls::WantsVerifier> {
    ClientConfig::builder_with_provider(Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
        .with_safe_default_protocol_versions()
        .expect("the provider supports the default protocol versions")
}

/// Load certificates from a PEM file.
fn load_certs(path: &str) -> Result<Vec<CertificateDer<'static>>, ClientError> {
    let certs = CertificateDer::pem_file_iter(path)
        .and_then(|certs| certs.collect::<Result<Vec<_>, _>>())
        .map_err(|e| {
            ClientError::tls(
                format!("reading the certificates in '{path}'"),
                Some(e.into()),
            )
        })?;

    if certs.is_empty() {
        return Err(ClientError::tls(
            format!("no certificates in '{path}'"),
            None,
        ));
    }

    Ok(certs)
}

/// A certificate verifier that accepts any certificate (for testing only).
#[derive(Debug)]
struct NoVerifier;

impl rustls::client::danger::ServerCertVerifier for NoVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::RSA_PKCS1_SHA384,
            rustls::SignatureScheme::RSA_PKCS1_SHA512,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            rustls::SignatureScheme::ECDSA_NISTP521_SHA512,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PSS_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA512,
            rustls::SignatureScheme::ED25519,
        ]
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::io;
    use std::net::SocketAddr;

    use tokio::net::{TcpListener, TcpStream};
    use tokio::task::JoinHandle;
    use tokio_rustls::TlsAcceptor;

    use super::*;

    /// The test CA, which signed `CERT`. The broker's own test certificate
    /// can't serve here: it is a CA certificate, which webpki refuses as a
    /// server's.
    const CA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/ca.pem");
    /// For `localhost` and 127.0.0.1
    const CERT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/server.pem");
    const KEY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/server-key.pem");
    /// A key that doesn't belong to `CERT`
    const OTHER_KEY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/other-key.pem");

    fn options() -> ConnectOptions {
        ConnectOptions::new("localhost", 8883)
    }

    fn connector_error(options: ConnectOptions) -> ClientError {
        match build_tls_connector(&options.tls, &options.host) {
            Ok(_) => panic!("the TLS settings were accepted"),
            Err(error) => error,
        }
    }

    /// A rustls server for `localhost` offering `alpn`, accepting one
    /// connection and answering with the protocol it agreed on.
    async fn server(alpn: &[&str]) -> (SocketAddr, JoinHandle<io::Result<Option<Vec<u8>>>>) {
        let key = PrivateKeyDer::from_pem_file(KEY).unwrap();
        let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(load_certs(CERT).unwrap(), key)
        .unwrap();
        config.alpn_protocols = alpn.iter().map(|p| p.as_bytes().to_vec()).collect();
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let accepted = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let stream = acceptor.accept(stream).await?;
            Ok(stream.get_ref().1.alpn_protocol().map(<[u8]>::to_vec))
        });
        (address, accepted)
    }

    async fn connect(
        options: ConnectOptions,
        address: SocketAddr,
    ) -> Result<TlsStream<TcpStream>, ClientError> {
        handshake(&options, TcpStream::connect(address).await.unwrap())
            .await
            .map(|(stream, _)| stream)
    }

    #[test]
    fn a_ca_file_is_loaded() {
        assert!(build_tls_connector(&options().ca_path(CA).tls, "localhost").is_ok());
    }

    #[test]
    fn a_missing_ca_file_is_reported_with_its_cause() {
        let error = connector_error(options().ca_path("/nonexistent/ca.pem"));
        assert!(matches!(error, ClientError::Tls { .. }), "{error:?}");
        assert!(error.to_string().contains("/nonexistent/ca.pem"), "{error}");
        assert!(error.source().is_some(), "{error:?}");
        assert!(!error.is_transient());
    }

    #[test]
    fn validating_loads_the_tls_files_without_connecting() {
        assert!(options()
            .ca_path(CA)
            .client_cert(CERT, KEY)
            .validate()
            .is_ok());
        let error = options()
            .ca_path("/nonexistent/ca.pem")
            .validate()
            .unwrap_err();
        assert!(error.to_string().contains("/nonexistent/ca.pem"), "{error}");
        let error = ConnectOptions::new("not a host name", 8883)
            .ca_path(CA)
            .validate()
            .unwrap_err();
        assert!(error.to_string().contains("not a host name"), "{error}");
    }

    #[test]
    fn a_ca_file_without_certificates_is_refused() {
        let error = connector_error(options().ca_path(KEY));
        assert!(error.to_string().contains("no certificates"), "{error}");
    }

    #[test]
    fn a_client_certificate_loads_with_its_key() {
        let options = options().ca_path(CA).client_cert(CERT, KEY);
        assert!(build_tls_connector(&options.tls, "localhost").is_ok());
    }

    #[test]
    fn a_client_certificate_with_a_missing_key_is_refused() {
        let error = connector_error(options().client_cert(CERT, "/nonexistent/key.pem"));
        assert!(matches!(error, ClientError::Tls { .. }), "{error:?}");
        assert!(
            error.to_string().contains("/nonexistent/key.pem"),
            "{error}"
        );
        assert!(error.source().is_some(), "{error:?}");
    }

    #[test]
    fn a_client_certificate_with_another_key_is_refused() {
        let error = connector_error(options().client_cert(CERT, OTHER_KEY));
        assert!(matches!(error, ClientError::Tls { .. }), "{error:?}");
        assert!(error.to_string().contains("client certificate"), "{error}");
    }

    #[test]
    fn without_a_ca_file_the_platform_store_is_trusted() {
        let platform = rustls_native_certs::load_native_certs().certs;
        let roots = roots_from(platform.clone());
        if platform.is_empty() {
            assert_eq!(roots.len(), webpki_roots::TLS_SERVER_ROOTS.len());
        } else {
            assert!(roots.len() <= platform.len());
            assert!(!roots.is_empty());
        }
    }

    #[test]
    fn an_empty_platform_store_falls_back_to_the_bundled_roots() {
        let roots = roots_from(Vec::new());
        assert_eq!(roots.len(), webpki_roots::TLS_SERVER_ROOTS.len());
        assert_eq!(roots_from(load_certs(CA).unwrap()).len(), 1);
    }

    #[tokio::test]
    async fn the_handshake_verifies_the_broker_with_the_ca_file_and_offers_alpn() {
        let (address, server) = server(&["mqtt"]).await;
        let options = options().ca_path(CA).alpn_protocols(["mqtt"]);
        let stream = connect(options, address).await.unwrap();
        assert_eq!(stream.get_ref().1.alpn_protocol(), Some(&b"mqtt"[..]));
        assert_eq!(
            server.await.unwrap().unwrap().as_deref(),
            Some(&b"mqtt"[..])
        );
    }

    #[tokio::test]
    async fn an_unknown_certificate_fails_the_handshake() {
        let (address, _server) = server(&[]).await;
        let error = connect(options().tls(true), address).await.unwrap_err();
        assert!(matches!(error, ClientError::Tls { .. }), "{error:?}");
        let source = error.source().expect("the rustls error");
        assert!(source.to_string().contains("certificate"), "{source}");
        assert!(!error.is_transient());
    }

    #[tokio::test]
    async fn skipping_verification_accepts_an_unknown_certificate() {
        let (address, server) = server(&[]).await;
        connect(options().danger_skip_tls_verify(true), address)
            .await
            .unwrap();
        assert_eq!(server.await.unwrap().unwrap(), None);
    }

    #[tokio::test]
    async fn a_configuration_of_its_own_replaces_the_settings() {
        let mut roots = rustls::RootCertStore::empty();
        roots.add_parsable_certificates(load_certs(CA).unwrap());
        let mut config = config_builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        config.alpn_protocols = vec![b"mqtt".to_vec()];
        let (address, server) = server(&["mqtt"]).await;
        let options = options().tls_config(Arc::new(config));
        assert!(options.tls.enabled);
        connect(options, address).await.unwrap();
        assert_eq!(
            server.await.unwrap().unwrap().as_deref(),
            Some(&b"mqtt"[..])
        );
    }
}
