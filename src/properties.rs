use bytes::{Buf, Bytes, BytesMut};

use crate::codec::{Decoder, Encoder, VariableByteInteger};
use crate::reason::ReasonCode;
use crate::result::Result;

macro_rules! define_property {
    ($t:ident {$i:ident: $a:expr, $($n:tt: $s:ty),*})  => {
        #[derive(PartialEq)]
        #[derive(Debug)]
        pub struct $t {$($n: $s,)*}

        impl $t {
            pub const $i: u32 = $a;
            pub fn new($($n: $s,)*) -> $t {
                $t {
                    $($n,)*
                }
            }
        }

        impl Encoder for $t {
            fn encode(&self, buffer: &mut BytesMut) {
                VariableByteInteger(Self::$i).encode(buffer);

                $(
                    self.$n.encode(buffer);
                )*
            }

            fn encoded_size(&self) -> usize {
                let mut len = 0;

                len += VariableByteInteger(Self::$i).encoded_size();

                $(
                    len += self.$n.encoded_size();
                )*

                len
            }
        }

        impl Decoder for $t {
            type Context = ();

            fn decode<T: Buf>(buffer: &mut T, _context: Option<&Self::Context>) -> Result<Self> {
                Ok($t::new($(<$s>::decode(buffer, None)?,)*))
            }
        }
    }
}

define_property!(PayloadFormatIndicator {
    ID: 0x01,
    value: u8
});

define_property!(MessageExpiryInterval {
    ID: 0x02,
    value: u32
});

define_property!(ContentType {
    ID: 0x03,
    value: String
});

define_property!(ResponseTopic {
    ID: 0x08,
    value: String
});

define_property!(CorrelationData {
    ID: 0x09,
    value: Bytes
});

define_property!(SubscriptionIdentifier {
    ID: 0x0b,
    value: VariableByteInteger
});

define_property!(SessionExpiryInterval {
    ID: 0x11,
    value: u32
});

define_property!(AssignedClientIdentifier {
    ID: 0x12,
    value: String
});

define_property!(ServerKeepAlive {
    ID: 0x13,
    value: u16
});

define_property!(AuthenticationMethod {
    ID: 0x15,
    value: String
});

define_property!(AuthenticationData {
    ID: 0x16,
    value: Bytes
});

define_property!(RequestProblemInformation {
    ID: 0x17,
    value: u8
});

define_property!(WillDelayInterval {
    ID: 0x18,
    value: u32
});

define_property!(RequestResponseInformation {
    ID: 0x19,
    value: u8
});

define_property!(ResponseInformation {
    ID: 0x1a,
    value: String
});

define_property!(ServerReference {
    ID: 0x1c,
    value: String
});

define_property!(ReasonString {
    ID: 0x1f,
    value: String
});

define_property!(ReceiveMaximum {
    ID: 0x21,
    value: u16
});

define_property!(TopicAliasMaximum {
    ID: 0x22,
    value: u16
});

define_property!(TopicAlias {
    ID: 0x23,
    value: u16
});

define_property!(MaximumQoS {
    ID: 0x24,
    value: u8
});

define_property!(RetainAvailable {
    ID: 0x25,
    value: bool
});

define_property!(UserProperty {
    ID: 0x26,
    key: String,
    value: String
});

define_property!(MaximumPacketSize {
    ID: 0x27,
    value: u32
});

define_property!(WildcardSubscriptionAvailable {
    ID: 0x28,
    value: bool
});

define_property!(SubscriptionIdentifierAvailable {
    ID: 0x29,
    value: bool
});

define_property!(SharedSubscriptionAvailable {
    ID: 0x2a,
    value: bool
});

#[derive(PartialEq, Debug)]
#[allow(clippy::enum_variant_names)] // Warns because of UserProperty
pub enum Property {
    PayloadFormatIndicator(PayloadFormatIndicator),
    MessageExpiryInterval(MessageExpiryInterval),
    ContentType(ContentType),
    ResponseTopic(ResponseTopic),
    CorrelationData(CorrelationData),
    SubscriptionIdentifier(SubscriptionIdentifier),
    SessionExpiryInterval(SessionExpiryInterval),
    AssignedClientIdentifier(AssignedClientIdentifier),
    ServerKeepAlive(ServerKeepAlive),
    AuthenticationMethod(AuthenticationMethod),
    AuthenticationData(AuthenticationData),
    RequestProblemInformation(RequestProblemInformation),
    WillDelayInterval(WillDelayInterval),
    RequestResponseInformation(RequestResponseInformation),
    ResponseInformation(ResponseInformation),
    ServerReference(ServerReference),
    ReasonString(ReasonString),
    ReceiveMaximum(ReceiveMaximum),
    TopicAliasMaximum(TopicAliasMaximum),
    TopicAlias(TopicAlias),
    MaximumQoS(MaximumQoS),
    RetainAvailable(RetainAvailable),
    UserProperty(UserProperty),
    MaximumPacketSize(MaximumPacketSize),
    WildcardSubscriptionAvailable(WildcardSubscriptionAvailable),
    SubscriptionIdentifierAvailable(SubscriptionIdentifierAvailable),
    SharedSubscriptionAvailable(SharedSubscriptionAvailable),
}

impl Decoder for Property {
    type Context = ();

    fn decode<T: Buf>(buffer: &mut T, _context: Option<&Self::Context>) -> Result<Self> {
        let id = VariableByteInteger::decode(buffer, None)?.0;
        let property = match id {
            PayloadFormatIndicator::ID => {
                Property::PayloadFormatIndicator(PayloadFormatIndicator::decode(buffer, None)?)
            }
            MessageExpiryInterval::ID => {
                Property::MessageExpiryInterval(MessageExpiryInterval::decode(buffer, None)?)
            }
            ContentType::ID => Property::ContentType(ContentType::decode(buffer, None)?),
            ResponseTopic::ID => Property::ResponseTopic(ResponseTopic::decode(buffer, None)?),
            CorrelationData::ID => {
                Property::CorrelationData(CorrelationData::decode(buffer, None)?)
            }
            SubscriptionIdentifier::ID => {
                Property::SubscriptionIdentifier(SubscriptionIdentifier::decode(buffer, None)?)
            }
            SessionExpiryInterval::ID => {
                Property::SessionExpiryInterval(SessionExpiryInterval::decode(buffer, None)?)
            }
            AssignedClientIdentifier::ID => {
                Property::AssignedClientIdentifier(AssignedClientIdentifier::decode(buffer, None)?)
            }
            ServerKeepAlive::ID => {
                Property::ServerKeepAlive(ServerKeepAlive::decode(buffer, None)?)
            }
            AuthenticationMethod::ID => {
                Property::AuthenticationMethod(AuthenticationMethod::decode(buffer, None)?)
            }
            AuthenticationData::ID => {
                Property::AuthenticationData(AuthenticationData::decode(buffer, None)?)
            }
            RequestProblemInformation::ID => Property::RequestProblemInformation(
                RequestProblemInformation::decode(buffer, None)?,
            ),
            WillDelayInterval::ID => {
                Property::WillDelayInterval(WillDelayInterval::decode(buffer, None)?)
            }
            RequestResponseInformation::ID => Property::RequestResponseInformation(
                RequestResponseInformation::decode(buffer, None)?,
            ),
            ResponseInformation::ID => {
                Property::ResponseInformation(ResponseInformation::decode(buffer, None)?)
            }
            ServerReference::ID => {
                Property::ServerReference(ServerReference::decode(buffer, None)?)
            }
            ReasonString::ID => Property::ReasonString(ReasonString::decode(buffer, None)?),
            ReceiveMaximum::ID => Property::ReceiveMaximum(ReceiveMaximum::decode(buffer, None)?),
            TopicAliasMaximum::ID => {
                Property::TopicAliasMaximum(TopicAliasMaximum::decode(buffer, None)?)
            }
            TopicAlias::ID => Property::TopicAlias(TopicAlias::decode(buffer, None)?),
            MaximumQoS::ID => Property::MaximumQoS(MaximumQoS::decode(buffer, None)?),
            RetainAvailable::ID => {
                Property::RetainAvailable(RetainAvailable::decode(buffer, None)?)
            }
            UserProperty::ID => Property::UserProperty(UserProperty::decode(buffer, None)?),
            MaximumPacketSize::ID => {
                Property::MaximumPacketSize(MaximumPacketSize::decode(buffer, None)?)
            }
            WildcardSubscriptionAvailable::ID => Property::WildcardSubscriptionAvailable(
                WildcardSubscriptionAvailable::decode(buffer, None)?,
            ),
            SubscriptionIdentifierAvailable::ID => Property::SubscriptionIdentifierAvailable(
                SubscriptionIdentifierAvailable::decode(buffer, None)?,
            ),
            SharedSubscriptionAvailable::ID => Property::SharedSubscriptionAvailable(
                SharedSubscriptionAvailable::decode(buffer, None)?,
            ),
            _ => return Err(ReasonCode::MalformedPacket.into()),
        };

        Ok(property)
    }
}
