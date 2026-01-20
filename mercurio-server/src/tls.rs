//! TLS support for the MQTT broker.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
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
}

impl TlsConfig {
    /// Create a new TLS configuration.
    pub fn new(cert_path: impl Into<String>, key_path: impl Into<String>) -> Self {
        Self {
            cert_path: cert_path.into(),
            key_path: key_path.into(),
            ca_path: None,
        }
    }

    /// Enable client certificate authentication (mTLS).
    pub fn with_client_auth(mut self, ca_path: impl Into<String>) -> Self {
        self.ca_path = Some(ca_path.into());
        self
    }

    /// Build a TLS acceptor from this configuration.
    pub fn build_acceptor(&self) -> Result<TlsAcceptor, ServerError> {
        let certs = load_certs(&self.cert_path)?;
        let key = load_private_key(&self.key_path)?;

        let config = ServerConfig::builder();

        let config = if let Some(ref ca_path) = self.ca_path {
            // Enable client certificate authentication
            let ca_certs = load_certs(ca_path)?;
            let mut root_store = rustls::RootCertStore::empty();
            for cert in ca_certs {
                root_store
                    .add(cert)
                    .map_err(|e| ServerError::Tls(format!("Failed to add CA cert: {}", e)))?;
            }
            let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(root_store))
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
    let file = File::open(Path::new(path))
        .map_err(|e| ServerError::Tls(format!("Failed to open cert file '{}': {}", path, e)))?;
    let mut reader = BufReader::new(file);
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| ServerError::Tls(format!("Failed to parse certs from '{}': {}", path, e)))?;

    if certs.is_empty() {
        return Err(ServerError::Tls(format!(
            "No certificates found in '{}'",
            path
        )));
    }

    Ok(certs)
}

/// Load a private key from a PEM file.
fn load_private_key(path: &str) -> Result<PrivateKeyDer<'static>, ServerError> {
    let file = File::open(Path::new(path))
        .map_err(|e| ServerError::Tls(format!("Failed to open key file '{}': {}", path, e)))?;
    let mut reader = BufReader::new(file);

    // Try to read different key formats
    loop {
        match rustls_pemfile::read_one(&mut reader) {
            Ok(Some(rustls_pemfile::Item::Pkcs1Key(key))) => {
                return Ok(PrivateKeyDer::Pkcs1(key));
            }
            Ok(Some(rustls_pemfile::Item::Pkcs8Key(key))) => {
                return Ok(PrivateKeyDer::Pkcs8(key));
            }
            Ok(Some(rustls_pemfile::Item::Sec1Key(key))) => {
                return Ok(PrivateKeyDer::Sec1(key));
            }
            Ok(Some(_)) => continue, // Skip other items (certs, etc.)
            Ok(None) => break,
            Err(e) => {
                return Err(ServerError::Tls(format!(
                    "Failed to parse key from '{}': {}",
                    path, e
                )));
            }
        }
    }

    Err(ServerError::Tls(format!(
        "No private key found in '{}'",
        path
    )))
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
    fn test_tls_config_with_client_auth() {
        let config =
            TlsConfig::new("/path/to/cert.pem", "/path/to/key.pem").with_client_auth("/path/to/ca.pem");
        assert!(config.ca_path.is_some());
        assert_eq!(config.ca_path.unwrap(), "/path/to/ca.pem");
    }
}
