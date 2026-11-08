//! Server error types.

use thiserror::Error;

/// Server-specific errors.
#[derive(Debug, Error)]
#[non_exhaustive]
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

    /// The broker's state couldn't be read from storage.
    #[error("Storage error: {0}")]
    Storage(#[from] mercurio_storage::StorageError),
}
