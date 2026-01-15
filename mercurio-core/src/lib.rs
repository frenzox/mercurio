pub mod codec;
pub mod error;
pub mod message;
pub mod properties;
pub mod qos;
pub mod reason;
pub mod topic;

/// A specialized `Result` type for mercurio operations
///
/// This is defined as a convenience
pub type Result<T> = std::result::Result<T, crate::error::Error>;
