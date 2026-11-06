use std::{sync::Arc, time::Duration};

use bytes::BytesMut;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufWriter},
    net::TcpStream,
    time,
};

use mercurio_core::{
    error::Error, protocol::ProtocolVersion, reason::ReasonCode, topic::parse_shared_subscription,
    Result,
};
use mercurio_packets::ControlPacket;

use crate::sys::Stats;

/// Default capacity for read buffer (8KB)
const READ_BUFFER_CAPACITY: usize = 8192;

/// Default capacity for write buffer (512 bytes - typical packet size)
const WRITE_BUFFER_CAPACITY: usize = 512;

/// Largest packet accepted from a client unless configured otherwise, fixed
/// header included. 1 MiB, the same default as EMQX.
pub const DEFAULT_MAX_PACKET_SIZE: usize = 1024 * 1024;

/// How long a closing connection waits for the client to close its side.
const LINGER: Duration = Duration::from_secs(1);

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
    /// Prefixed to the topics the client uses, and removed from those it is
    /// sent, so it only sees its part of the topic tree
    mount_point: Option<String>,
    /// Where the bytes read and written are counted
    stats: Option<Arc<Stats>>,
}

/// Moves the topics a client sent under `mount_point`. Empty names are left
/// alone: they refer to topic aliases, or are invalid and must stay so.
fn mount(packet: &mut ControlPacket, mount_point: &str) {
    let mounted = |topic: &mut String| {
        if !topic.is_empty() {
            topic.insert_str(0, mount_point);
        }
    };
    let mounted_filter = |filter: &mut String| {
        if !filter.starts_with("$share/") {
            return mounted(filter);
        }
        // Inside a shared subscription only the topic filter is mounted;
        // a malformed one is left for the session to refuse
        if let Some((share_name, topic_filter)) = parse_shared_subscription(filter) {
            if !topic_filter.is_empty() {
                *filter = format!("$share/{share_name}/{mount_point}{topic_filter}");
            }
        }
    };
    match packet {
        ControlPacket::Connect(connect) => {
            if let Some(topic) = &mut connect.payload.will_topic {
                mounted(topic);
            }
        }
        ControlPacket::Publish(publish) => mounted(&mut publish.topic_name),
        ControlPacket::Subscribe(subscribe) => {
            for sub in &mut subscribe.payload {
                mounted_filter(&mut sub.topic_filter);
            }
        }
        ControlPacket::Unsubscribe(unsubscribe) => {
            for unsub in &mut unsubscribe.payload {
                mounted_filter(&mut unsub.topic_filter);
            }
        }
        _ => {}
    }
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
            mount_point: None,
            stats: None,
        }
    }

    pub(crate) fn set_stats(&mut self, stats: Arc<Stats>) {
        self.stats = Some(stats);
    }

    pub fn set_mount_point(&mut self, mount_point: Option<String>) {
        self.mount_point = mount_point;
    }

    pub fn set_max_packet_size(&mut self, max_packet_size: usize) {
        self.max_packet_size = max_packet_size;
    }

    pub fn set_protocol_version(&mut self, version: ProtocolVersion) {
        self.protocol_version = version;
    }

    pub async fn read_packet(&mut self) -> Result<Option<ControlPacket>> {
        loop {
            if let Some(mut packet) = self.parse_packet()? {
                if let Some(mount_point) = &self.mount_point {
                    mount(&mut packet, mount_point);
                }
                return Ok(Some(packet));
            }

            let read = self.stream.read_buf(&mut self.read_buffer).await?;
            if let Some(stats) = &self.stats {
                Stats::add_n(&stats.bytes_received, read as u64);
            }
            if read == 0 {
                if self.read_buffer.is_empty() {
                    return Ok(None);
                } else {
                    return Err(ReasonCode::NormalDisconnection.into());
                }
            }
        }
    }

    pub async fn write_packet(&mut self, mut packet: ControlPacket) -> Result<()> {
        // Clear and reuse the write buffer instead of allocating a new one
        self.write_buffer.clear();

        if let (Some(mount_point), ControlPacket::Publish(publish)) =
            (&self.mount_point, &mut packet)
        {
            if let Some(topic) = publish.topic_name.strip_prefix(mount_point.as_str()) {
                publish.topic_name = topic.to_string();
            }
        }

        packet.encode_with_version(&mut self.write_buffer, self.protocol_version);

        self.stream.write_all(&self.write_buffer).await?;
        self.stream.flush().await?;
        if let Some(stats) = &self.stats {
            Stats::add_n(&stats.bytes_sent, self.write_buffer.len() as u64);
        }

        Ok(())
    }

    /// Closes the connection without losing what was just sent. Closing a
    /// socket while the client is still sending makes the kernel answer with
    /// a reset, and a reset can make the client drop a DISCONNECT or CONNACK
    /// it hasn't read yet. So writing stops first, then whatever the client
    /// still sends is discarded until it closes too, for at most a second.
    pub async fn close(&mut self) {
        let _ = self.stream.shutdown().await;
        let mut discard = [0u8; 4096];
        let _ = time::timeout(LINGER, async {
            while matches!(self.stream.read(&mut discard).await, Ok(n) if n > 0) {}
        })
        .await;
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
