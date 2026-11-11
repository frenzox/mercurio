//! Quality of service levels: how hard a message's delivery is tried.

/// Delivery guarantee of an application message (section 4.3).
///
/// The discriminant is the two-bit value used on the wire, so `qos as u8`
/// gives the encoded level, and [`TryFrom<u8>`] reads it back.
#[repr(u8)]
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum QoS {
    /// QoS 0: sent once, with no acknowledgement. The message may be lost.
    #[default]
    AtMostOnce = 0,
    /// QoS 1: resent until a PUBACK arrives. The message may be delivered
    /// more than once.
    AtLeastOnce = 1,
    /// QoS 2: a PUBREC, PUBREL, PUBCOMP exchange makes sure the message is
    /// delivered once.
    ExactlyOnce = 2,
}

/// The QoS of a wire value, or the value itself when it is not 0, 1 or 2.
/// The value 3 is reserved and is a malformed packet in a PUBLISH
/// [MQTT-3.3.1-4].
impl TryFrom<u8> for QoS {
    type Error = u8;

    fn try_from(n: u8) -> Result<Self, u8> {
        match n {
            0x00 => Ok(QoS::AtMostOnce),
            0x01 => Ok(QoS::AtLeastOnce),
            0x02 => Ok(QoS::ExactlyOnce),
            _ => Err(n),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::qos::QoS;

    #[test]
    fn test_qos_try_from_u8() {
        assert_eq!(QoS::try_from(0), Ok(QoS::AtMostOnce));
        assert_eq!(QoS::try_from(1), Ok(QoS::AtLeastOnce));
        assert_eq!(QoS::try_from(2), Ok(QoS::ExactlyOnce));
        assert_eq!(QoS::try_from(3), Err(3));
        assert_eq!(QoS::try_from(0xff), Err(0xff));
    }
}
