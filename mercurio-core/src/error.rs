//! Error types for the Mercurio MQTT implementation.

use core::fmt;

use crate::{
    protocol::{PacketType, ProtocolVersion},
    reason::ReasonCode,
};

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

    /// A value couldn't be encoded.
    Encode(EncodeError),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::PacketIncomplete => write!(f, "Packet is not complete"),
            #[cfg(feature = "std")]
            Error::Io(e) => write!(f, "I/O Error: {}", e),
            Error::Reason(e) => write!(f, "MQTT Error: {}", e),
            Error::Encode(e) => write!(f, "Encoding error: {}", e),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            Error::Encode(e) => Some(e),
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

impl From<EncodeError> for Error {
    fn from(e: EncodeError) -> Self {
        Error::Encode(e)
    }
}

/// Why a value can't be encoded: MQTT can't carry it, or the receiver
/// would have to refuse it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EncodeError {
    /// A string or binary value longer than 65,535 bytes.
    TooLong,
    /// A Variable Byte Integer above 268,435,455, such as the Remaining
    /// Length of a packet too large for MQTT to frame.
    TooLarge,
    /// A string holding U+0000 [MQTT-1.5.4-2].
    NulInString,
    /// A Packet Identifier of 0 [MQTT-2.2.1-3].
    PacketIdZero,
    /// A QoS 1 or 2 PUBLISH without a Packet Identifier, or a QoS 0 one
    /// with one [MQTT-2.2.1-2].
    PacketIdMismatch,
    /// A QoS 0 PUBLISH with the DUP flag set [MQTT-3.3.1-2].
    DupAtQoS0,
    /// A reason code the packet type does not list.
    InvalidReasonCode(PacketType, ReasonCode),
    /// A SUBSCRIBE, UNSUBSCRIBE, SUBACK or MQTT 5 UNSUBACK without entries.
    EmptyPayload(PacketType),
    /// A property whose value the spec rules out, by its identifier: a
    /// Topic Alias of 0, a Response Topic with wildcards, Authentication
    /// Data without an Authentication Method, and the like.
    InvalidProperty(u32),
    /// An MQTT 3.x CONNECT with a password but no user name
    /// [MQTT-3.1.2-22].
    PasswordWithoutUserName,
    /// A packet the protocol version doesn't have, such as AUTH in MQTT
    /// 3.x, or a CONNECT for another version than it names.
    UnsupportedVersion(PacketType, ProtocolVersion),
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EncodeError::TooLong => write!(f, "string or binary data over 65,535 bytes"),
            EncodeError::TooLarge => write!(f, "value over 268,435,455, too large to encode"),
            EncodeError::NulInString => write!(f, "string holding U+0000"),
            EncodeError::PacketIdZero => write!(f, "packet identifier 0"),
            EncodeError::PacketIdMismatch => {
                write!(f, "packet identifier does not match the QoS")
            }
            EncodeError::DupAtQoS0 => write!(f, "DUP flag on a QoS 0 PUBLISH"),
            EncodeError::InvalidReasonCode(packet_type, reason) => {
                write!(f, "reason code {reason} is not valid in {packet_type:?}")
            }
            EncodeError::EmptyPayload(packet_type) => write!(f, "{packet_type:?} without entries"),
            EncodeError::InvalidProperty(id) => write!(f, "invalid value for property {id:#04x}"),
            EncodeError::PasswordWithoutUserName => {
                write!(f, "password without a user name in MQTT 3.x")
            }
            EncodeError::UnsupportedVersion(packet_type, version) => {
                write!(f, "{packet_type:?} can't be encoded for {version}")
            }
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for EncodeError {}
