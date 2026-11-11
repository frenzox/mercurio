//! Error types for the Mercurio MQTT implementation.

use core::fmt;

#[cfg(not(feature = "std"))]
use alloc::string::String;

use crate::reason::ReasonCode;

/// Error type for mercurio operations.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The packet is incomplete and needs more data.
    PacketIncomplete,

    /// I/O error (only available with `std` feature).
    #[cfg(feature = "std")]
    Io(std::io::Error),

    /// The packet breaks the protocol; the reason code says how, and is the
    /// one to send the peer when its packet was the cause.
    Reason(ReasonCode),

    /// A storage backend failed, described by its message. The broker
    /// carries it through the same results as protocol errors.
    Storage(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::PacketIncomplete => write!(f, "Packet is not complete"),
            #[cfg(feature = "std")]
            Error::Io(e) => write!(f, "I/O Error: {}", e),
            Error::Reason(e) => write!(f, "MQTT Error: {}", e),
            Error::Storage(s) => write!(f, "Storage error: {}", s),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

#[cfg(feature = "std")]
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<ReasonCode> for Error {
    fn from(e: ReasonCode) -> Self {
        Error::Reason(e)
    }
}
