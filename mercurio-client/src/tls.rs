//! TLS support for the MQTT client.

use std::sync::Arc;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
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
        .map_err(|e| ClientError::tls(format!("invalid server name `{host}`"), Some(e.into())))?;

    let builder = if options.danger_skip_verify {
        // WARNING: This disables all certificate verification - only for testing!
        config_builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerifier))
    } else if let Some(ref ca_path) = options.ca_path {
        // Use custom CA certificate
        let ca_certs = load_certs(ca_path)?;
        let mut root_store = rustls::RootCertStore::empty();
        for cert in ca_certs {
            root_store
                .add(cert)
                .map_err(|e| ClientError::tls("invalid CA certificate", Some(e.into())))?;
        }
        config_builder().with_root_certificates(root_store)
    } else {
        // Use system root certificates
        let root_store =
            rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        config_builder().with_root_certificates(root_store)
    };

    let config = match (&options.cert_path, &options.key_path) {
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

    Ok((TlsConnector::from(Arc::new(config)), server_name))
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
