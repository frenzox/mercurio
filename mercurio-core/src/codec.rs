//! Encoding and decoding traits for MQTT packets.

use core::mem;

#[cfg(not(feature = "std"))]
use alloc::{string::String, vec::Vec};

use bytes::{Buf, BufMut, Bytes, BytesMut};

use crate::{error::Error, reason::ReasonCode};

/// A type that can be read from its MQTT wire encoding.
pub trait Decoder {
    /// Reads one value from the front of `buffer` and advances past it.
    ///
    /// Returns [`Error::PacketIncomplete`] when the buffer ends before a
    /// fixed-size field or a length prefix is complete. A length prefix that
    /// points past the end of the buffer, or a value the spec forbids, is a
    /// [`ReasonCode::MalformedPacket`] or [`ReasonCode::ProtocolError`].
    fn decode<T>(buffer: &mut T) -> crate::Result<Self>
    where
        Self: Sized,
        T: Buf;
}

/// A type that can be written in its MQTT wire encoding.
pub trait Encoder {
    /// Appends the encoded value to `buffer`.
    fn encode(&self, buffer: &mut BytesMut);

    /// Number of bytes [`encode`](Encoder::encode) writes.
    ///
    /// The default returns the in-memory size of the value. It is only
    /// correct for fixed-size integers; every other type overrides it.
    fn encoded_size(&self) -> usize {
        mem::size_of_val(self)
    }
}

/// The longest packet MQTT can frame: the fixed header byte, a four-byte
/// Remaining Length, and the 268,435,455 bytes that length can count.
pub const MAX_PACKET_SIZE: usize = 1 + 4 + 268_435_455;

/// Writes the two-byte length of a string or binary value (section 1.5.4).
///
/// # Panics
///
/// If `len` is above 65,535, which MQTT can't carry.
fn put_length(len: usize, buffer: &mut BytesMut) {
    let len = u16::try_from(len).expect("MQTT strings and binary data are at most 65,535 bytes");
    buffer.put_u16(len);
}

fn encode_var_byte_integer(value: u32, encoded: &mut BytesMut) {
    let mut x = value;

    loop {
        let mut encoded_byte: u8 = (x % 128) as u8;
        x /= 128;

        if x > 0 {
            encoded_byte |= 0b1000_0000;
        }

        encoded.put_u8(encoded_byte);

        if x == 0 {
            break;
        }
    }
}

fn decode_var_byte_integer<T: Buf>(encoded: &mut T) -> crate::Result<VariableByteInteger> {
    let mut value: u32 = 0;

    // At most four bytes; a fifth continuation is malformed [MQTT-1.5.5-1]
    for shift in [0, 7, 14, 21] {
        if !encoded.has_remaining() {
            return Err(Error::PacketIncomplete);
        }
        let encoded_byte = encoded.get_u8();
        value |= ((encoded_byte & 0b0111_1111) as u32) << shift;

        if (encoded_byte & 0b1000_0000) == 0 {
            return Ok(VariableByteInteger(value));
        }
    }

    Err(ReasonCode::MalformedPacket.into())
}

/// An MQTT Variable Byte Integer (section 1.5.5).
///
/// Each byte carries seven bits of the value, least significant first, and
/// its high bit is set when another byte follows. One to four bytes encode
/// 0 to 268,435,455. It is used for the Remaining Length, property lengths,
/// property identifiers and the Subscription Identifier.
///
/// Decoding fails with [`Error::PacketIncomplete`] when the buffer ends
/// before the last byte, and with [`ReasonCode::MalformedPacket`] when a
/// fourth byte still has its continuation bit set [MQTT-1.5.5-1].
/// Values above 268,435,455 encode to five bytes, which no peer accepts;
/// packets that large are longer than [`MAX_PACKET_SIZE`].
///
/// ```
/// use bytes::BytesMut;
/// use mercurio_core::codec::{Decoder, Encoder, VariableByteInteger};
///
/// let mut buffer = BytesMut::new();
/// VariableByteInteger(321).encode(&mut buffer);
/// assert_eq!(&buffer[..], &[0xc1, 0x02]);
/// assert_eq!(VariableByteInteger::decode(&mut buffer).unwrap().0, 321);
/// ```
#[derive(PartialEq, Eq, Debug, Default, Clone)]
pub struct VariableByteInteger(pub u32);

impl Encoder for VariableByteInteger {
    fn encode(&self, buffer: &mut BytesMut) {
        encode_var_byte_integer(self.0, buffer);
    }

    fn encoded_size(&self) -> usize {
        match self.0 {
            0..=127 => 1,
            128..=16383 => 2,
            16384..=2097151 => 3,
            2097152..=268435455 => 4,
            // Too large for MQTT; encode writes five bytes, and the packet
            // is longer than MAX_PACKET_SIZE
            _ => 5,
        }
    }
}

impl Decoder for VariableByteInteger {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        decode_var_byte_integer(buffer)
    }
}

/// Encoding panics when the value is longer than 65,535 bytes.
impl Encoder for String {
    fn encode(&self, buffer: &mut BytesMut) {
        put_length(self.len(), buffer);
        buffer.put(self.as_bytes());
    }

    fn encoded_size(&self) -> usize {
        self.len() + mem::size_of::<u16>()
    }
}

impl Decoder for String {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        if buffer.remaining() < 2 {
            return Err(Error::PacketIncomplete);
        }

        let length = buffer.get_u16();
        if buffer.remaining() < length as usize {
            return Err(ReasonCode::MalformedPacket.into());
        }

        let bytes = buffer.copy_to_bytes(length.into());

        match String::from_utf8(bytes.to_vec()) {
            // U+0000 is never allowed in an MQTT string [MQTT-1.5.4-2]
            Ok(s) if !s.contains('\0') => Ok(s),
            _ => Err(ReasonCode::MalformedPacket.into()),
        }
    }
}

/// Encoding panics when the value is longer than 65,535 bytes.
impl Encoder for &'static str {
    fn encode(&self, buffer: &mut BytesMut) {
        put_length(self.len(), buffer);
        buffer.put(self.as_bytes());
    }

    fn encoded_size(&self) -> usize {
        self.len() + mem::size_of::<u16>()
    }
}

impl Encoder for u8 {
    fn encode(&self, buffer: &mut BytesMut) {
        buffer.put_u8(*self);
    }
}

impl Decoder for u8 {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        if !buffer.has_remaining() {
            return Err(Error::PacketIncomplete);
        }

        Ok(buffer.get_u8())
    }
}

impl Encoder for u16 {
    fn encode(&self, buffer: &mut BytesMut) {
        buffer.put_u16(*self);
    }
}

impl Decoder for u16 {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        if buffer.remaining() < 2 {
            return Err(Error::PacketIncomplete);
        }

        Ok(buffer.get_u16())
    }
}

impl Encoder for u32 {
    fn encode(&self, buffer: &mut BytesMut) {
        buffer.put_u32(*self);
    }
}

impl Decoder for u32 {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        if buffer.remaining() < 4 {
            return Err(Error::PacketIncomplete);
        }

        Ok(buffer.get_u32())
    }
}

impl Encoder for bool {
    fn encode(&self, buffer: &mut BytesMut) {
        buffer.put_u8(*self as u8);
    }
}

impl Decoder for bool {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        if buffer.remaining() < 1 {
            return Err(Error::PacketIncomplete);
        }

        // Byte flags only take 0 and 1
        match buffer.get_u8() {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(ReasonCode::ProtocolError.into()),
        }
    }
}

/// Encoding panics when the value is longer than 65,535 bytes.
impl Encoder for Bytes {
    fn encode(&self, buffer: &mut BytesMut) {
        put_length(self.len(), buffer);
        buffer.extend(self);
    }

    fn encoded_size(&self) -> usize {
        mem::size_of::<u16>() + self.len()
    }
}

impl Decoder for Bytes {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        if buffer.remaining() < 2 {
            return Err(Error::PacketIncomplete);
        }

        let length = buffer.get_u16();
        if buffer.remaining() < length as usize {
            return Err(ReasonCode::MalformedPacket.into());
        }

        Ok(buffer.copy_to_bytes(length.into()))
    }
}

impl<T> Encoder for Option<T>
where
    T: Encoder,
{
    fn encode(&self, buffer: &mut BytesMut) {
        if let Some(v) = self {
            v.encode(buffer)
        }
    }

    fn encoded_size(&self) -> usize {
        match self {
            Some(v) => v.encoded_size(),
            None => 0,
        }
    }
}

impl<T> Encoder for Vec<T>
where
    T: Encoder,
{
    fn encode(&self, buffer: &mut BytesMut) {
        for e in self {
            e.encode(buffer);
        }
    }

    fn encoded_size(&self) -> usize {
        let mut len = 0;

        for e in self {
            len += e.encoded_size();
        }

        len
    }
}

#[cfg(test)]
mod tests {
    use crate::{codec::*, error::Error};

    #[test]
    fn test_codec_encode_decode() -> crate::Result<()> {
        let value: u16 = 325;
        let mut encoded = BytesMut::new();

        VariableByteInteger(value as u32).encode(&mut encoded);
        assert_eq!(encoded, Bytes::from(vec![0xc5, 0x02]));

        let decoded = VariableByteInteger::decode(&mut encoded)?;
        assert_eq!(decoded.0 as u16, value);

        Ok(())
    }

    #[test]
    fn test_decoder_malformed_integer() {
        let mut encoded = Bytes::from(vec![0xc5, 0xc5, 0xc5, 0xc5, 0x02]);

        match VariableByteInteger::decode(&mut encoded) {
            Err(Error::MQTTReasonCode(e)) => assert_eq!(e, ReasonCode::MalformedPacket),
            _ => unreachable!(),
        };
    }

    #[test]
    fn test_oversized_integer_size_matches_what_encode_writes() {
        let mut encoded = BytesMut::new();
        VariableByteInteger(u32::MAX).encode(&mut encoded);
        assert_eq!(VariableByteInteger(u32::MAX).encoded_size(), encoded.len());
    }

    #[test]
    #[should_panic(expected = "at most 65,535 bytes")]
    fn test_overlong_string_is_never_truncated() {
        "a".repeat(65_536).encode(&mut BytesMut::new());
    }
}
