//! Server error types.

use std::fmt;

use thiserror::Error;

/// Server-specific errors.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ServerError {
    /// TLS configuration error.
    #[error("TLS error: {0}")]
    Tls(#[from] TlsError),

    /// I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Configuration error.
    #[error("Configuration error: {0}")]
    Config(#[from] ConfigError),

    /// The broker's state couldn't be read from storage.
    #[error("Storage error: {0}")]
    Storage(#[from] mercurio_storage::StorageError),
}

/// TLS settings that can't be served, or files they name that can't be
/// read.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum TlsError {
    #[error(
        "Revocation lists, optional client certificates and certificate identities need a CA \
         to verify clients with"
    )]
    NeedsCa,
    #[error("How revocation lists are checked needs revocation lists to check")]
    NeedsRevocationLists,
    #[error("Failed to read certs from '{path}': {source}")]
    ReadCertificates {
        path: String,
        #[source]
        source: rustls::pki_types::pem::Error,
    },
    #[error("No certificates found in '{path}'")]
    NoCertificates { path: String },
    #[error("Failed to read CRLs from '{path}': {source}")]
    ReadRevocationLists {
        path: String,
        #[source]
        source: rustls::pki_types::pem::Error,
    },
    #[error("Failed to read key from '{path}': {source}")]
    ReadKey {
        path: String,
        #[source]
        source: rustls::pki_types::pem::Error,
    },
    #[error("Unsupported TLS versions: {0}")]
    Versions(#[source] rustls::Error),
    #[error("Failed to add CA cert: {0}")]
    CaCertificate(#[source] rustls::Error),
    #[error("Failed to build client verifier: {0}")]
    ClientVerifier(#[source] rustls::server::VerifierBuilderError),
    #[error("Failed to configure TLS: {0}")]
    Configure(#[source] rustls::Error),
    /// [`Tls::reload`](crate::tls::Tls::reload) of a configuration built
    /// with [`Tls::from_rustls`](crate::tls::Tls::from_rustls)
    #[error("the TLS configuration was not read from files")]
    NotFromFiles,
}

/// A listener whose settings can't be served together.
#[derive(Debug)]
#[non_exhaustive]
pub struct ConfigError {
    /// Its position among the broker's listeners, from 0, in the order
    /// they were given
    pub listener: usize,
    /// Its address or socket path; empty for a socket without one
    pub name: String,
    pub kind: ConfigErrorKind,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.name.is_empty() {
            true => write!(f, "listener {}: {}", self.listener, self.kind),
            false => write!(
                f,
                "listener {} ({}): {}",
                self.listener, self.name, self.kind
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

/// What is wrong with a listener's settings.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ConfigErrorKind {
    #[error(
        "only a Unix socket listener can be trusted: anyone who reaches a TCP one would \
         administer the broker"
    )]
    TrustedTcp,
    #[error(
        "a trusted listener skips authentication and access control, so it takes no auth or \
         plugins"
    )]
    TrustedWithAuth,
    #[error(
        "a trusted listener can't sit behind a proxy: the kernel reports the proxy's \
         credentials, so every remote client would be trusted"
    )]
    TrustedBehindProxy,
    #[error("a trusted listener verifies no user names, so it can't use them as client ids")]
    TrustedUsernameAsClientId,
    #[error(
        "mount point `{0}` has a wildcard: its clients' topic filters would reach other mount \
         points' topics"
    )]
    MountPointWildcard(String),
    /// Names the setting: `http_dir`, `allowed_origins`, `health` or
    /// `metrics`
    #[error("{0} only applies to WebSocket listeners")]
    WebSocketOnly(&'static str),
    #[error(
        "a PROXY header can claim any address, so proxy_protocol needs proxy_from to name the \
         proxies that may send it; to let any peer send one, set proxy_from to \
         [\"0.0.0.0/0\", \"::/0\"]"
    )]
    ProxyFromRequired,
    #[error(
        "a Unix socket's clients have no address for proxy_from to match; its file permissions \
         decide who may connect"
    )]
    ProxyFromOnUnix,
    #[error("a peer filter needs a Unix socket listener")]
    PeerFilterOnTcp,
}
