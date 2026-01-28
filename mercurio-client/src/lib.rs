//! Mercurio MQTT Client Library
//!
//! This crate provides an asynchronous MQTT client for connecting to MQTT brokers.
//!
//! # Example
//!
//! ```no_run
//! use mercurio_client::{MqttClient, ConnectOptions};
//! use mercurio_core::qos::QoS;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Connect to broker
//!     let options = ConnectOptions::new("localhost", 1883)
//!         .client_id("my-client")
//!         .clean_start(true);
//!
//!     let client = MqttClient::connect(options).await?;
//!
//!     // Subscribe to a topic
//!     client.subscribe(&[("test/topic", QoS::AtLeastOnce)]).await?;
//!
//!     // Publish a message
//!     client.publish("test/topic", "Hello, MQTT!", QoS::AtLeastOnce).await?;
//!
//!     // Receive messages
//!     while let Some(event) = client.recv().await {
//!         println!("Received event: {:?}", event);
//!     }
//!
//!     Ok(())
//! }
//! ```

mod client;
mod connection;
mod error;
mod event;
mod options;
mod tls;

pub use client::MqttClient;
pub use error::{ClientError, Result};
pub use event::{DisconnectReason, Event, SubscribeResult};
pub use options::{ConnectOptions, Will};

// Re-export commonly used types from mercurio-core
pub use mercurio_core::protocol::ProtocolVersion;
pub use mercurio_core::qos::QoS;
