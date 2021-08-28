use bytes::{Buf, BufMut, Bytes, BytesMut};
use crate::reason::ReasonCode;
use std::mem;

pub trait Encoder {
    fn encode(&self, buffer: &mut BytesMut);
    fn get_encoded_size(&self) -> usize {
        mem::size_of_val(self)
    }
}

pub trait Decoder {
    fn decode<T>(buffer: &mut T) -> Result<Option<Self>, ReasonCode>
    where
        Self: Sized,
        T: Buf;
}

pub trait DecoderWithContext<U> {
    fn decode<T>(buffer: &mut T, context: &U) -> Result<Option<Self>, ReasonCode>
    where
        Self: Sized,
        T: Buf;
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

fn decode_var_byte_integer<T: Buf>(encoded: &mut T) -> Result<VariableByteInteger, ReasonCode> {
    let mut multiplier = 1;
    let mut value: u32 = 0;

    loop {
        if encoded.has_remaining() {
            let encoded_byte = encoded.get_u8();
            value += (encoded_byte & 0b0111_1111) as u32 * multiplier;

            if multiplier > (128 * 128 * 128) {
                return Err(ReasonCode::MalformedPacket);
            }

            multiplier *= 128;

            if (encoded_byte & 0b1000_0000) == 0 {
                break;
            }
        } else {
            return Err(ReasonCode::MalformedPacket);
        }
    }

    Ok(VariableByteInteger(value))
}

#[derive(PartialEq, Debug)]
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
    fn decode<T: Buf>(buffer: &mut T) -> Result<Option<Self>, ReasonCode> {
        match decode_var_byte_integer(buffer) {
            Ok(v) => Ok(Some(v)),
            Err(e) => Err(e),
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

impl Decoder for String {
    fn decode<T: Buf>(buffer: &mut T) -> Result<Option<Self>, ReasonCode> {
        if buffer.remaining() < 2 {
            return Ok(None);
        }

        let length = buffer.get_u16();
        if buffer.remaining() < length as usize {
            return Err(ReasonCode::MalformedPacket);
        }

        let bytes = buffer.copy_to_bytes(length.into());

        match String::from_utf8(bytes.to_vec()) {
            Err(_) => Err(ReasonCode::MalformedPacket),
            Ok(s) => Ok(Some(s)),
        }
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

impl Encoder for u8 {
    fn encode(&self, buffer: &mut BytesMut) {
        buffer.put_u8(*self);
    }
}

impl Decoder for u8 {
    fn decode<T: Buf>(buffer: &mut T) -> Result<Option<Self>, ReasonCode> {
        if !buffer.has_remaining() {
            return Ok(None);
        }

        Ok(Some(buffer.get_u8()))
    }
}

impl Encoder for u16 {
    fn encode(&self, buffer: &mut BytesMut) {
        buffer.put_u16(*self);
    }
}

impl Decoder for u16 {
    fn decode<T: Buf>(buffer: &mut T) -> Result<Option<Self>, ReasonCode> {
        if buffer.remaining() < 2 {
            return Ok(None);
        }

        Ok(Some(buffer.get_u16()))
    }
}

impl Encoder for u32 {
    fn encode(&self, buffer: &mut BytesMut) {
        buffer.put_u32(*self);
    }
}

impl Decoder for u32 {
    fn decode<T: Buf>(buffer: &mut T) -> Result<Option<Self>, ReasonCode> {
        if buffer.remaining() < 4 {
            return Ok(None);
        }

        Ok(Some(buffer.get_u32()))
    }
}

impl Encoder for bool {
    fn encode(&self, buffer: &mut BytesMut) {
        buffer.put_u8(*self as u8);
    }
}

impl Decoder for bool {
    fn decode<T: Buf>(buffer: &mut T) -> Result<Option<Self>, ReasonCode> {
        if buffer.remaining() < 1 {
            return Ok(None);
        }

        Ok(Some(buffer.get_u8() != 0))
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

impl Decoder for Bytes {
    fn decode<T: Buf>(buffer: &mut T) -> Result<Option<Self>, ReasonCode> {
        if buffer.remaining() < 2 {
            return Ok(None);
        }

        let length = buffer.get_u16();
        if buffer.remaining() < length as usize {
            return Err(ReasonCode::MalformedPacket);
        }

        Ok(Some(buffer.copy_to_bytes(length.into())))
    }
}

impl<T> Encoder for Option<T>
where
    T: Encoder,
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
    use crate::endec::*;

    #[test]
    fn test_endec_encode_decode() -> Result<(), ReasonCode> {
        let value: u16 = 325;
        let mut encoded = BytesMut::new();

        VariableByteInteger(value as u32).encode(&mut encoded);
        assert_eq!(encoded, Bytes::from(vec![0xc5, 0x02]));

        let decoded = VariableByteInteger::decode(&mut encoded)?.unwrap();
        assert_eq!(decoded.0 as u16, value);

        Ok(())
    }

    #[test]
    fn test_decoder_malformed_integer() {
        let mut encoded = Bytes::from(vec![0xc5, 0xc5, 0xc5, 0xc5, 0x02]);

        match VariableByteInteger::decode(&mut encoded) {
            Ok(_) => panic!("Succesfully decoded invalid packet, should never happen"),
            Err(e) => assert_eq!(e, ReasonCode::MalformedPacket),
        };
    }
}
