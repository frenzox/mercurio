use bytes::BytesMut;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufWriter},
    net::TcpStream,
};

use crate::{codec::Encoder, control_packet::ControlPacket, error::Error, reason::ReasonCode};

pub struct Connection {
    stream: BufWriter<TcpStream>,
    buffer: BytesMut,
}

impl Connection {
    pub fn new(socket: TcpStream) -> Connection {
        Connection {
            stream: BufWriter::new(socket),
            buffer: BytesMut::with_capacity(8192),
        }
    }

    pub async fn read_packet(&mut self) -> crate::Result<Option<ControlPacket>> {
        loop {
            if let Some(e) = self.parse_packet()? {
                return Ok(Some(e));
            }

            if 0 == self.stream.read_buf(&mut self.buffer).await? {
                if self.buffer.is_empty() {
                    return Ok(None);
                } else {
                    return Err(ReasonCode::NormalDisconnection.into());
                }
            }
        }
    }

    pub async fn write_packet(&mut self, packet: ControlPacket) -> crate::Result<()> {
        let mut buf = BytesMut::new();

        packet.encode(&mut buf);

        self.stream.write_all(&buf).await?;
        self.stream.flush().await?;

        Ok(())
    }

    fn parse_packet(&mut self) -> crate::Result<Option<ControlPacket>> {
        match ControlPacket::check(&mut self.buffer) {
            Ok(_) => {
                let packet = ControlPacket::parse(&mut self.buffer)?;

                Ok(Some(packet))
            }
            // Not enough bytes in the rx_buf to parse a packet
            Err(Error::PacketIncomplete) => Ok(None),

            // An actual error
            Err(e) => Err(e),
        }
    }
}
