#![doc = include_str!("../README.md")]

/// Chainable `with_*` setters for the public fields of configuration
/// structs, which `#[non_exhaustive]` keeps code outside the crate from
/// building with a struct expression. `name => field: Some(Type)` sets an
/// optional field to the value given.
macro_rules! setters {
    ($ty:ty { $($body:tt)* }) => {
        impl $ty {
            setters!(@each $($body)*);
        }
    };
    (@each) => {};
    (@each $(#[$attr:meta])* $name:ident => $field:ident: Some($t:ty), $($rest:tt)*) => {
        $(#[$attr])*
        #[doc = concat!("Sets [`", stringify!($field), "`](Self::", stringify!($field), ").")]
        #[must_use]
        pub fn $name(mut self, value: $t) -> Self {
            self.$field = Some(value);
            self
        }
        setters!(@each $($rest)*);
    };
    (@each $(#[$attr:meta])* $name:ident => $field:ident: $t:ty, $($rest:tt)*) => {
        $(#[$attr])*
        #[doc = concat!("Sets [`", stringify!($field), "`](Self::", stringify!($field), ").")]
        #[must_use]
        pub fn $name(mut self, value: $t) -> Self {
            self.$field = value;
            self
        }
        setters!(@each $($rest)*);
    };
}

pub mod acl;
pub mod admin;
pub mod auth;
mod bans;
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
