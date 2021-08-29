use bytes::Buf;
use std::{error, fmt};

use crate::{
    control_packet::ControlPacketType,
    endec::{DecoderWithContext, Encoder},
};

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
        match *self {
            ReasonCode::Success => 0x00,
            ReasonCode::NormalDisconnection => 0x00,
            ReasonCode::GrantedQoS0 => 0x00,
            ReasonCode::GrantedQoS1 => 0x01,
            ReasonCode::GrantedQoS2 => 0x02,
            ReasonCode::DisconnectWithWillMessage => 0x04,
            ReasonCode::NoMatchingSubscribers => 0x10,
            ReasonCode::NoSubscriptionExisted => 0x11,
            ReasonCode::ContinueAuthentication => 0x18,
            ReasonCode::ReAuthenticate => 0x19,
            ReasonCode::UnspecifiedError => 0x80,
            ReasonCode::MalformedPacket => 0x81,
            ReasonCode::ProtocolError => 0x82,
            ReasonCode::ImplementationSpecificError => 0x83,
            ReasonCode::UnsupportedProtocolVersion => 0x84,
            ReasonCode::ClientIdentifierNotValid => 0x85,
            ReasonCode::BadUserNameOrPassword => 0x86,
            ReasonCode::NotAuthorized => 0x87,
            ReasonCode::ServerUnavailable => 0x88,
            ReasonCode::ServerBusy => 0x89,
            ReasonCode::Banned => 0x8a,
            ReasonCode::ServerShuttingDown => 0x8b,
            ReasonCode::BadAuthenticationMethod => 0x8c,
            ReasonCode::KeepAliveTimeout => 0x8d,
            ReasonCode::SessionTakenOver => 0x8e,
            ReasonCode::TopicFilterInvalid => 0x8f,
            ReasonCode::TopicNameInvalid => 0x90,
            ReasonCode::PacketIdentifierInUse => 0x91,
            ReasonCode::PacketIdentifierNotFound => 0x92,
            ReasonCode::ReceiveMaximumExceeded => 0x93,
            ReasonCode::TopicAliasInvalid => 0x94,
            ReasonCode::PacketTooLarge => 0x95,
            ReasonCode::MessageRateTooHigh => 0x96,
            ReasonCode::QuotaExceeded => 0x97,
            ReasonCode::AdministrativeAction => 0x98,
            ReasonCode::PayloadFormatInvalid => 0x99,
            ReasonCode::RetainNotSupported => 0x9a,
            ReasonCode::QoSNotSupported => 0x9b,
            ReasonCode::UseAnotherServer => 0x9c,
            ReasonCode::ServerMoved => 0x9d,
            ReasonCode::SharedSubscriptionsNotSupported => 0x9e,
            ReasonCode::ConnectionRateExceeded => 0x9f,
            ReasonCode::MaximumConnectTime => 0xa0,
            ReasonCode::SubscriptionIdentifiersNotSupported => 0xa1,
            ReasonCode::WildcardSubscriptionsNotSupported => 0xa2,
        }
    }
}

impl Encoder for ReasonCode {
    fn encode(&self, buffer: &mut bytes::BytesMut) {
        self.get_code().encode(buffer);
    }

    fn get_encoded_size(&self) -> usize {
        self.get_code().get_encoded_size()
    }
}

impl DecoderWithContext<ControlPacketType> for ReasonCode {
    fn decode<T: Buf>(
        buffer: &mut T,
        context: &ControlPacketType,
    ) -> Result<Option<Self>, ReasonCode> {
        let reason = match buffer.get_u8() {
            0x00 => match context {
                ControlPacketType::SubAck => ReasonCode::GrantedQoS0,
                ControlPacketType::Disconnect => ReasonCode::NormalDisconnection,
                _ => ReasonCode::Success,
            },
            0x01 => ReasonCode::GrantedQoS1,
            0x02 => ReasonCode::GrantedQoS2,
            0x04 => ReasonCode::DisconnectWithWillMessage,
            0x10 => ReasonCode::NoMatchingSubscribers,
            0x11 => ReasonCode::NoSubscriptionExisted,
            0x18 => ReasonCode::ContinueAuthentication,
            0x19 => ReasonCode::ReAuthenticate,
            0x80 => ReasonCode::UnspecifiedError,
            0x81 => ReasonCode::MalformedPacket,
            0x82 => ReasonCode::ProtocolError,
            0x83 => ReasonCode::ImplementationSpecificError,
            0x84 => ReasonCode::UnsupportedProtocolVersion,
            0x85 => ReasonCode::ClientIdentifierNotValid,
            0x86 => ReasonCode::BadUserNameOrPassword,
            0x87 => ReasonCode::NotAuthorized,
            0x88 => ReasonCode::ServerUnavailable,
            0x89 => ReasonCode::ServerBusy,
            0x8a => ReasonCode::Banned,
            0x8b => ReasonCode::ServerShuttingDown,
            0x8c => ReasonCode::BadAuthenticationMethod,
            0x8d => ReasonCode::KeepAliveTimeout,
            0x8e => ReasonCode::SessionTakenOver,
            0x8f => ReasonCode::TopicFilterInvalid,
            0x90 => ReasonCode::TopicNameInvalid,
            0x91 => ReasonCode::PacketIdentifierInUse,
            0x92 => ReasonCode::PacketIdentifierNotFound,
            0x93 => ReasonCode::ReceiveMaximumExceeded,
            0x94 => ReasonCode::TopicAliasInvalid,
            0x95 => ReasonCode::PacketTooLarge,
            0x96 => ReasonCode::MessageRateTooHigh,
            0x97 => ReasonCode::QuotaExceeded,
            0x98 => ReasonCode::AdministrativeAction,
            0x99 => ReasonCode::PayloadFormatInvalid,
            0x9a => ReasonCode::RetainNotSupported,
            0x9b => ReasonCode::QoSNotSupported,
            0x9c => ReasonCode::UseAnotherServer,
            0x9d => ReasonCode::ServerMoved,
            0x9e => ReasonCode::SharedSubscriptionsNotSupported,
            0x9f => ReasonCode::ConnectionRateExceeded,
            0xa0 => ReasonCode::MaximumConnectTime,
            0xa1 => ReasonCode::SubscriptionIdentifiersNotSupported,
            0xa2 => ReasonCode::WildcardSubscriptionsNotSupported,
            _ => return Err(ReasonCode::MalformedPacket),
        };

        Ok(Some(reason))
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
        match *self {
            ReasonCode::Success => write!(f, "Succcess"),
            ReasonCode::NormalDisconnection => write!(f, "Normal disconnection"),
            ReasonCode::GrantedQoS0 => write!(f, "Granted QoS 0"),
            ReasonCode::GrantedQoS1 => write!(f, "Granted QoS 1"),
            ReasonCode::GrantedQoS2 => write!(f, "Granted QoS 2"),
            ReasonCode::DisconnectWithWillMessage => write!(f, "Disconnect with Will Message"),
            ReasonCode::NoMatchingSubscribers => write!(f, "No matching subscribers"),
            ReasonCode::NoSubscriptionExisted => write!(f, "No subscriptions existed"),
            ReasonCode::ContinueAuthentication => write!(f, "Continue authentication"),
            ReasonCode::ReAuthenticate => write!(f, "Re-authenticate"),
            ReasonCode::UnspecifiedError => write!(f, "Unspecified error"),
            ReasonCode::MalformedPacket => write!(f, "Malformed packet"),
            ReasonCode::ProtocolError => writeln!(f, "Protocol error"),
            ReasonCode::ImplementationSpecificError => write!(f, "Implementation specific error"),
            ReasonCode::UnsupportedProtocolVersion => write!(f, "Unsupported protocol version"),
            ReasonCode::ClientIdentifierNotValid => write!(f, "Client identifier not valid"),
            ReasonCode::BadUserNameOrPassword => write!(f, "Bad User Name or Password"),
            ReasonCode::NotAuthorized => write!(f, "Not authorized"),
            ReasonCode::ServerUnavailable => write!(f, "Server unavailable"),
            ReasonCode::ServerBusy => write!(f, "Server busy"),
            ReasonCode::Banned => write!(f, "Banned"),
            ReasonCode::ServerShuttingDown => write!(f, "Server shutting down"),
            ReasonCode::BadAuthenticationMethod => write!(f, "Bad authentication method"),
            ReasonCode::KeepAliveTimeout => write!(f, "Keep Alive timeout"),
            ReasonCode::SessionTakenOver => write!(f, "Session taken over"),
            ReasonCode::TopicFilterInvalid => write!(f, "Topic filter invalid"),
            ReasonCode::TopicNameInvalid => write!(f, "Topic name invalid"),
            ReasonCode::PacketIdentifierInUse => write!(f, "Packet identifier in use"),
            ReasonCode::PacketIdentifierNotFound => write!(f, "Packet identifier not found"),
            ReasonCode::ReceiveMaximumExceeded => write!(f, "Receive maximum exceeded"),
            ReasonCode::TopicAliasInvalid => write!(f, "Topic alias invalid"),
            ReasonCode::PacketTooLarge => write!(f, "Packet too large"),
            ReasonCode::MessageRateTooHigh => write!(f, "Message rate too high"),
            ReasonCode::QuotaExceeded => write!(f, "Quota exceeded"),
            ReasonCode::AdministrativeAction => write!(f, "Administrative action"),
            ReasonCode::PayloadFormatInvalid => write!(f, "Payload format invalid"),
            ReasonCode::RetainNotSupported => write!(f, "Retain not supported"),
            ReasonCode::QoSNotSupported => write!(f, "QoS not supported"),
            ReasonCode::UseAnotherServer => write!(f, "User another server"),
            ReasonCode::ServerMoved => write!(f, "Server moved"),
            ReasonCode::SharedSubscriptionsNotSupported => {
                write!(f, "Shared subscriptions not supported")
            }
            ReasonCode::ConnectionRateExceeded => write!(f, "Connection rate exceeded"),
            ReasonCode::MaximumConnectTime => write!(f, "Maximum connect time"),
            ReasonCode::SubscriptionIdentifiersNotSupported => {
                write!(f, "Subscription indentifiers not supported")
            }
            ReasonCode::WildcardSubscriptionsNotSupported => {
                write!(f, "Wildcard subscriptions not supported")
            }
        }
    }
}
