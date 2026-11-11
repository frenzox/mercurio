//! MQTT reason codes as defined in the MQTT specification.

use bytes::Buf;
use core::fmt;

use crate::{
    codec::{Decoder, Encoder},
    protocol::PacketType,
};

/// MQTT reason code used in various packets to indicate success or failure.
///
/// One enum covers the reason codes of every packet type (section 2.4).
/// Codes below 0x80 report success and codes from 0x80 report failure.
/// Each variant is one byte on the wire, and the discriminant is that byte.
/// The spec gives 0x00 a different name in some packets; those names are
/// constants for the same [`Success`](ReasonCode::Success) variant, so
/// codes compare the way the wire does:
///
/// ```
/// use mercurio_core::reason::ReasonCode;
///
/// assert_eq!(ReasonCode::NormalDisconnection, ReasonCode::Success);
/// assert_eq!(ReasonCode::GrantedQoS0, ReasonCode::Success);
/// assert_eq!(u8::from(ReasonCode::PacketTooLarge), 0x95);
/// assert_eq!(ReasonCode::try_from(0x95), Ok(ReasonCode::PacketTooLarge));
/// ```
///
/// An unknown code is a [`MalformedPacket`](ReasonCode::MalformedPacket) to
/// the [`Decoder`], and the packet decoders also refuse a known code their
/// packet does not list; see [`is_valid_for`](ReasonCode::is_valid_for).
///
/// It is also the error type of protocol failures, through
/// [`Error::Reason`](crate::error::Error::Reason).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum ReasonCode {
    /// 0x00 in every packet with a reason code. DISCONNECT names it
    /// [`NormalDisconnection`](ReasonCode::NormalDisconnection) and SUBACK
    /// [`GrantedQoS0`](ReasonCode::GrantedQoS0).
    #[default]
    Success = 0x00,
    /// 0x01 in SUBACK.
    GrantedQoS1 = 0x01,
    /// 0x02 in SUBACK.
    GrantedQoS2 = 0x02,
    /// 0x04 in DISCONNECT: the client closes the connection and asks the
    /// server to publish its will message.
    DisconnectWithWillMessage = 0x04,
    NoMatchingSubscribers = 0x10,
    NoSubscriptionExisted = 0x11,
    ContinueAuthentication = 0x18,
    ReAuthenticate = 0x19,
    UnspecifiedError = 0x80,
    MalformedPacket = 0x81,
    ProtocolError = 0x82,
    ImplementationSpecificError = 0x83,
    UnsupportedProtocolVersion = 0x84,
    ClientIdentifierNotValid = 0x85,
    BadUserNameOrPassword = 0x86,
    NotAuthorized = 0x87,
    ServerUnavailable = 0x88,
    ServerBusy = 0x89,
    Banned = 0x8a,
    ServerShuttingDown = 0x8b,
    BadAuthenticationMethod = 0x8c,
    KeepAliveTimeout = 0x8d,
    SessionTakenOver = 0x8e,
    TopicFilterInvalid = 0x8f,
    TopicNameInvalid = 0x90,
    PacketIdentifierInUse = 0x91,
    PacketIdentifierNotFound = 0x92,
    ReceiveMaximumExceeded = 0x93,
    TopicAliasInvalid = 0x94,
    PacketTooLarge = 0x95,
    MessageRateTooHigh = 0x96,
    QuotaExceeded = 0x97,
    AdministrativeAction = 0x98,
    PayloadFormatInvalid = 0x99,
    RetainNotSupported = 0x9a,
    QoSNotSupported = 0x9b,
    UseAnotherServer = 0x9c,
    ServerMoved = 0x9d,
    SharedSubscriptionsNotSupported = 0x9e,
    ConnectionRateExceeded = 0x9f,
    MaximumConnectTime = 0xa0,
    SubscriptionIdentifiersNotSupported = 0xa1,
    WildcardSubscriptionsNotSupported = 0xa2,
}

impl fmt::Display for ReasonCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use ReasonCode::*;
        let msg = match self {
            Success => "Success",
            GrantedQoS1 => "Granted QoS 1",
            GrantedQoS2 => "Granted QoS 2",
            DisconnectWithWillMessage => "Disconnect with will message",
            NoMatchingSubscribers => "No matching subscribers",
            NoSubscriptionExisted => "No subscription existed",
            ContinueAuthentication => "Continue authentication",
            ReAuthenticate => "Re-authenticate",
            UnspecifiedError => "Unspecified error",
            MalformedPacket => "Malformed packet",
            ProtocolError => "Protocol error",
            ImplementationSpecificError => "Implementation specific error",
            UnsupportedProtocolVersion => "Unsupported protocol version",
            ClientIdentifierNotValid => "Client identifier not valid",
            BadUserNameOrPassword => "Bad user name or password",
            NotAuthorized => "Not authorized",
            ServerUnavailable => "Server unavailable",
            ServerBusy => "Server busy",
            Banned => "Banned",
            ServerShuttingDown => "Server shutting down",
            BadAuthenticationMethod => "Bad authentication method",
            KeepAliveTimeout => "Keep alive timeout",
            SessionTakenOver => "Session taken over",
            TopicFilterInvalid => "Topic filter invalid",
            TopicNameInvalid => "Topic name invalid",
            PacketIdentifierInUse => "Packet identifier in use",
            PacketIdentifierNotFound => "Packet identifier not found",
            ReceiveMaximumExceeded => "Receive maximum exceeded",
            TopicAliasInvalid => "Topic alias invalid",
            PacketTooLarge => "Packet too large",
            MessageRateTooHigh => "Message rate too high",
            QuotaExceeded => "Quota exceeded",
            AdministrativeAction => "Administrative action",
            PayloadFormatInvalid => "Payload format invalid",
            RetainNotSupported => "Retain not supported",
            QoSNotSupported => "QoS not supported",
            UseAnotherServer => "Use another server",
            ServerMoved => "Server moved",
            SharedSubscriptionsNotSupported => "Shared subscriptions not supported",
            ConnectionRateExceeded => "Connection rate exceeded",
            MaximumConnectTime => "Maximum connect time",
            SubscriptionIdentifiersNotSupported => "Subscription identifiers not supported",
            WildcardSubscriptionsNotSupported => "Wildcard subscriptions not supported",
        };
        write!(f, "{}", msg)
    }
}

#[cfg(feature = "std")]
impl std::error::Error for ReasonCode {}

impl ReasonCode {
    /// 0x00 in DISCONNECT: the connection closes without publishing the
    /// will message.
    #[allow(non_upper_case_globals)]
    pub const NormalDisconnection: ReasonCode = ReasonCode::Success;

    /// 0x00 in SUBACK: the subscription was granted at QoS 0.
    #[allow(non_upper_case_globals)]
    pub const GrantedQoS0: ReasonCode = ReasonCode::Success;

    /// The byte that encodes this reason code.
    pub const fn code(self) -> u8 {
        self as u8
    }

    /// Whether the code reports success: any code below 0x80.
    pub const fn is_success(self) -> bool {
        self.code() < 0x80
    }

    /// Whether the code reports a failure: 0x80 and above.
    pub const fn is_error(self) -> bool {
        !self.is_success()
    }

    /// Whether an MQTT 5 packet of `packet_type` may carry this reason code,
    /// following the tables in sections 3.2.2.2 (CONNACK), 3.4.2.1 (PUBACK),
    /// 3.5.2.1 (PUBREC), 3.6.2.1 (PUBREL), 3.7.2.1 (PUBCOMP), 3.9.3
    /// (SUBACK), 3.11.3 (UNSUBACK), 3.14.2.1 (DISCONNECT) and 3.15.2.1
    /// (AUTH). Packets without a reason code allow none.
    ///
    /// ```
    /// use mercurio_core::{protocol::PacketType, reason::ReasonCode};
    ///
    /// assert!(ReasonCode::PacketIdentifierNotFound.is_valid_for(PacketType::PubRel));
    /// assert!(!ReasonCode::PacketIdentifierNotFound.is_valid_for(PacketType::PubAck));
    /// ```
    pub fn is_valid_for(self, packet_type: PacketType) -> bool {
        let code = self.code();
        match packet_type {
            PacketType::ConnAck => matches!(
                code,
                0x00 | 0x80..=0x8a | 0x8c | 0x90 | 0x95 | 0x97 | 0x99..=0x9d | 0x9f
            ),
            PacketType::PubAck | PacketType::PubRec => matches!(
                code,
                0x00 | 0x10 | 0x80 | 0x83 | 0x87 | 0x90 | 0x91 | 0x97 | 0x99
            ),
            PacketType::PubRel | PacketType::PubComp => matches!(code, 0x00 | 0x92),
            PacketType::SubAck => matches!(
                code,
                0x00..=0x02 | 0x80 | 0x83 | 0x87 | 0x8f | 0x91 | 0x97 | 0x9e | 0xa1 | 0xa2
            ),
            PacketType::UnsubAck => {
                matches!(code, 0x00 | 0x11 | 0x80 | 0x83 | 0x87 | 0x8f | 0x91)
            }
            PacketType::Disconnect => matches!(
                code,
                0x00 | 0x04 | 0x80..=0x83 | 0x87 | 0x89 | 0x8b | 0x8d..=0x90 | 0x93..=0xa2
            ),
            PacketType::Auth => matches!(code, 0x00 | 0x18 | 0x19),
            PacketType::Connect
            | PacketType::Publish
            | PacketType::Subscribe
            | PacketType::Unsubscribe
            | PacketType::PingReq
            | PacketType::PingResp => false,
        }
    }
}

impl From<ReasonCode> for u8 {
    fn from(reason: ReasonCode) -> u8 {
        reason.code()
    }
}

/// The reason code a byte encodes, or the byte itself when no packet
/// type lists it.
impl TryFrom<u8> for ReasonCode {
    type Error = u8;

    fn try_from(code: u8) -> Result<Self, u8> {
        use ReasonCode::*;

        let reason = match code {
            0x00 => Success,
            0x01 => GrantedQoS1,
            0x02 => GrantedQoS2,
            0x04 => DisconnectWithWillMessage,
            0x10 => NoMatchingSubscribers,
            0x11 => NoSubscriptionExisted,
            0x18 => ContinueAuthentication,
            0x19 => ReAuthenticate,
            0x80 => UnspecifiedError,
            0x81 => MalformedPacket,
            0x82 => ProtocolError,
            0x83 => ImplementationSpecificError,
            0x84 => UnsupportedProtocolVersion,
            0x85 => ClientIdentifierNotValid,
            0x86 => BadUserNameOrPassword,
            0x87 => NotAuthorized,
            0x88 => ServerUnavailable,
            0x89 => ServerBusy,
            0x8a => Banned,
            0x8b => ServerShuttingDown,
            0x8c => BadAuthenticationMethod,
            0x8d => KeepAliveTimeout,
            0x8e => SessionTakenOver,
            0x8f => TopicFilterInvalid,
            0x90 => TopicNameInvalid,
            0x91 => PacketIdentifierInUse,
            0x92 => PacketIdentifierNotFound,
            0x93 => ReceiveMaximumExceeded,
            0x94 => TopicAliasInvalid,
            0x95 => PacketTooLarge,
            0x96 => MessageRateTooHigh,
            0x97 => QuotaExceeded,
            0x98 => AdministrativeAction,
            0x99 => PayloadFormatInvalid,
            0x9a => RetainNotSupported,
            0x9b => QoSNotSupported,
            0x9c => UseAnotherServer,
            0x9d => ServerMoved,
            0x9e => SharedSubscriptionsNotSupported,
            0x9f => ConnectionRateExceeded,
            0xa0 => MaximumConnectTime,
            0xa1 => SubscriptionIdentifiersNotSupported,
            0xa2 => WildcardSubscriptionsNotSupported,
            _ => return Err(code),
        };

        Ok(reason)
    }
}

impl Encoder for ReasonCode {
    fn encode(&self, buffer: &mut bytes::BytesMut) {
        self.code().encode(buffer);
    }

    fn encoded_size(&self) -> usize {
        1
    }
}

impl Decoder for ReasonCode {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        ReasonCode::try_from(u8::decode(buffer)?).map_err(|_| ReasonCode::MalformedPacket.into())
    }
}

#[cfg(test)]
mod tests {
    use std::vec::Vec;

    use super::*;

    /// Every code the decoder knows, as the bytes 0x00 to 0xff decode.
    fn known_codes() -> Vec<ReasonCode> {
        (0..=u8::MAX)
            .filter_map(|byte| ReasonCode::decode(&mut &[byte][..]).ok())
            .collect()
    }

    #[test]
    fn test_reason_codes_per_packet_match_the_spec_tables() {
        let expected = [
            (PacketType::ConnAck, 22),
            (PacketType::PubAck, 9),
            (PacketType::PubRec, 9),
            (PacketType::PubRel, 2),
            (PacketType::PubComp, 2),
            (PacketType::SubAck, 12),
            (PacketType::UnsubAck, 7),
            (PacketType::Disconnect, 29),
            (PacketType::Auth, 3),
            (PacketType::Publish, 0),
            (PacketType::Connect, 0),
        ];
        for (packet_type, count) in expected {
            let valid = known_codes()
                .into_iter()
                .filter(|code| code.is_valid_for(packet_type))
                .count();
            assert_eq!(valid, count, "{packet_type:?}");
        }
    }

    #[test]
    fn test_zero_has_one_value_under_each_of_its_names() {
        let decoded = ReasonCode::decode(&mut &[0x00][..]).unwrap();
        assert_eq!(decoded, ReasonCode::NormalDisconnection);
        assert_eq!(decoded, ReasonCode::GrantedQoS0);
        assert!(matches!(decoded, ReasonCode::NormalDisconnection));
    }

    #[test]
    fn test_every_byte_converts_back_to_itself() {
        for byte in 0..=u8::MAX {
            if let Ok(reason) = ReasonCode::try_from(byte) {
                assert_eq!(u8::from(reason), byte);
            }
        }
        assert_eq!(known_codes().len(), 43);
        assert_eq!(ReasonCode::try_from(0x03), Err(0x03));
    }

    #[test]
    fn test_success_and_error_split_at_0x80() {
        assert!(ReasonCode::NoMatchingSubscribers.is_success());
        assert!(!ReasonCode::NoMatchingSubscribers.is_error());
        assert!(ReasonCode::UnspecifiedError.is_error());
        assert!(!ReasonCode::UnspecifiedError.is_success());
    }
}
