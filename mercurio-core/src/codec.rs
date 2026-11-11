//! Encoding and decoding traits for MQTT packets.

use core::mem;

#[cfg(not(feature = "std"))]
use alloc::{string::String, vec::Vec};

use bytes::{Buf, BufMut, Bytes, BytesMut};

use crate::{
    error::{EncodeError, Error},
    reason::ReasonCode,
};

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
    ///
    /// Fails, instead of writing bytes the receiver would refuse, when the
    /// value can't be carried: a string or binary value over 65,535 bytes,
    /// a string holding U+0000, a Variable Byte Integer over 268,435,455, or
    /// a value the spec rules out. The packet encoders leave `buffer` as it
    /// was when they fail; a smaller value may leave part of itself behind.
    fn encode(&self, buffer: &mut BytesMut) -> Result<(), EncodeError>;

    /// Number of bytes [`encode`](Encoder::encode) writes.
    fn encoded_size(&self) -> usize;
}

/// The longest packet MQTT can frame: the fixed header byte, a four-byte
/// Remaining Length, and the 268,435,455 bytes that length can count.
pub const MAX_PACKET_SIZE: usize = 1 + 4 + VariableByteInteger::MAX as usize;

/// Writes the two-byte length of a string or binary value (section 1.5.4),
/// which can't be over 65,535.
fn put_length(len: usize, buffer: &mut BytesMut) -> Result<(), EncodeError> {
    let len = u16::try_from(len).map_err(|_| EncodeError::TooLong)?;
    buffer.put_u16(len);
    Ok(())
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
            // A last byte of zero after the first adds nothing: the value
            // fits in fewer bytes [MQTT-1.5.5-1]
            if shift > 0 && encoded_byte == 0 {
                return Err(ReasonCode::MalformedPacket.into());
            }
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
/// fourth byte still has its continuation bit set or the value uses more
/// bytes than it needs [MQTT-1.5.5-1].
/// [`new`](VariableByteInteger::new) refuses values above 268,435,455, so
/// every value encodes; packets that large would be longer than
/// [`MAX_PACKET_SIZE`].
///
/// ```
/// use bytes::BytesMut;
/// use mercurio_core::codec::{Decoder, Encoder, VariableByteInteger};
///
/// let mut buffer = BytesMut::new();
/// let value = VariableByteInteger::new(321).unwrap();
/// value.encode(&mut buffer).unwrap();
/// assert_eq!(&buffer[..], &[0xc1, 0x02]);
/// assert_eq!(VariableByteInteger::decode(&mut buffer).unwrap().value(), 321);
/// assert_eq!(VariableByteInteger::new(268_435_456), None);
/// ```
#[derive(PartialEq, Eq, Debug, Default, Clone, Copy, Hash)]
pub struct VariableByteInteger(u32);

impl VariableByteInteger {
    /// The largest value a Variable Byte Integer can carry.
    pub const MAX: u32 = 268_435_455;

    /// `value` as a Variable Byte Integer, or `None` above [`Self::MAX`].
    pub const fn new(value: u32) -> Option<Self> {
        if value <= Self::MAX {
            Some(VariableByteInteger(value))
        } else {
            None
        }
    }

    pub const fn value(self) -> u32 {
        self.0
    }

    /// A property identifier, which is always far below the maximum.
    pub(crate) const fn from_id(id: u32) -> Self {
        VariableByteInteger(id)
    }
}

impl From<VariableByteInteger> for u32 {
    fn from(value: VariableByteInteger) -> u32 {
        value.0
    }
}

impl Encoder for VariableByteInteger {
    fn encode(&self, buffer: &mut BytesMut) -> Result<(), EncodeError> {
        encode_var_byte_integer(self.0, buffer);
        Ok(())
    }

    fn encoded_size(&self) -> usize {
        match self.0 {
            0..=127 => 1,
            128..=16383 => 2,
            16384..=2097151 => 3,
            _ => 4,
        }
    }
}

impl Decoder for VariableByteInteger {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        decode_var_byte_integer(buffer)
    }
}

impl Encoder for String {
    fn encode(&self, buffer: &mut BytesMut) -> Result<(), EncodeError> {
        self.as_str().encode(buffer)
    }

    fn encoded_size(&self) -> usize {
        self.as_str().encoded_size()
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

/// Refuses a string over 65,535 bytes or holding U+0000 [MQTT-1.5.4-2].
impl Encoder for str {
    fn encode(&self, buffer: &mut BytesMut) -> Result<(), EncodeError> {
        if self.contains('\0') {
            return Err(EncodeError::NulInString);
        }
        put_length(self.len(), buffer)?;
        buffer.put(self.as_bytes());
        Ok(())
    }

    fn encoded_size(&self) -> usize {
        self.len() + mem::size_of::<u16>()
    }
}

impl Encoder for u8 {
    fn encode(&self, buffer: &mut BytesMut) -> Result<(), EncodeError> {
        buffer.put_u8(*self);
        Ok(())
    }

    fn encoded_size(&self) -> usize {
        1
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
    fn encode(&self, buffer: &mut BytesMut) -> Result<(), EncodeError> {
        buffer.put_u16(*self);
        Ok(())
    }

    fn encoded_size(&self) -> usize {
        2
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
    fn encode(&self, buffer: &mut BytesMut) -> Result<(), EncodeError> {
        buffer.put_u32(*self);
        Ok(())
    }

    fn encoded_size(&self) -> usize {
        4
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
    fn encode(&self, buffer: &mut BytesMut) -> Result<(), EncodeError> {
        buffer.put_u8(*self as u8);
        Ok(())
    }

    fn encoded_size(&self) -> usize {
        1
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

/// Refuses binary data over 65,535 bytes.
impl Encoder for Bytes {
    fn encode(&self, buffer: &mut BytesMut) -> Result<(), EncodeError> {
        put_length(self.len(), buffer)?;
        buffer.extend(self);
        Ok(())
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
    fn encode(&self, buffer: &mut BytesMut) -> Result<(), EncodeError> {
        match self {
            Some(v) => v.encode(buffer),
            None => Ok(()),
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
    fn encode(&self, buffer: &mut BytesMut) -> Result<(), EncodeError> {
        for e in self {
            e.encode(buffer)?;
        }
        Ok(())
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
    use crate::{
        codec::*,
        error::{EncodeError, Error},
    };

    #[test]
    fn test_codec_encode_decode() -> crate::Result<()> {
        let value: u16 = 325;
        let mut encoded = BytesMut::new();

        VariableByteInteger::new(value.into())
            .unwrap()
            .encode(&mut encoded)
            .unwrap();
        assert_eq!(encoded, Bytes::from(vec![0xc5, 0x02]));

        let decoded = VariableByteInteger::decode(&mut encoded)?;
        assert_eq!(decoded.value(), u32::from(value));

        Ok(())
    }

    #[test]
    fn test_decoder_malformed_integer() {
        let mut encoded = Bytes::from(vec![0xc5, 0xc5, 0xc5, 0xc5, 0x02]);

        match VariableByteInteger::decode(&mut encoded) {
            Err(Error::Reason(e)) => assert_eq!(e, ReasonCode::MalformedPacket),
            _ => unreachable!(),
        };
    }

    #[test]
    fn test_decoder_refuses_non_minimal_integers() {
        for bytes in [&[0x80, 0x00][..], &[0xff, 0x00], &[0x80, 0x80, 0x80, 0x00]] {
            assert!(
                matches!(
                    VariableByteInteger::decode(&mut Bytes::copy_from_slice(bytes)),
                    Err(Error::Reason(ReasonCode::MalformedPacket))
                ),
                "{bytes:02x?}"
            );
        }

        let mut minimal = Bytes::from_static(&[0x80, 0x01]);
        assert_eq!(
            VariableByteInteger::decode(&mut minimal).unwrap().value(),
            128
        );
        let mut zero = Bytes::from_static(&[0x00]);
        assert_eq!(VariableByteInteger::decode(&mut zero).unwrap().value(), 0);
    }

    #[test]
    fn test_oversized_integer_is_refused() {
        assert_eq!(VariableByteInteger::new(268_435_456), None);
        let largest = VariableByteInteger::new(268_435_455).unwrap();
        let mut encoded = BytesMut::new();
        largest.encode(&mut encoded).unwrap();
        assert_eq!(encoded.len(), largest.encoded_size());
        assert_eq!(encoded.len(), 4);
    }

    #[test]
    fn test_overlong_string_or_binary_is_refused() {
        let mut encoded = BytesMut::new();
        assert_eq!(
            "a".repeat(65_536).encode(&mut encoded),
            Err(EncodeError::TooLong)
        );
        assert_eq!(
            Bytes::from(vec![0; 65_536]).encode(&mut encoded),
            Err(EncodeError::TooLong)
        );
        assert!(encoded.is_empty());
        "a".repeat(65_535).encode(&mut encoded).unwrap();
        assert_eq!(encoded.len(), 65_537);
    }

    #[test]
    fn test_string_with_nul_is_refused() {
        let mut encoded = BytesMut::new();
        assert_eq!("a\0b".encode(&mut encoded), Err(EncodeError::NulInString));
        assert!(encoded.is_empty());
    }
}
