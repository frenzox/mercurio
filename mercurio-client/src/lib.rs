#![doc = include_str!("../README.md")]
//!
//! # Features
//!
//! - `clap`: the `cli` module, with command-line arguments for connecting
//!   that tools can flatten into their own `clap` command. Off by default.
#[cfg(feature = "clap")]
pub mod cli;
mod client;
mod connection;
mod error;
mod event;
mod options;
mod tls;

pub use client::{MqttClient, PendingPublish};
pub use error::{ClientError, Result};
pub use event::{AckToken, DisconnectReason, Event, SubscribeResult, UnsubscribeResult};
pub use mercurio_packets::subscribe::{RetainHandling, SubscriptionOptions};
pub use options::{Authenticator, Backoff, ConnectOptions, MessageProperties, Will};

// Types of the public API that come from other crates, so applications
// need no direct dependency on them
pub use bytes::Bytes;
pub use mercurio_core::error::Error as PacketError;
pub use mercurio_core::protocol::ProtocolVersion;
pub use mercurio_core::qos::QoS;
pub use mercurio_core::reason::ReasonCode;
