//! Error types for the Mercurio MQTT implementation.

use core::fmt;

#[cfg(not(feature = "std"))]
use alloc::string::String;

use crate::reason::ReasonCode;

/// Error type for mercurio operations.
#[derive(Debug)]
pub enum Error {
    /// The packet is incomplete and needs more data.
    PacketIncomplete,

    /// I/O error (only available with `std` feature).
    #[cfg(feature = "std")]
    Io(std::io::Error),

    /// MQTT protocol error with a reason code.
    MQTTReasonCode(ReasonCode),

    /// Storage backend error.
    Storage(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::PacketIncomplete => write!(f, "Packet is not complete"),
            #[cfg(feature = "std")]
            Error::Io(e) => write!(f, "I/O Error: {}", e),
            Error::MQTTReasonCode(e) => write!(f, "MQTT Error: {}", e),
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
        Error::MQTTReasonCode(e)
    }
}
