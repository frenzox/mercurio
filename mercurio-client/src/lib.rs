#![doc = include_str!("../README.md")]
mod client;
mod connection;
mod error;
mod event;
mod options;
mod tls;

pub use client::MqttClient;
pub use error::{ClientError, Result};
pub use event::{DisconnectReason, Event, SubscribeResult, UnsubscribeResult};
pub use mercurio_packets::subscribe::{RetainHandling, SubscriptionOptions};
pub use options::{Authenticator, ConnectOptions, MessageProperties, Will};

// Re-export commonly used types from mercurio-core
pub use mercurio_core::protocol::ProtocolVersion;
pub use mercurio_core::qos::QoS;
