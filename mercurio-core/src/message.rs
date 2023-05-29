use bytes::Bytes;

use crate::qos::QoS;

#[derive(Clone, Debug)]
pub struct Message {
    pub packet_id: Option<u16>,
    pub topic: String,
    pub dup: bool,
    pub qos: QoS,
    pub retain: bool,
    pub payload: Option<Bytes>,
}
