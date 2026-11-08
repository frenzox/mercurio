//! Quality of service levels: how hard a message's delivery is tried.

/// Delivery guarantee of an application message (section 4.3).
///
/// The discriminant is the two-bit value used on the wire, so `qos as u8`
/// gives the encoded level.
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
    /// Any wire value other than 0, 1 or 2, as produced by `From<u8>`.
    ///
    /// The value 3 is reserved and is a malformed packet in a PUBLISH
    /// [MQTT-3.3.1-4]. Code that reads a QoS field from the wire (PUBLISH,
    /// SUBSCRIBE options, the CONNECT Will QoS) must check for this variant
    /// and reject the packet.
    Invalid = 0xff,
}

impl From<u8> for QoS {
    fn from(n: u8) -> Self {
        match n {
            0x00 => QoS::AtMostOnce,
            0x01 => QoS::AtLeastOnce,
            0x02 => QoS::ExactlyOnce,
            _ => QoS::Invalid,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::qos::QoS;

    #[test]
    fn test_qos_from_u8() {
        let at_most_once = 0x0u8;
        let mut result: QoS = at_most_once.into();
        assert_eq!(QoS::AtMostOnce, result);

        let at_least_once = 0x01u8;
        result = at_least_once.into();
        assert_eq!(QoS::AtLeastOnce, result);

        let exactly_once = 0x02u8;
        result = exactly_once.into();
        assert_eq!(QoS::ExactlyOnce, result);

        let invalid = 0x03u8;
        result = invalid.into();
        assert_eq!(QoS::Invalid, result);
    }
}
