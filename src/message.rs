use bytes::Bytes;

use crate::qos::QoS;

#[derive(Clone, Debug)]
pub struct Message {
    pub(crate) topic: String,
    pub(crate) dup: bool,
    pub(crate) qos: QoS,
    pub(crate) retain: bool,
    pub(crate) payload: Option<Bytes>,
}
