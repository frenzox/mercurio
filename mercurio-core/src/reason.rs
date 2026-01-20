//! MQTT reason codes as defined in the MQTT specification.

use bytes::Buf;
use core::fmt;

use crate::codec::{Decoder, Encoder};

/// MQTT reason code used in various packets to indicate success or failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReasonCode {
    #[default]
    Success,
    NormalDisconnection,
    GrantedQoS0,
    GrantedQoS1,
    GrantedQoS2,
    DisconnectWithWillMessage,
    NoMatchingSubscribers,
    NoSubscriptionExisted,
    ContinueAuthentication,
    ReAuthenticate,
    UnspecifiedError,
    MalformedPacket,
    ProtocolError,
    ImplementationSpecificError,
    UnsupportedProtocolVersion,
    ClientIdentifierNotValid,
    BadUserNameOrPassword,
    NotAuthorized,
    ServerUnavailable,
    ServerBusy,
    Banned,
    ServerShuttingDown,
    BadAuthenticationMethod,
    KeepAliveTimeout,
    SessionTakenOver,
    TopicFilterInvalid,
    TopicNameInvalid,
    PacketIdentifierInUse,
    PacketIdentifierNotFound,
    ReceiveMaximumExceeded,
    TopicAliasInvalid,
    PacketTooLarge,
    MessageRateTooHigh,
    QuotaExceeded,
    AdministrativeAction,
    PayloadFormatInvalid,
    RetainNotSupported,
    QoSNotSupported,
    UseAnotherServer,
    ServerMoved,
    SharedSubscriptionsNotSupported,
    ConnectionRateExceeded,
    MaximumConnectTime,
    SubscriptionIdentifiersNotSupported,
    WildcardSubscriptionsNotSupported,
}

impl fmt::Display for ReasonCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use ReasonCode::*;
        let msg = match self {
            Success => "Success",
            NormalDisconnection => "Normal disconnection",
            GrantedQoS0 => "Granted QoS 0",
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
    pub fn get_code(&self) -> u8 {
        use ReasonCode::*;

        match *self {
            Success => 0x00,
            NormalDisconnection => 0x00,
            GrantedQoS0 => 0x00,
            GrantedQoS1 => 0x01,
            GrantedQoS2 => 0x02,
            DisconnectWithWillMessage => 0x04,
            NoMatchingSubscribers => 0x10,
            NoSubscriptionExisted => 0x11,
            ContinueAuthentication => 0x18,
            ReAuthenticate => 0x19,
            UnspecifiedError => 0x80,
            MalformedPacket => 0x81,
            ProtocolError => 0x82,
            ImplementationSpecificError => 0x83,
            UnsupportedProtocolVersion => 0x84,
            ClientIdentifierNotValid => 0x85,
            BadUserNameOrPassword => 0x86,
            NotAuthorized => 0x87,
            ServerUnavailable => 0x88,
            ServerBusy => 0x89,
            Banned => 0x8a,
            ServerShuttingDown => 0x8b,
            BadAuthenticationMethod => 0x8c,
            KeepAliveTimeout => 0x8d,
            SessionTakenOver => 0x8e,
            TopicFilterInvalid => 0x8f,
            TopicNameInvalid => 0x90,
            PacketIdentifierInUse => 0x91,
            PacketIdentifierNotFound => 0x92,
            ReceiveMaximumExceeded => 0x93,
            TopicAliasInvalid => 0x94,
            PacketTooLarge => 0x95,
            MessageRateTooHigh => 0x96,
            QuotaExceeded => 0x97,
            AdministrativeAction => 0x98,
            PayloadFormatInvalid => 0x99,
            RetainNotSupported => 0x9a,
            QoSNotSupported => 0x9b,
            UseAnotherServer => 0x9c,
            ServerMoved => 0x9d,
            SharedSubscriptionsNotSupported => 0x9e,
            ConnectionRateExceeded => 0x9f,
            MaximumConnectTime => 0xa0,
            SubscriptionIdentifiersNotSupported => 0xa1,
            WildcardSubscriptionsNotSupported => 0xa2,
        }
    }
}

impl Encoder for ReasonCode {
    fn encode(&self, buffer: &mut bytes::BytesMut) {
        self.get_code().encode(buffer);
    }

    fn encoded_size(&self) -> usize {
        self.get_code().encoded_size()
    }
}

impl Decoder for ReasonCode {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        use ReasonCode::*;

        let reason = match buffer.get_u8() {
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
            _ => return Err(MalformedPacket.into()),
        };

        Ok(reason)
    }
}
