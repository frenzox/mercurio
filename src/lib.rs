#![allow(dead_code)]
mod codec;
mod control_packet;
mod error;
mod packets;
mod properties;
mod qos;
mod reason;
/// A specialized `Result` type for mercurio operations
///
/// This is defined as a convenience
pub type Result<T> = std::result::Result<T, crate::error::Error>;
