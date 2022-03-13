#![allow(dead_code)]
mod result {
    pub type Result<T> = std::result::Result<T, crate::error::Error>;
}
mod codec;
mod control_packet;
mod error;
mod packets;
mod properties;
mod qos;
mod reason;
