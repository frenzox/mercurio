use bytes::Buf;
use thiserror::Error;

use crate::codec::{Decoder, Encoder};

#[derive(Error, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReasonCode {
    #[default]
    #[error("Success")]
    Success,

    #[error("NormalDisconnection")]
    NormalDisconnection,

    #[error("Granted QoS 0")]
    GrantedQoS0,

    #[error("Granted QoS 1")]
    GrantedQoS1,

    #[error("Granted QoS 2")]
    GrantedQoS2,

    #[error("Disconnect with Will Message")]
    DisconnectWithWillMessage,

    #[error("No matching subscribers")]
    NoMatchingSubscribers,

    #[error("No subscription existed")]
    NoSubscriptionExisted,

    #[error("Continue authentication")]
    ContinueAuthentication,

    #[error("Re-authenticate")]
    ReAuthenticate,

    #[error("Unspecified error")]
    UnspecifiedError,

    #[error("Malformed packet")]
    MalformedPacket,

    #[error("Protocol error")]
    ProtocolError,

    #[error("Implementation specific error")]
    ImplementationSpecificError,

    #[error("Unsupported protocol version")]
    UnsupportedProtocolVersion,

    #[error("Client identifier not valid")]
    ClientIdentifierNotValid,

    #[error("Bad User Name or Password")]
    BadUserNameOrPassword,

    #[error("Not authorized")]
    NotAuthorized,

    #[error("Server unavailable")]
    ServerUnavailable,

    #[error("Server busy")]
    ServerBusy,

    #[error("Banned")]
    Banned,

    #[error("Server shutting down")]
    ServerShuttingDown,

    #[error("Bad authentication method")]
    BadAuthenticationMethod,

    #[error("Keep Alive timeout")]
    KeepAliveTimeout,

    #[error("Session taken over")]
    SessionTakenOver,

    #[error("Topic filter invalid")]
    TopicFilterInvalid,

    #[error("Topic name invalid")]
    TopicNameInvalid,

    #[error("Packet identifier in use")]
    PacketIdentifierInUse,

    #[error("Packet identifier not found")]
    PacketIdentifierNotFound,

    #[error("Receive maximum exceeded")]
    ReceiveMaximumExceeded,

    #[error("Topic alias invalid")]
    TopicAliasInvalid,

    #[error("Packet too large")]
    PacketTooLarge,

    #[error("Message rate too high")]
    MessageRateTooHigh,

    #[error("Quota exceeded")]
    QuotaExceeded,

    #[error("Administrative action")]
    AdministrativeAction,

    #[error("Payload format invalid")]
    PayloadFormatInvalid,

    #[error("Retain not supported")]
    RetainNotSupported,

    #[error("QoS not supported")]
    QoSNotSupported,

    #[error("Use another server")]
    UseAnotherServer,

    #[error("Server moved")]
    ServerMoved,

    #[error("Shared subscriptions not supported")]
    SharedSubscriptionsNotSupported,

    #[error("Connection rate exceeded")]
    ConnectionRateExceeded,

    #[error("Maximum connect time")]
    MaximumConnectTime,

    #[error("Subscription indentifiers not supported")]
    SubscriptionIdentifiersNotSupported,

    #[error("Wildcard subscriptions not supported")]
    WildcardSubscriptionsNotSupported,
}

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
