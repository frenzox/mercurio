use std::{error, fmt};

use bytes::Buf;

use crate::codec::{Decoder, Encoder};
use crate::result::Result;

#[derive(Debug, PartialEq)]
pub enum ReasonCode {
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
    type Context = ();

    fn decode<T: Buf>(buffer: &mut T, _: Option<&Self::Context>) -> Result<Self> {
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

impl Default for ReasonCode {
    fn default() -> Self {
        ReasonCode::Success
    }
}

impl error::Error for ReasonCode {}
impl fmt::Display for ReasonCode {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use ReasonCode::*;

        match *self {
            Success => write!(f, "Succcess"),
            NormalDisconnection => write!(f, "Normal disconnection"),
            GrantedQoS0 => write!(f, "Granted QoS 0"),
            GrantedQoS1 => write!(f, "Granted QoS 1"),
            GrantedQoS2 => write!(f, "Granted QoS 2"),
            DisconnectWithWillMessage => write!(f, "Disconnect with Will Message"),
            NoMatchingSubscribers => write!(f, "No matching subscribers"),
            NoSubscriptionExisted => write!(f, "No subscriptions existed"),
            ContinueAuthentication => write!(f, "Continue authentication"),
            ReAuthenticate => write!(f, "Re-authenticate"),
            UnspecifiedError => write!(f, "Unspecified error"),
            MalformedPacket => write!(f, "Malformed packet"),
            ProtocolError => writeln!(f, "Protocol error"),
            ImplementationSpecificError => write!(f, "Implementation specific error"),
            UnsupportedProtocolVersion => write!(f, "Unsupported protocol version"),
            ClientIdentifierNotValid => write!(f, "Client identifier not valid"),
            BadUserNameOrPassword => write!(f, "Bad User Name or Password"),
            NotAuthorized => write!(f, "Not authorized"),
            ServerUnavailable => write!(f, "Server unavailable"),
            ServerBusy => write!(f, "Server busy"),
            Banned => write!(f, "Banned"),
            ServerShuttingDown => write!(f, "Server shutting down"),
            BadAuthenticationMethod => write!(f, "Bad authentication method"),
            KeepAliveTimeout => write!(f, "Keep Alive timeout"),
            SessionTakenOver => write!(f, "Session taken over"),
            TopicFilterInvalid => write!(f, "Topic filter invalid"),
            TopicNameInvalid => write!(f, "Topic name invalid"),
            PacketIdentifierInUse => write!(f, "Packet identifier in use"),
            PacketIdentifierNotFound => write!(f, "Packet identifier not found"),
            ReceiveMaximumExceeded => write!(f, "Receive maximum exceeded"),
            TopicAliasInvalid => write!(f, "Topic alias invalid"),
            PacketTooLarge => write!(f, "Packet too large"),
            MessageRateTooHigh => write!(f, "Message rate too high"),
            QuotaExceeded => write!(f, "Quota exceeded"),
            AdministrativeAction => write!(f, "Administrative action"),
            PayloadFormatInvalid => write!(f, "Payload format invalid"),
            RetainNotSupported => write!(f, "Retain not supported"),
            QoSNotSupported => write!(f, "QoS not supported"),
            UseAnotherServer => write!(f, "User another server"),
            ServerMoved => write!(f, "Server moved"),
            SharedSubscriptionsNotSupported => {
                write!(f, "Shared subscriptions not supported")
            }
            ConnectionRateExceeded => write!(f, "Connection rate exceeded"),
            MaximumConnectTime => write!(f, "Maximum connect time"),
            SubscriptionIdentifiersNotSupported => {
                write!(f, "Subscription indentifiers not supported")
            }
            WildcardSubscriptionsNotSupported => {
                write!(f, "Wildcard subscriptions not supported")
            }
        }
    }
}
