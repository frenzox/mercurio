use std::time::Duration;

use bytes::BytesMut;
use mercurio_core::codec::MAX_PACKET_SIZE;
use mercurio_core::protocol::ProtocolVersion;
use mercurio_core::reason::ReasonCode;
use mercurio_packets::ControlPacket;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::timeout;

use crate::error::{ClientError, Result};
use crate::options::ConnectOptions;

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
    max_incoming_size: u32,
    /// Longest a packet may take to write before the connection counts
    /// as lost
    write_timeout: Duration,
    /// A write failed or timed out, maybe halfway through a packet, so
    /// nothing more can go over this connection
    broken: bool,
}

impl Connection {
    /// A connection still to be set up, writes bounded by the connect
    /// timeout until CONNACK settles the keep-alive.
    pub fn new(stream: impl Stream + 'static, options: &ConnectOptions) -> Self {
        Self {
            stream: Box::new(stream),
            read_buffer: BytesMut::with_capacity(4096),
            protocol_version: options.protocol_version,
            max_packet_size: None,
            max_incoming_size: options.incoming_packet_limit(),
            write_timeout: options.connect_timeout,
            broken: false,
        }
    }

    pub fn set_write_timeout(&mut self, write_timeout: Duration) {
        self.write_timeout = write_timeout;
    }

    /// Whether a failed write ended the connection.
    pub fn is_broken(&self) -> bool {
        self.broken
    }

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
        if self.broken {
            return Err(ClientError::Disconnected);
        }
        let stream = &mut self.stream;
        let written = timeout(self.write_timeout, async {
            stream.write_all(&buffer).await?;
            stream.flush().await
        })
        .await;
        match written {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => {
                self.broken = true;
                Err(e.into())
            }
            Err(_) => {
                self.broken = true;
                Err(ClientError::Timeout)
            }
        }
    }

    /// Read a control packet from the connection.
    /// Returns None if the connection was closed.
    /// Uses version-aware parsing based on the configured protocol version.
    pub async fn read_packet(&mut self) -> Result<Option<ControlPacket>> {
        loop {
            // Refused from the fixed header, before buffering the body
            match ControlPacket::frame_len(&self.read_buffer) {
                Ok(len) if len > self.max_incoming_size as usize => {
                    return Err(ClientError::BrokerViolation(ReasonCode::PacketTooLarge));
                }
                Ok(len) if self.read_buffer.len() >= len => {
                    let packet = ControlPacket::parse_with_version(
                        &mut self.read_buffer,
                        self.protocol_version,
                    )
                    .map_err(ClientError::Packet)?;
                    return Ok(Some(packet));
                }
                Ok(_) | Err(mercurio_core::error::Error::PacketIncomplete) => {}
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
