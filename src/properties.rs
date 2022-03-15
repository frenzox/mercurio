use bytes::{Buf, Bytes, BytesMut};

use crate::codec::{Decoder, Encoder, VariableByteInteger};
use crate::reason::ReasonCode;
use crate::result::Result;

macro_rules! def_prop {
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

def_prop!(PayloadFormatIndicator {
    ID: 0x01,
    value: u8
});

def_prop!(MessageExpiryInterval {
    ID: 0x02,
    value: u32
});

def_prop!(ContentType {
    ID: 0x03,
    value: String
});

def_prop!(ResponseTopic {
    ID: 0x08,
    value: String
});

def_prop!(CorrelationData {
    ID: 0x09,
    value: Bytes
});

def_prop!(SubscriptionIdentifier {
    ID: 0x0b,
    value: VariableByteInteger
});

def_prop!(SessionExpiryInterval {
    ID: 0x11,
    value: u32
});

def_prop!(AssignedClientIdentifier {
    ID: 0x12,
    value: String
});

def_prop!(ServerKeepAlive {
    ID: 0x13,
    value: u16
});

def_prop!(AuthenticationMethod {
    ID: 0x15,
    value: String
});

def_prop!(AuthenticationData {
    ID: 0x16,
    value: Bytes
});

def_prop!(RequestProblemInformation {
    ID: 0x17,
    value: u8
});

def_prop!(WillDelayInterval {
    ID: 0x18,
    value: u32
});

def_prop!(RequestResponseInformation {
    ID: 0x19,
    value: u8
});

def_prop!(ResponseInformation {
    ID: 0x1a,
    value: String
});

def_prop!(ServerReference {
    ID: 0x1c,
    value: String
});

def_prop!(ReasonString {
    ID: 0x1f,
    value: String
});

def_prop!(ReceiveMaximum {
    ID: 0x21,
    value: u16
});

def_prop!(TopicAliasMaximum {
    ID: 0x22,
    value: u16
});

def_prop!(TopicAlias {
    ID: 0x23,
    value: u16
});

def_prop!(MaximumQoS {
    ID: 0x24,
    value: u8
});

def_prop!(RetainAvailable {
    ID: 0x25,
    value: bool
});

def_prop!(UserProperty {
    ID: 0x26,
    key: String,
    value: String
});

def_prop!(MaximumPacketSize {
    ID: 0x27,
    value: u32
});

def_prop!(WildcardSubscriptionAvailable {
    ID: 0x28,
    value: bool
});

def_prop!(SubscriptionIdentifierAvailable {
    ID: 0x29,
    value: bool
});

def_prop!(SharedSubscriptionAvailable {
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

macro_rules! dec_prop {
    ($name:ident, $buf: tt) => {
        Property::$name($name::decode($buf, None)?)
    };
}

#[inline(always)]
fn decode_with_id<T: Buf>(id: u32, buffer: &mut T) -> Result<Property> {
    let property = match id {
        PayloadFormatIndicator::ID => dec_prop!(PayloadFormatIndicator, buffer),
        MessageExpiryInterval::ID => dec_prop!(MessageExpiryInterval, buffer),
        ContentType::ID => dec_prop!(ContentType, buffer),
        ResponseTopic::ID => dec_prop!(ResponseTopic, buffer),
        CorrelationData::ID => dec_prop!(CorrelationData, buffer),
        SubscriptionIdentifier::ID => dec_prop!(SubscriptionIdentifier, buffer),
        SessionExpiryInterval::ID => dec_prop!(SessionExpiryInterval, buffer),
        AssignedClientIdentifier::ID => dec_prop!(AssignedClientIdentifier, buffer),
        ServerKeepAlive::ID => dec_prop!(ServerKeepAlive, buffer),
        AuthenticationMethod::ID => dec_prop!(AuthenticationMethod, buffer),
        AuthenticationData::ID => dec_prop!(AuthenticationData, buffer),
        RequestProblemInformation::ID => dec_prop!(RequestProblemInformation, buffer),
        WillDelayInterval::ID => dec_prop!(WillDelayInterval, buffer),
        RequestResponseInformation::ID => dec_prop!(RequestProblemInformation, buffer),
        ResponseInformation::ID => dec_prop!(ResponseInformation, buffer),
        ServerReference::ID => dec_prop!(ServerReference, buffer),
        ReasonString::ID => dec_prop!(ReasonString, buffer),
        ReceiveMaximum::ID => dec_prop!(ReceiveMaximum, buffer),
        TopicAliasMaximum::ID => dec_prop!(TopicAliasMaximum, buffer),
        TopicAlias::ID => dec_prop!(TopicAlias, buffer),
        MaximumQoS::ID => dec_prop!(MaximumQoS, buffer),
        RetainAvailable::ID => dec_prop!(RetainAvailable, buffer),
        UserProperty::ID => dec_prop!(UserProperty, buffer),
        MaximumPacketSize::ID => dec_prop!(MaximumPacketSize, buffer),
        WildcardSubscriptionAvailable::ID => dec_prop!(WildcardSubscriptionAvailable, buffer),
        SubscriptionIdentifierAvailable::ID => dec_prop!(SubscriptionIdentifierAvailable, buffer),
        SharedSubscriptionAvailable::ID => dec_prop!(SharedSubscriptionAvailable, buffer),
        _ => return Err(ReasonCode::MalformedPacket.into()),
    };

    Ok(property)
}

impl Decoder for Property {
    type Context = ();

    fn decode<T: Buf>(buffer: &mut T, _context: Option<&Self::Context>) -> Result<Self> {
        let id = VariableByteInteger::decode(buffer, None)?.0;

        decode_with_id(id, buffer)
    }
}
