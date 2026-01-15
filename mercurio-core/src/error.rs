use thiserror::Error;

use crate::reason::ReasonCode;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Packet is not complete")]
    PacketIncomplete,

    #[error("I/O Error: {0}")]
    Io(#[from] std::io::Error),

    #[error("MQTT Error: {0}")]
    MQTTReasonCode(#[from] ReasonCode),

    #[error("Storage error: {0}")]
    Storage(String),
}
