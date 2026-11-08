use bytes::BytesMut;
use mercurio_core::codec::MAX_PACKET_SIZE;
use mercurio_core::protocol::ProtocolVersion;
use mercurio_core::reason::ReasonCode;
use mercurio_packets::ControlPacket;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::{ClientError, Result};

/// A byte stream to the broker: plain TCP or TLS.
pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> Stream for S {}

/// A connection to an MQTT broker with packet framing.
pub struct Connection {
    stream: Box<dyn Stream>,
    read_buffer: BytesMut,
    protocol_version: ProtocolVersion,
    /// The largest packet the broker accepts, once it said so
    max_packet_size: Option<u32>,
    /// The largest packet this client accepts
    max_incoming_size: Option<u32>,
}

impl Connection {
    pub fn new(
        stream: impl Stream + 'static,
        protocol_version: ProtocolVersion,
        max_incoming_size: Option<u32>,
    ) -> Self {
        Self {
            stream: Box::new(stream),
            read_buffer: BytesMut::with_capacity(4096),
            protocol_version,
            max_packet_size: None,
            max_incoming_size,
        }
    }

    /// Write a control packet to the connection.
    pub fn set_max_packet_size(&mut self, max_packet_size: Option<u32>) {
        self.max_packet_size = max_packet_size;
    }

    /// Sends a packet, unless it exceeds what the broker accepts
    /// [MQTT-3.2.2-15].
    pub async fn write_packet(&mut self, packet: ControlPacket) -> Result<()> {
        let mut buffer = BytesMut::new();
        packet.encode_with_version(&mut buffer, self.protocol_version);
        let max = self
            .max_packet_size
            .map_or(MAX_PACKET_SIZE, |max| max as usize);
        if buffer.len() > max {
            return Err(ClientError::PacketTooLarge);
        }
        self.stream.write_all(&buffer).await?;
        self.stream.flush().await?;
        Ok(())
    }

    /// Read a control packet from the connection.
    /// Returns None if the connection was closed.
    /// Uses version-aware parsing based on the configured protocol version.
    pub async fn read_packet(&mut self) -> Result<Option<ControlPacket>> {
        loop {
            match ControlPacket::check(&mut self.read_buffer) {
                Ok(()) => {
                    let buffered = self.read_buffer.len();
                    let packet = ControlPacket::parse_with_version(
                        &mut self.read_buffer,
                        self.protocol_version,
                    )
                    .map_err(ClientError::Packet)?;
                    // ponytail: checked once the packet is buffered; checking
                    // the fixed header first would cap memory too
                    let size = buffered - self.read_buffer.len();
                    if self
                        .max_incoming_size
                        .is_some_and(|max| size > max as usize)
                    {
                        return Err(ClientError::BrokerViolation(ReasonCode::PacketTooLarge));
                    }
                    return Ok(Some(packet));
                }
                Err(mercurio_core::error::Error::PacketIncomplete) => {}
                Err(e) => return Err(ClientError::Packet(e)),
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
}
