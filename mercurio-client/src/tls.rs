//! TLS support for the MQTT client.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;

use rustls::pki_types::{CertificateDer, ServerName};
use rustls::ClientConfig;
use tokio_rustls::TlsConnector;

use crate::error::ClientError;
use crate::options::TlsOptions;

/// Build a TLS connector from the given options.
pub fn build_tls_connector(
    options: &TlsOptions,
    host: &str,
) -> Result<(TlsConnector, ServerName<'static>), ClientError> {
    let server_name = ServerName::try_from(host.to_string())
        .map_err(|_| ClientError::Tls(format!("Invalid server name: {}", host)))?;

    let config = if options.danger_skip_verify {
        // WARNING: This disables all certificate verification - only for testing!
        ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerifier))
            .with_no_client_auth()
    } else if let Some(ref ca_path) = options.ca_path {
        // Use custom CA certificate
        let ca_certs = load_certs(ca_path)?;
        let mut root_store = rustls::RootCertStore::empty();
        for cert in ca_certs {
            root_store
                .add(cert)
                .map_err(|e| ClientError::Tls(format!("Failed to add CA cert: {}", e)))?;
        }
        ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth()
    } else {
        // Use system root certificates
        let root_store =
            rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth()
    };

    Ok((TlsConnector::from(Arc::new(config)), server_name))
}

/// Load certificates from a PEM file.
fn load_certs(path: &str) -> Result<Vec<CertificateDer<'static>>, ClientError> {
    let file = File::open(Path::new(path))
        .map_err(|e| ClientError::Tls(format!("Failed to open cert file '{}': {}", path, e)))?;
    let mut reader = BufReader::new(file);
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| ClientError::Tls(format!("Failed to parse certs from '{}': {}", path, e)))?;

    if certs.is_empty() {
        return Err(ClientError::Tls(format!(
            "No certificates found in '{}'",
            path
        )));
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
