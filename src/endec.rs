use bytes::{Bytes, BytesMut, BufMut, Buf};
use std::{error, fmt, io::Cursor, mem};

#[derive(Debug, PartialEq)]
pub enum EndecError {
    MalformedPacket,
}

impl error::Error for EndecError {}
impl fmt::Display for EndecError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            EndecError::MalformedPacket => write!(f, "Malformed Variable Byte Integer"),
        }
    }
}

pub trait Encoder {
    fn encode(&self, buffer: &mut BytesMut);
    fn get_encoded_size(&self) -> usize {
       mem::size_of_val(self)
    }
}

pub trait Decoder {
    type Output;

    fn decode(buffer: &Bytes) -> Result<Option<Self::Output>, EndecError>;
}

fn encode_var_byte_integer(value: u32, encoded: &mut BytesMut) {
    let mut x = value;

    loop {
        let mut encoded_byte: u8 = (x % 128) as u8;
        x = x / 128;

        if x > 0 {
            encoded_byte = encoded_byte | 0b1000_0000;
        }

        encoded.put_u8(encoded_byte);

        if x == 0 {
            break;
        }
    }
}

fn decode_var_byte_integer(encoded: &Bytes) -> Result<VariableByteInteger, EndecError> {
    let mut multiplier = 1;
    let mut value: u32 = 0;
    let mut i = encoded.iter();

    loop {
        if let Some(v) = i.next() {
            let encoded_byte = v;
            value += (encoded_byte & 0b0111_1111) as u32 * multiplier;

            if multiplier > (128 * 128 * 128) {
                return Err(EndecError::MalformedPacket);
            }

            multiplier *= 128;

            if (encoded_byte & 0b1000_0000) == 0 {
                break;
            }
        } else {
            return Err(EndecError::MalformedPacket);
        }
    }

    Ok(VariableByteInteger(value))
}

pub struct VariableByteInteger(pub u32);

impl Encoder for VariableByteInteger {
    fn encode(&self, buffer: &mut BytesMut) {
        encode_var_byte_integer(self.0, buffer);
    }

    fn get_encoded_size(&self) -> usize {
        match self.0 {
            0..=127 => 1,
            128..=16383 => 2,
            16384..=2097151 => 3,
            2097152..=268435455 => 4,
            _ => unreachable!(),
        }
    }
}

impl Decoder for VariableByteInteger {
    type Output = VariableByteInteger;

    fn decode(encoded: &Bytes) -> Result<Option<Self::Output>, EndecError> {
        match decode_var_byte_integer(encoded) {
            Ok(v) => Ok(Some(v)),
            Err(e) => Err(e)
        }
    }
}

impl Encoder for String {
    fn encode(&self, buffer: &mut BytesMut) {
        buffer.put_u16(self.len() as u16);
        buffer.put(self.as_bytes());
    }

    fn get_encoded_size(&self) -> usize {
        self.len() + mem::size_of::<u16>()
    }
}

impl Encoder for &'static str {
    fn encode(&self, buffer: &mut BytesMut) {
        buffer.put_u16(self.len() as u16);
        buffer.put(self.as_bytes());
    }

    fn get_encoded_size(&self) -> usize {
        self.len() + mem::size_of::<u16>()
    }
}

impl Decoder for String {
    type Output = String;

    fn decode(encoded: &Bytes) -> Result<Option<String>, EndecError> {
        let mut cursor = Cursor::new(encoded);

        if cursor.remaining() < 2 {
            return Ok(None);
        }

        let length = cursor.get_u16();
        if cursor.remaining() < length as usize {
            return Err(EndecError::MalformedPacket);
        }

        let position = cursor.position() as usize;

        match String::from_utf8(encoded.as_ref()[position..position + length as usize].into()) {
            Err(_) => return Err(EndecError::MalformedPacket),
            Ok(s) => Ok(Some(s))
        }
    }
}

impl Encoder for u8 {
    fn encode(&self, buffer: &mut BytesMut) {
        buffer.put_u8(*self);
    }
}

impl Decoder for u8 {
    type Output = u8;

    fn decode(encoded: &Bytes) -> Result<Option<u8>, EndecError> {
        let mut cursor = Cursor::new(encoded);
        if !cursor.has_remaining() {
            return Ok(None);
        }

        Ok(Some(cursor.get_u8()))
    }
}

impl Encoder for u16 {
    fn encode(&self, buffer: &mut BytesMut) {
        buffer.put_u16(*self);
    }
}

impl Decoder for u16 {
    type Output = u16;

    fn decode(encoded: &Bytes) -> Result<Option<u16>, EndecError> {
        let mut cursor = Cursor::new(encoded);
        if !cursor.has_remaining() {
            return Ok(None);
        }

        Ok(Some(cursor.get_u16()))
    }
}

impl Encoder for u32 {
    fn encode(&self, buffer: &mut BytesMut) {
        buffer.put_u32(*self);
    }
}

impl Decoder for u32 {
    type Output = u32;

    fn decode(encoded: &Bytes) -> Result<Option<u32>, EndecError> {
        let mut cursor = Cursor::new(encoded);
        if !cursor.has_remaining() {
            return Ok(None);
        }

        Ok(Some(cursor.get_u32()))
    }
}

impl Encoder for bool {
    fn encode(&self, buffer: &mut BytesMut) {
        buffer.put_u8(*self as u8);
    }
}

impl Encoder for Bytes {
    fn encode(&self, buffer: &mut BytesMut) {
        buffer.put_u16(self.len() as u16);
        buffer.extend(self);
    }

    fn get_encoded_size(&self) -> usize {
        mem::size_of::<u16>() + self.len()
    }
}

impl<T> Encoder for Option<T>
where
    T: Encoder
{
    fn encode(&self, buffer: &mut BytesMut) {
        match self {
            Some(v) => v.encode(buffer),
            None => {}
        }
    }

    fn get_encoded_size(&self) -> usize {
        match self {
            Some(v) => v.get_encoded_size(),
            None => 0
        }
    }
}

impl<T> Encoder for Vec<T>
where
    T: Encoder
{
    fn encode(&self, buffer: &mut BytesMut) {
        for e in self {
            e.encode(buffer);
        }
    }

    fn get_encoded_size(&self) -> usize {
        let mut len = 0;

        for e in self {
            len += e.get_encoded_size();
        }

        len
    }
}


#[cfg(test)]
mod tests {
    use crate::{endec::*};

    #[test]
    fn test_endec_encode_decode() -> Result<(), EndecError> {
        let value: u16 = 325;
        let mut encoded = BytesMut::new();

        VariableByteInteger(value as u32).encode(&mut encoded);
        let frozen = encoded.freeze();
        let decoded = VariableByteInteger::decode(&frozen)?.unwrap();

        assert_eq!(frozen, Bytes::from(vec![0xc5, 0x02]));
        assert_eq!(decoded.0 as u16, value);

        Ok(())
    }

    #[test]
    fn test_decoder_malformed_integer() {
        let mut encoded = Bytes::from(vec![0xc5, 0xc5, 0xc5, 0xc5, 0x02]);

        match VariableByteInteger::decode(&mut encoded) {
            Ok(_) => {
                assert!(false);
            },
            Err(e) => {
                assert_eq!(e, EndecError::MalformedPacket);
            }
        };
    }
}
