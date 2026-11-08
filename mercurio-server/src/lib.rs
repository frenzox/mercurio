pub mod acl;
pub mod auth;
mod broker;
pub mod connection;
pub mod error;
mod http;
mod proxy;
pub mod queue;
pub mod server;
mod session;
pub mod session_manager;
mod shutdown;
mod sys;
pub mod tls;
mod topic_tree;
mod websocket;
mod writer;

/// Tracing target of the messages logged when clients connect and
/// disconnect, so they can be filtered out on busy brokers.
pub const CONNECTIONS: &str = "mercurio::connections";
