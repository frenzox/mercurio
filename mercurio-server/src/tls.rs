//! TLS support for the MQTT broker.

use std::sync::Arc;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, CertificateRevocationListDer, PrivateKeyDer};
use rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;

use crate::error::ServerError;

/// TLS configuration for the server.
#[derive(Debug, Clone)]
pub struct TlsConfig {
    /// Path to the certificate file (PEM format)
    pub cert_path: String,
    /// Path to the private key file (PEM format)
    pub key_path: String,
    /// Optional path to CA certificate for client authentication (mTLS)
    pub ca_path: Option<String>,
    /// Certificate revocation lists (PEM) to check client certificates
    /// against; requires `ca_path`
    pub crl_path: Option<String>,
    /// Oldest TLS version accepted
    pub min_version: TlsVersion,
    /// Use the Common Name of the client certificate as the client's user
    /// name, in place of the user name and password; requires `ca_path`
    pub identity_as_username: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TlsVersion {
    #[default]
    Tls12,
    Tls13,
}

impl TlsConfig {
    /// Create a new TLS configuration.
    pub fn new(cert_path: impl Into<String>, key_path: impl Into<String>) -> Self {
        Self {
            cert_path: cert_path.into(),
            key_path: key_path.into(),
            ca_path: None,
            crl_path: None,
            min_version: TlsVersion::default(),
            identity_as_username: false,
        }
    }

    /// Enable client certificate authentication (mTLS).
    pub fn with_client_auth(mut self, ca_path: impl Into<String>) -> Self {
        self.ca_path = Some(ca_path.into());
        self
    }

    /// Build a TLS acceptor from this configuration.
    pub fn build_acceptor(&self) -> Result<TlsAcceptor, ServerError> {
        if self.ca_path.is_none() && (self.crl_path.is_some() || self.identity_as_username) {
            return Err(ServerError::Tls(
                "Revocation lists and certificate identities need a CA to verify clients with"
                    .to_string(),
            ));
        }
        let certs = load_certs(&self.cert_path)?;
        let key = load_private_key(&self.key_path)?;

        let versions: &[&rustls::SupportedProtocolVersion] = match self.min_version {
            TlsVersion::Tls12 => rustls::DEFAULT_VERSIONS,
            TlsVersion::Tls13 => &[&rustls::version::TLS13],
        };
        // Named rather than taken from the process default, which rustls
        // can't pick when other crates enable a second provider
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let config = ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(versions)
            .map_err(|e| ServerError::Tls(format!("Unsupported TLS versions: {}", e)))?;

        let config = if let Some(ref ca_path) = self.ca_path {
            // Enable client certificate authentication
            let ca_certs = load_certs(ca_path)?;
            let mut root_store = rustls::RootCertStore::empty();
            for cert in ca_certs {
                root_store
                    .add(cert)
                    .map_err(|e| ServerError::Tls(format!("Failed to add CA cert: {}", e)))?;
            }
            let mut verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(root_store));
            if let Some(crl_path) = &self.crl_path {
                verifier = verifier.with_crls(load_crls(crl_path)?);
            }
            let verifier = verifier
                .build()
                .map_err(|e| ServerError::Tls(format!("Failed to build client verifier: {}", e)))?;
            config.with_client_cert_verifier(verifier)
        } else {
            // No client authentication
            config.with_no_client_auth()
        };

        let config = config
            .with_single_cert(certs, key)
            .map_err(|e| ServerError::Tls(format!("Failed to configure TLS: {}", e)))?;

        Ok(TlsAcceptor::from(Arc::new(config)))
    }
}

/// Load certificates from a PEM file.
fn load_certs(path: &str) -> Result<Vec<CertificateDer<'static>>, ServerError> {
    let certs = CertificateDer::pem_file_iter(path)
        .and_then(|certs| certs.collect::<Result<Vec<_>, _>>())
        .map_err(|e| ServerError::Tls(format!("Failed to read certs from '{}': {}", path, e)))?;

    if certs.is_empty() {
        return Err(ServerError::Tls(format!(
            "No certificates found in '{}'",
            path
        )));
    }

    Ok(certs)
}

/// Load certificate revocation lists from a PEM file.
fn load_crls(path: &str) -> Result<Vec<CertificateRevocationListDer<'static>>, ServerError> {
    CertificateRevocationListDer::pem_file_iter(path)
        .and_then(|crls| crls.collect::<Result<Vec<_>, _>>())
        .map_err(|e| ServerError::Tls(format!("Failed to read CRLs from '{}': {}", path, e)))
}

/// The Common Name of a DER certificate's subject.
pub(crate) fn common_name(cert: &CertificateDer<'_>) -> Option<String> {
    let (_, cert) = x509_parser::parse_x509_certificate(cert).ok()?;
    let cn = cert.subject().iter_common_name().next()?;
    cn.as_str().ok().map(str::to_string)
}

/// Load the first private key, in PKCS#1, PKCS#8 or SEC1 format, from a PEM
/// file.
fn load_private_key(path: &str) -> Result<PrivateKeyDer<'static>, ServerError> {
    PrivateKeyDer::from_pem_file(path)
        .map_err(|e| ServerError::Tls(format!("Failed to read key from '{}': {}", path, e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tls_config_new() {
        let config = TlsConfig::new("/path/to/cert.pem", "/path/to/key.pem");
        assert_eq!(config.cert_path, "/path/to/cert.pem");
        assert_eq!(config.key_path, "/path/to/key.pem");
        assert!(config.ca_path.is_none());
    }

    #[test]
    fn test_client_certificate_options_need_a_ca() {
        let mut config = TlsConfig::new("/path/to/cert.pem", "/path/to/key.pem");
        config.identity_as_username = true;
        let Err(ServerError::Tls(message)) = config.build_acceptor() else {
            panic!("accepted identity_as_username without a CA");
        };
        assert!(message.contains("need a CA"), "{message}");
    }

    #[test]
    fn test_tls_config_with_client_auth() {
        let config = TlsConfig::new("/path/to/cert.pem", "/path/to/key.pem")
            .with_client_auth("/path/to/ca.pem");
        assert!(config.ca_path.is_some());
        assert_eq!(config.ca_path.unwrap(), "/path/to/ca.pem");
    }
}
