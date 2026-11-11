#![doc = include_str!("../README.md")]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(not(feature = "std"))]
extern crate alloc;

// Tests use std even when the library is built without it
#[cfg(all(test, not(feature = "std")))]
#[macro_use]
extern crate std;

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
