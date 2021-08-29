#[repr(u8)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum QoS {
    AtMostOnce = 0,
    AtLeastOnce = 1,
    ExactlyOnce = 2,
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

impl Default for QoS {
    fn default() -> Self {
        QoS::AtMostOnce
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
