use bytes::{Bytes, BytesMut, BufMut};
use crate::endec::{Encoder, VariableByteInteger};
use std::mem;

#[repr(u32)]
#[derive(Clone, Copy)]
pub enum Property {
    PayloadFormatIndicator = 0x01,
    MessageExpiryInterval = 0x02,
    ContentType = 0x03,
    ResponseTopic = 0x08,
    CorrelationData = 0x09,
    SubscriptionIdentifier = 0x0b,
    SessionExpiryInterval = 0x11,
    AssignedClientIdentifier = 0x12,
    ServerKeepAlive = 0x13,
    AuthenticationMethod = 0x15,
    AuthenticationData = 0x16,
    RequestProblemInformation = 0x17,
    WillDelayInterval = 0x18,
    RequestResponseInformation = 0x19,
    ResponseInformation = 0x1a,
    ServerReference = 0x1c,
    ReasonString = 0x1f,
    ReceiveMaximum = 0x21,
    TopicAliasMaximum = 0x22,
    TopicAlias = 0x23,
    MaximumQoS = 0x24,
    RetainAvailable = 0x25,
    UserProperty = 0x26,
    MaximumPacketSize = 0x27,
    WildcardSubscriptionAvailable = 0x28,
    SubscriptionIdentifierAvailable = 0x29,
    SharedSubscriptionAvailable = 0x2a,
}

impl Encoder for Property {
    fn encode(&self, buffer: &mut BytesMut) {
        buffer.put_u8(*self as u8);
    }

    fn get_encoded_size(&self) -> usize {
        mem::size_of::<u8>()
    }
}

macro_rules! endecable_property {
    ($t:ident {$($n:tt: $s:ty),*})  => {
        pub struct $t {$($n: $s,)*}

        impl $t {
            pub fn new($($n: $s,)*) -> $t {
                $t {
                    $($n,)*
                }
            }
        }

        impl Encoder for $t {
            fn encode(&self, buffer: &mut BytesMut) {
                Property::$t.encode(buffer);
                $(
                    self.$n.encode(buffer);
                )*
            }

            fn get_encoded_size(&self) -> usize {
                let mut len = 0;

                len += Property::$t.get_encoded_size();

                $(
                    len += self.$n.get_encoded_size();
                )*

                len
            }
        }
    }
}

endecable_property!(PayloadFormatIndicator {value: u8});
endecable_property!(MessageExpiryInterval {value: u32});
endecable_property!(ContentType {value: String});
endecable_property!(ResponseTopic {value: String});
endecable_property!(CorrelationData {value: Bytes});
endecable_property!(SubscriptionIdentifier {value: VariableByteInteger});
endecable_property!(SessionExpiryInterval {value: u32});
endecable_property!(AssignedClientIdentifier {value: String});
endecable_property!(ServerKeepAlive {value: u16});
endecable_property!(AuthenticationMethod {value: String});
endecable_property!(AuthenticationData {value: Bytes});
endecable_property!(RequestProblemInformation {value: u8});
endecable_property!(WillDelayInterval {value: u32});
endecable_property!(RequestResponseInformation {value: u8});
endecable_property!(ResponseInformation {value: String});
endecable_property!(ServerReference {value: String});
endecable_property!(ReasonString {value: String});
endecable_property!(ReceiveMaximum {value: u16});
endecable_property!(TopicAliasMaximum {value: u16});
endecable_property!(TopicAlias {value: u16});
endecable_property!(MaximumQoS {value: u8});
endecable_property!(RetainAvailable {value: bool});
endecable_property!(UserProperty {key: String, value: String});
endecable_property!(MaximumPacketSize {value: u32});
endecable_property!(WildcardSubscriptionAvailable {value: bool});
endecable_property!(SubscriptionIdentifierAvailable {value: bool});
endecable_property!(SharedSubscriptionAvailable {value: bool});
