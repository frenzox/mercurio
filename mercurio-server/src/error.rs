//! Server error types.

use thiserror::Error;

/// Server-specific errors.
#[derive(Debug, Error)]
pub enum ServerError {
    /// TLS configuration or connection error.
    #[error("TLS error: {0}")]
    Tls(String),

    /// I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Configuration error.
    #[error("Configuration error: {0}")]
    Config(String),
}
