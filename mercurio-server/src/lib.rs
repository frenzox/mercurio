#![doc = include_str!("../README.md")]
pub mod acl;
pub mod admin;
pub mod auth;
mod broker;
mod connection;
pub mod error;
mod http;
pub mod plugin;
mod proxy;
mod queue;
pub mod server;
mod session;
mod session_manager;
mod shutdown;
mod sys;
pub mod tls;
mod topic_tree;
mod websocket;
mod writer;

/// Tracing target of the messages logged when clients connect and
/// disconnect, so they can be filtered out on busy brokers.
pub const CONNECTIONS: &str = "mercurio::connections";
