use bytes::BytesMut;
use mercurio_core::{codec::Encoder, protocol::ProtocolVersion};
use mercurio_packets::ControlPacket;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

use crate::error::{ClientError, Result};

/// A TCP connection to an MQTT broker with packet framing.
pub struct Connection {
    stream: TcpStream,
    read_buffer: BytesMut,
    protocol_version: ProtocolVersion,
}

impl Connection {
    /// Create a new connection from a TCP stream.
    /// Defaults to MQTT 5.0 protocol version.
    pub fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            read_buffer: BytesMut::with_capacity(4096),
            protocol_version: ProtocolVersion::V5,
        }
    }

    /// Set the protocol version for version-aware packet parsing.
    pub fn set_protocol_version(&mut self, version: ProtocolVersion) {
        self.protocol_version = version;
    }

    /// Write a control packet to the connection.
    pub async fn write_packet(&mut self, packet: ControlPacket) -> Result<()> {
        let mut buffer = BytesMut::new();
        packet.encode(&mut buffer);
        self.stream.write_all(&buffer).await?;
        self.stream.flush().await?;
        Ok(())
    }

    /// Read a control packet from the connection.
    /// Returns None if the connection was closed.
    /// Uses version-aware parsing based on the configured protocol version.
    pub async fn read_packet(&mut self) -> Result<Option<ControlPacket>> {
        loop {
            // Try to decode a packet from the buffer
            if !self.read_buffer.is_empty() {
                match ControlPacket::parse_with_version(
                    &mut self.read_buffer.clone(),
                    self.protocol_version,
                ) {
                    Ok(packet) => {
                        // Calculate how many bytes were consumed
                        let consumed = self.calculate_packet_size(&packet);
                        let _ = self.read_buffer.split_to(consumed);
                        return Ok(Some(packet));
                    }
                    Err(mercurio_core::error::Error::PacketIncomplete) => {
                        // Need more data
                    }
                    Err(e) => return Err(ClientError::Packet(e)),
                }
            }

            // Read more data from the socket
            let mut temp_buf = [0u8; 4096];
            let n = self.stream.read(&mut temp_buf).await?;
            if n == 0 {
                // Connection closed
                return Ok(None);
            }
            self.read_buffer.extend_from_slice(&temp_buf[..n]);
        }
    }

    /// Calculate the size of an encoded packet.
    fn calculate_packet_size(&self, packet: &ControlPacket) -> usize {
        let mut buffer = BytesMut::new();
        packet.encode(&mut buffer);
        buffer.len()
    }
}
