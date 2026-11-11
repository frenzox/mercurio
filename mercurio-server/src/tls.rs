//! TLS support for the MQTT broker.

use std::sync::{Arc, RwLock};

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, CertificateRevocationListDer, PrivateKeyDer};
use rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;

use crate::error::ServerError;

/// TLS configuration for the server.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct TlsConfig {
    /// Path to the certificate file (PEM format)
    pub cert_path: String,
    /// Path to the private key file (PEM format)
    pub key_path: String,
    /// Optional path to CA certificate for client authentication (mTLS)
    pub ca_path: Option<String>,
    /// With `ca_path`, refuse clients that present no certificate. When
    /// false, they connect and authenticate as on a listener without
    /// `ca_path`; a certificate that is presented must still be valid.
    pub require_certificate: bool,
    /// Certificate revocation lists (PEM) to check client certificates
    /// against; requires `ca_path`
    pub crl_path: Option<String>,
    /// Oldest TLS version accepted
    pub min_version: TlsVersion,
    /// Take the client's user name from its certificate, in place of the
    /// user name and password it sends; requires `ca_path`
    pub username_from_certificate: Option<CertificateName>,
}

/// What part of a client certificate names the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CertificateName {
    /// The subject's Common Name, as mosquitto's `use_identity_as_username`
    CommonName,
    /// The whole subject in RFC 2253 form, such as `CN=alice,O=Example`, as
    /// mosquitto's `use_subject_as_username`
    Subject,
}

/// The oldest TLS version a listener accepts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
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
            require_certificate: true,
            crl_path: None,
            min_version: TlsVersion::default(),
            username_from_certificate: None,
        }
    }

    /// Enable client certificate authentication (mTLS).
    pub fn with_client_auth(mut self, ca_path: impl Into<String>) -> Self {
        self.ca_path = Some(ca_path.into());
        self
    }

    /// Reads the files and builds the rustls configuration they describe.
    pub fn server_config(&self) -> Result<Arc<ServerConfig>, ServerError> {
        let certificate_options = self.crl_path.is_some()
            || self.username_from_certificate.is_some()
            || !self.require_certificate;
        if self.ca_path.is_none() && certificate_options {
            return Err(ServerError::Tls(
                "Revocation lists, optional client certificates and certificate identities \
                 need a CA to verify clients with"
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
            if !self.require_certificate {
                verifier = verifier.allow_unauthenticated();
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

        Ok(Arc::new(config))
    }
}

/// The TLS a listener serves. Clones share their settings: a reload
/// through any of them reaches every listener given one. Connections
/// already up keep the settings they started with.
#[derive(Clone)]
pub struct Tls {
    current: Arc<RwLock<Arc<ServerConfig>>>,
    files: Arc<TlsConfig>,
}

impl std::fmt::Debug for Tls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tls").field("files", &self.files).finish()
    }
}

impl Tls {
    /// Reads the files of `config`, failing if they can't be used.
    pub fn new(config: TlsConfig) -> Result<Self, ServerError> {
        Ok(Self {
            current: Arc::new(RwLock::new(config.server_config()?)),
            files: Arc::new(config),
        })
    }

    /// Reads the files again, for connections from now on. On error the
    /// settings in use are kept.
    pub fn reload(&self) -> Result<(), ServerError> {
        let config = self.files.server_config()?;
        *self.current.write().unwrap() = config;
        Ok(())
    }

    /// The files read, as given to [`Tls::new`].
    pub fn files(&self) -> &TlsConfig {
        &self.files
    }

    pub(crate) fn username_from_certificate(&self) -> Option<CertificateName> {
        self.files.username_from_certificate
    }

    pub(crate) fn acceptor(&self) -> TlsAcceptor {
        TlsAcceptor::from(Arc::clone(&self.current.read().unwrap()))
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

/// The name `part` of a DER certificate gives.
pub(crate) fn certificate_name(cert: &CertificateDer<'_>, part: CertificateName) -> Option<String> {
    let (_, cert) = x509_parser::parse_x509_certificate(cert).ok()?;
    match part {
        CertificateName::CommonName => {
            let cn = cert.subject().iter_common_name().next()?;
            cn.as_str().ok().map(str::to_string)
        }
        CertificateName::Subject => rfc2253(cert.subject()),
    }
}

/// A distinguished name as RFC 2253 writes it: last component first,
/// attributes by their short names, special characters escaped.
fn rfc2253(name: &x509_parser::x509::X509Name<'_>) -> Option<String> {
    let registry = x509_parser::objects::oid_registry();
    let mut components = Vec::new();
    for rdn in name.iter() {
        let mut attributes = Vec::new();
        for attribute in rdn.iter() {
            let oid = attribute.attr_type();
            let key = x509_parser::objects::oid2abbrev(oid, registry)
                .map(str::to_string)
                .unwrap_or_else(|_| oid.to_id_string());
            attributes.push(format!("{key}={}", escape(attribute.as_str().ok()?)));
        }
        components.push(attributes.join("+"));
    }
    components.reverse();
    Some(components.join(","))
}

fn escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for (i, c) in value.chars().enumerate() {
        let leading = i == 0 && (c == ' ' || c == '#');
        let trailing = i == value.chars().count() - 1 && c == ' ';
        if leading || trailing || matches!(c, ',' | '+' | '"' | '\\' | '<' | '>' | ';') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
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
    fn test_rfc2253_escaping() {
        assert_eq!(escape("Acme, Inc"), "Acme\\, Inc");
        assert_eq!(escape(" #x+y "), "\\ #x\\+y\\ ");
        assert_eq!(escape("#tag"), "\\#tag");
        assert_eq!(escape("plain"), "plain");
    }

    #[test]
    fn test_client_certificate_options_need_a_ca() {
        let mut config = TlsConfig::new("/path/to/cert.pem", "/path/to/key.pem");
        config.username_from_certificate = Some(CertificateName::CommonName);
        let Err(ServerError::Tls(message)) = Tls::new(config) else {
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
