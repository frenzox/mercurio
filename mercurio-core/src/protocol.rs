//! MQTT protocol version handling.

use core::fmt;

/// MQTT protocol version.
///
/// Represents the different versions of the MQTT protocol that can be
/// used in communication between clients and the broker.
#[repr(u8)]
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
#[non_exhaustive]
pub enum ProtocolVersion {
    /// MQTT 3.1 - Protocol name "MQIsdp", level 3
    V3_1 = 3,
    /// MQTT 3.1.1 - Protocol name "MQTT", level 4
    V3_1_1 = 4,
    /// MQTT 5.0 - Protocol name "MQTT", level 5
    #[default]
    V5 = 5,
}

impl ProtocolVersion {
    /// Returns the protocol name string for this version.
    pub fn protocol_name(&self) -> &'static str {
        match self {
            ProtocolVersion::V3_1 => "MQIsdp",
            ProtocolVersion::V3_1_1 | ProtocolVersion::V5 => "MQTT",
        }
    }

    /// Returns the protocol level byte for this version.
    pub fn protocol_level(&self) -> u8 {
        *self as u8
    }

    /// Returns true if this version supports properties.
    pub fn supports_properties(&self) -> bool {
        matches!(self, ProtocolVersion::V5)
    }

    /// Returns true if this version supports reason codes.
    pub fn supports_reason_codes(&self) -> bool {
        matches!(self, ProtocolVersion::V5)
    }

    /// Returns true if this version supports the AUTH packet.
    pub fn supports_auth_packet(&self) -> bool {
        matches!(self, ProtocolVersion::V5)
    }

    /// Attempts to determine the protocol version from protocol name and level.
    ///
    /// Returns `None` if the combination is invalid or unsupported.
    pub fn from_name_and_level(name: &str, level: u8) -> Option<ProtocolVersion> {
        match (name, level) {
            ("MQIsdp", 3) => Some(ProtocolVersion::V3_1),
            ("MQTT", 4) => Some(ProtocolVersion::V3_1_1),
            ("MQTT", 5) => Some(ProtocolVersion::V5),
            _ => None,
        }
    }
}

/// The version of a protocol level, or the level itself when no version
/// has it.
impl TryFrom<u8> for ProtocolVersion {
    type Error = u8;

    fn try_from(level: u8) -> Result<Self, u8> {
        match level {
            3 => Ok(ProtocolVersion::V3_1),
            4 => Ok(ProtocolVersion::V3_1_1),
            5 => Ok(ProtocolVersion::V5),
            _ => Err(level),
        }
    }
}

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtocolVersion::V3_1 => write!(f, "MQTT 3.1"),
            ProtocolVersion::V3_1_1 => write!(f, "MQTT 3.1.1"),
            ProtocolVersion::V5 => write!(f, "MQTT 5.0"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_protocol_version_from_u8() {
        assert_eq!(ProtocolVersion::try_from(3), Ok(ProtocolVersion::V3_1));
        assert_eq!(ProtocolVersion::try_from(4), Ok(ProtocolVersion::V3_1_1));
        assert_eq!(ProtocolVersion::try_from(5), Ok(ProtocolVersion::V5));
        assert_eq!(ProtocolVersion::try_from(6), Err(6));
    }

    #[test]
    fn test_protocol_version_from_name_and_level() {
        assert_eq!(
            ProtocolVersion::from_name_and_level("MQIsdp", 3),
            Some(ProtocolVersion::V3_1)
        );
        assert_eq!(
            ProtocolVersion::from_name_and_level("MQTT", 4),
            Some(ProtocolVersion::V3_1_1)
        );
        assert_eq!(
            ProtocolVersion::from_name_and_level("MQTT", 5),
            Some(ProtocolVersion::V5)
        );
        assert_eq!(ProtocolVersion::from_name_and_level("MQTT", 3), None);
        assert_eq!(ProtocolVersion::from_name_and_level("MQIsdp", 4), None);
    }

    #[test]
    fn test_protocol_name() {
        assert_eq!(ProtocolVersion::V3_1.protocol_name(), "MQIsdp");
        assert_eq!(ProtocolVersion::V3_1_1.protocol_name(), "MQTT");
        assert_eq!(ProtocolVersion::V5.protocol_name(), "MQTT");
    }

    #[test]
    fn test_supports_properties() {
        assert!(!ProtocolVersion::V3_1.supports_properties());
        assert!(!ProtocolVersion::V3_1_1.supports_properties());
        assert!(ProtocolVersion::V5.supports_properties());
    }

    #[test]
    fn test_display() {
        assert_eq!(format!("{}", ProtocolVersion::V3_1), "MQTT 3.1");
        assert_eq!(format!("{}", ProtocolVersion::V3_1_1), "MQTT 3.1.1");
        assert_eq!(format!("{}", ProtocolVersion::V5), "MQTT 5.0");
    }
}
