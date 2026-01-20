//! Core types and traits for the Mercurio MQTT implementation.
//!
//! This crate provides the fundamental building blocks for MQTT packet encoding/decoding
//! and is designed to be `no_std` compatible when the `std` feature is disabled.
//!
//! ## Features
//!
//! - `std` (enabled by default): Enables standard library support including `std::io::Error`.
//!   When disabled, the crate is `no_std` compatible and requires the `alloc` crate.

#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(not(feature = "std"))]
extern crate alloc;

pub mod codec;
pub mod error;
pub mod message;
pub mod properties;
pub mod protocol;
pub mod qos;
pub mod reason;
pub mod topic;

/// A specialized `Result` type for mercurio operations
///
/// This is defined as a convenience
pub type Result<T> = core::result::Result<T, crate::error::Error>;
