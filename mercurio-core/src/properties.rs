//! MQTT 5.0 properties encoding and decoding.

#[cfg(not(feature = "std"))]
use alloc::{string::String, vec::Vec};

use bytes::{Buf, Bytes, BytesMut};

use crate::{
    codec::{Decoder, Encoder, VariableByteInteger},
    error::Error,
    reason::ReasonCode,
    topic::validate_publish_topic,
};

macro_rules! def_prop {
    ($t:ident {$i:ident: $a:expr, $($n:tt: $s:ty),*})  => {
        /// An MQTT 5 property. Its meaning is described on the [`Property`]
        /// variant of the same name.
        #[derive(Debug, Default, PartialEq, Eq, Clone)]
        pub struct $t {$(pub $n: $s,)*}

        impl $t {
            /// Property identifier, encoded as a Variable Byte Integer
            /// before the value.
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
            fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
                Ok($t::new($(<$s>::decode(buffer)?,)*))
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

/// An MQTT 5 property (section 2.2.2.2), as read from a property list.
///
/// Decoding reads the identifier and the value. An unknown identifier is a
/// [`ReasonCode::MalformedPacket`], and a value the spec rules out (such as
/// a Topic Alias of 0) is a [`ReasonCode::ProtocolError`]. Decoding does
/// not check that the packet type allows the property; the packet decoders
/// do that.
#[derive(Clone, PartialEq, Eq, Debug)]
#[allow(clippy::enum_variant_names)] // Warns because of UserProperty
#[non_exhaustive]
pub enum Property {
    /// 0: the payload is unspecified bytes. 1: the payload is UTF-8 character
    /// data. Other values are a protocol error. PUBLISH and will properties.
    PayloadFormatIndicator(PayloadFormatIndicator),
    /// Lifetime of the application message in seconds. Without it the message
    /// does not expire. PUBLISH and will properties.
    MessageExpiryInterval(MessageExpiryInterval),
    /// Describes the payload content, such as a MIME type. The application
    /// defines its meaning. PUBLISH and will properties.
    ContentType(ContentType),
    /// Topic name the receiver should publish a response to. It must be a
    /// topic name, without wildcards [MQTT-3.3.2-14]; the decoder rejects a
    /// filter as a protocol error. PUBLISH and will properties.
    ResponseTopic(ResponseTopic),
    /// Binary data the requester uses to match a response to its request.
    /// PUBLISH and will properties.
    CorrelationData(CorrelationData),
    /// Identifier of a subscription, 1 to 268,435,455; 0 is a protocol
    /// error. The client sets it in SUBSCRIBE, and the server adds it to
    /// each PUBLISH that matches the subscription, so one PUBLISH can carry
    /// several.
    SubscriptionIdentifier(SubscriptionIdentifier),
    /// Seconds the session state is kept after the network connection
    /// closes. 0 or absent ends the session with the connection, and
    /// `u32::MAX` means the session never expires. CONNECT, CONNACK and
    /// DISCONNECT.
    SessionExpiryInterval(SessionExpiryInterval),
    /// Client identifier the server assigned because the CONNECT had an empty
    /// one. CONNACK.
    AssignedClientIdentifier(AssignedClientIdentifier),
    /// Keep alive in seconds that the server requires. It replaces the value
    /// the client sent in CONNECT. CONNACK.
    ServerKeepAlive(ServerKeepAlive),
    /// Name of the extended authentication method (section 4.12). CONNECT,
    /// CONNACK and AUTH.
    AuthenticationMethod(AuthenticationMethod),
    /// Binary data of the extended authentication exchange. The
    /// authentication method defines its content. CONNECT, CONNACK and AUTH.
    AuthenticationData(AuthenticationData),
    /// 0: the server sends a Reason String or User Properties only in PUBLISH,
    /// CONNACK and DISCONNECT [MQTT-3.1.2-29]. 1 or absent: in any packet.
    /// Other values are a protocol error. CONNECT.
    RequestProblemInformation(RequestProblemInformation),
    /// Seconds the server waits after the connection closes before it
    /// publishes the will message. The will is not sent if the client
    /// reconnects to the session first. Will properties in CONNECT.
    WillDelayInterval(WillDelayInterval),
    /// 1 asks the server to send Response Information in CONNACK. 0 or absent
    /// does not. Other values are a protocol error. CONNECT.
    RequestResponseInformation(RequestResponseInformation),
    /// String the client can use as the base of its response topics. CONNACK.
    ResponseInformation(ResponseInformation),
    /// Other server the client can use, sent with
    /// [`ReasonCode::UseAnotherServer`] or [`ReasonCode::ServerMoved`].
    /// CONNACK and DISCONNECT.
    ServerReference(ServerReference),
    /// Human-readable text about the reason code, for diagnostics. The
    /// receiver must not parse it.
    ReasonString(ReasonString),
    /// Most QoS 1 and QoS 2 PUBLISH packets the sender of this property will
    /// process at once without acknowledging them. 1 to 65,535; absent means
    /// 65,535 and 0 is a protocol error. CONNECT and CONNACK.
    ReceiveMaximum(ReceiveMaximum),
    /// Highest Topic Alias the sender of this property accepts. 0 or absent
    /// means it accepts none. CONNECT and CONNACK.
    TopicAliasMaximum(TopicAliasMaximum),
    /// Number that stands for a topic name in later PUBLISH packets on the
    /// same connection. 1 up to the receiver's Topic Alias Maximum; 0 is a
    /// protocol error. PUBLISH.
    TopicAlias(TopicAlias),
    /// Highest QoS the server supports: 0 or 1. Absent means 2, and other
    /// values are a protocol error. CONNACK.
    MaximumQoS(MaximumQoS),
    /// Whether the server supports retained messages. Absent means it does.
    /// CONNACK.
    RetainAvailable(RetainAvailable),
    /// Name and value pair defined by the application. Most packets allow it,
    /// and it may appear more than once, also with the same name.
    UserProperty(UserProperty),
    /// Largest whole packet, in bytes, that the sender of this property
    /// accepts. Absent means no limit beyond the protocol's; 0 is a protocol
    /// error. CONNECT and CONNACK.
    MaximumPacketSize(MaximumPacketSize),
    /// Whether the server supports wildcard subscriptions. Absent means it
    /// does. CONNACK.
    WildcardSubscriptionAvailable(WildcardSubscriptionAvailable),
    /// Whether the server supports Subscription Identifiers. Absent means it
    /// does. CONNACK.
    SubscriptionIdentifierAvailable(SubscriptionIdentifierAvailable),
    /// Whether the server supports shared subscriptions. Absent means it does.
    /// CONNACK.
    SharedSubscriptionAvailable(SharedSubscriptionAvailable),
}

macro_rules! dec_prop {
    ($name:ident, $buf: tt) => {
        Property::$name($name::decode($buf)?)
    };
}

#[inline(always)]
fn decode_with_id<T: Buf>(id: u32, buffer: &mut T) -> crate::Result<Property> {
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
        RequestResponseInformation::ID => dec_prop!(RequestResponseInformation, buffer),
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

    // Values the spec rules out are protocol errors
    let valid = match &property {
        Property::PayloadFormatIndicator(p) => p.value <= 1,
        Property::RequestProblemInformation(p) => p.value <= 1,
        Property::RequestResponseInformation(p) => p.value <= 1,
        Property::MaximumQoS(p) => p.value <= 1,
        Property::SubscriptionIdentifier(p) => p.value.0 != 0,
        Property::TopicAlias(p) => p.value != 0,
        Property::ReceiveMaximum(p) => p.value != 0,
        Property::MaximumPacketSize(p) => p.value != 0,
        // A Response Topic is a topic name, never a filter [MQTT-3.3.2-14]
        Property::ResponseTopic(p) => validate_publish_topic(&p.value).is_ok(),
        _ => true,
    };
    if !valid {
        return Err(ReasonCode::ProtocolError.into());
    }

    Ok(property)
}

/// Stores a property that may appear at most once in a packet; a repeat is
/// a protocol error [MQTT-2.2.2-2].
pub fn set_once<T>(slot: &mut Option<T>, value: T) -> crate::Result<()> {
    if slot.is_some() {
        return Err(ReasonCode::ProtocolError.into());
    }
    *slot = Some(value);
    Ok(())
}

/// Authentication Data only means something with an Authentication Method;
/// alone it is a protocol error (section 3.1.2.11.10).
pub fn check_authentication(
    method: &Option<AuthenticationMethod>,
    data: &Option<AuthenticationData>,
) -> crate::Result<()> {
    if data.is_some() && method.is_none() {
        return Err(ReasonCode::ProtocolError.into());
    }
    Ok(())
}

/// Appends a User Property, which may appear any number of times.
pub fn push_user_property(slot: &mut Option<Vec<UserProperty>>, value: UserProperty) {
    slot.get_or_insert_with(Vec::new).push(value);
}

/// Reads a property list (section 2.2.2): its Variable Byte Integer length,
/// then each property in it, handed to `accept`.
///
/// A length that runs past the end of `buffer`, or a property cut short by
/// the end of the list, is a [`ReasonCode::MalformedPacket`]. `accept`
/// rejects the properties its packet does not allow.
pub fn decode_property_list<T, F>(buffer: &mut T, accept: F) -> crate::Result<()>
where
    T: Buf,
    F: FnMut(Property) -> crate::Result<()>,
{
    let len = VariableByteInteger::decode(buffer)?.0 as usize;
    if buffer.remaining() < len {
        return Err(ReasonCode::MalformedPacket.into());
    }

    decode_properties(&mut buffer.take(len), accept)
}

/// Reads properties until `buffer` is empty, for a list whose length is
/// already known.
pub(crate) fn decode_properties<T, F>(buffer: &mut T, mut accept: F) -> crate::Result<()>
where
    T: Buf,
    F: FnMut(Property) -> crate::Result<()>,
{
    while buffer.has_remaining() {
        let property = Property::decode(buffer).map_err(|e| match e {
            Error::PacketIncomplete => ReasonCode::MalformedPacket.into(),
            e => e,
        })?;
        accept(property)?;
    }

    Ok(())
}

impl Decoder for Property {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        let id = VariableByteInteger::decode(buffer)?.0;

        decode_with_id(id, buffer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(bytes: &[u8]) -> crate::Result<Property> {
        Property::decode(&mut Bytes::copy_from_slice(bytes))
    }

    #[test]
    fn test_out_of_range_values_are_protocol_errors() {
        let invalid: [&[u8]; 6] = [
            // Payload Format Indicator 2
            &[0x01, 0x02],
            // Request Problem Information 2
            &[0x17, 0x02],
            // Retain Available 2
            &[0x25, 0x02],
            // Topic Alias 0
            &[0x23, 0x00, 0x00],
            // Receive Maximum 0
            &[0x21, 0x00, 0x00],
            // Subscription Identifier 0
            &[0x0b, 0x00],
        ];
        for bytes in invalid {
            assert!(
                matches!(
                    decode(bytes),
                    Err(crate::error::Error::Reason(ReasonCode::ProtocolError))
                ),
                "{bytes:02x?}"
            );
        }

        // Response Topic "a/+"
        assert!(matches!(
            decode(&[0x08, 0x00, 0x03, b'a', b'/', b'+']),
            Err(crate::error::Error::Reason(ReasonCode::ProtocolError))
        ));
        assert!(decode(&[0x08, 0x00, 0x03, b'a', b'/', b'b']).is_ok());

        assert!(decode(&[0x01, 0x01]).is_ok());
        assert!(decode(&[0x25, 0x00]).is_ok());
        assert!(decode(&[0x23, 0x00, 0x01]).is_ok());
    }
}
