#![allow(dead_code)]
mod broker;
mod codec;
pub mod connection;
mod control_packet;
mod error;
mod message;
mod packets;
mod properties;
mod qos;
mod reason;
pub mod server;
mod session;
pub mod session_manager;
mod shutdown;
mod topic_tree;

/// A specialized `Result` type for mercurio operations
///
/// This is defined as a convenience
pub type Result<T> = std::result::Result<T, crate::error::Error>;
