use bytes::BytesMut;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufWriter},
    net::TcpStream,
};

use mercurio_core::{error::Error, protocol::ProtocolVersion, reason::ReasonCode, Result};
use mercurio_packets::ControlPacket;

/// Default capacity for read buffer (8KB)
const READ_BUFFER_CAPACITY: usize = 8192;

/// Default capacity for write buffer (512 bytes - typical packet size)
const WRITE_BUFFER_CAPACITY: usize = 512;

/// Largest packet accepted from a client unless configured otherwise, fixed
/// header included. 1 MiB, the same default as EMQX.
pub const DEFAULT_MAX_PACKET_SIZE: usize = 1024 * 1024;

/// A connection that can be either plain TCP or TLS-encrypted.
pub struct Connection<S = TcpStream>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    stream: BufWriter<S>,
    /// Reusable read buffer - avoids allocation per read
    read_buffer: BytesMut,
    /// Reusable write buffer - avoids allocation per write
    write_buffer: BytesMut,
    /// Negotiated in CONNECT; decides the wire format of every other packet
    protocol_version: ProtocolVersion,
    max_packet_size: usize,
    connect_version: Option<ProtocolVersion>,
}

impl<S> Connection<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    /// Speaks MQTT over `stream`: plain TCP, TLS or a WebSocket.
    pub fn new(stream: S) -> Connection<S> {
        Connection {
            stream: BufWriter::new(stream),
            read_buffer: BytesMut::with_capacity(READ_BUFFER_CAPACITY),
            write_buffer: BytesMut::with_capacity(WRITE_BUFFER_CAPACITY),
            protocol_version: ProtocolVersion::V5,
            max_packet_size: DEFAULT_MAX_PACKET_SIZE,
            connect_version: None,
        }
    }

    pub fn set_max_packet_size(&mut self, max_packet_size: usize) {
        self.max_packet_size = max_packet_size;
    }

    pub fn set_protocol_version(&mut self, version: ProtocolVersion) {
        self.protocol_version = version;
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

        packet.encode_with_version(&mut self.write_buffer, self.protocol_version);

        self.stream.write_all(&self.write_buffer).await?;
        self.stream.flush().await?;

        Ok(())
    }

    /// The protocol version of the last CONNECT read, even if it turned out
    /// to be malformed.
    pub fn connect_version(&self) -> Option<ProtocolVersion> {
        self.connect_version
    }

    fn parse_packet(&mut self) -> Result<Option<ControlPacket>> {
        if let Some(version) = ControlPacket::connect_version(&self.read_buffer) {
            self.connect_version = Some(version);
        }

        // Refuse oversized packets from the fixed header alone, before
        // buffering them
        if ControlPacket::frame_len(&self.read_buffer).is_ok_and(|len| len > self.max_packet_size) {
            return Err(ReasonCode::PacketTooLarge.into());
        }

        match ControlPacket::check(&mut self.read_buffer) {
            Ok(_) => {
                let packet = ControlPacket::parse_with_version(
                    &mut self.read_buffer,
                    self.protocol_version,
                )?;
                Ok(Some(packet))
            }
            // Not enough bytes in the buffer to parse a packet
            Err(Error::PacketIncomplete) => Ok(None),
            // An actual error
            Err(e) => Err(e),
        }
    }
}
