use bytes::BytesMut;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufWriter},
    net::TcpStream,
};

use mercurio_core::{codec::Encoder, error::Error, reason::ReasonCode, Result};
use mercurio_packets::ControlPacket;

/// Default capacity for read buffer (8KB)
const READ_BUFFER_CAPACITY: usize = 8192;

/// Default capacity for write buffer (512 bytes - typical packet size)
const WRITE_BUFFER_CAPACITY: usize = 512;

pub struct Connection {
    stream: BufWriter<TcpStream>,
    /// Reusable read buffer - avoids allocation per read
    read_buffer: BytesMut,
    /// Reusable write buffer - avoids allocation per write
    write_buffer: BytesMut,
}

impl Connection {
    pub fn new(socket: TcpStream) -> Connection {
        Connection {
            stream: BufWriter::new(socket),
            read_buffer: BytesMut::with_capacity(READ_BUFFER_CAPACITY),
            write_buffer: BytesMut::with_capacity(WRITE_BUFFER_CAPACITY),
        }
    }

    pub async fn read_packet(&mut self) -> Result<Option<ControlPacket>> {
        loop {
            if let Some(e) = self.parse_packet()? {
                return Ok(Some(e));
            }

            if 0 == self.stream.read_buf(&mut self.read_buffer).await? {
                if self.read_buffer.is_empty() {
                    return Ok(None);
                } else {
                    return Err(ReasonCode::NormalDisconnection.into());
                }
            }
        }
    }

    pub async fn write_packet(&mut self, packet: ControlPacket) -> Result<()> {
        // Clear and reuse the write buffer instead of allocating a new one
        self.write_buffer.clear();

        packet.encode(&mut self.write_buffer);

        self.stream.write_all(&self.write_buffer).await?;
        self.stream.flush().await?;

        Ok(())
    }

    fn parse_packet(&mut self) -> Result<Option<ControlPacket>> {
        match ControlPacket::check(&mut self.read_buffer) {
            Ok(_) => {
                let packet = ControlPacket::parse(&mut self.read_buffer)?;
                Ok(Some(packet))
            }
            // Not enough bytes in the buffer to parse a packet
            Err(Error::PacketIncomplete) => Ok(None),
            // An actual error
            Err(e) => Err(e),
        }
    }
}
