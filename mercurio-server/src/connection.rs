use std::{sync::Arc, time::Duration};

use bytes::BytesMut;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    time,
};

use mercurio_core::{
    codec::MAX_PACKET_SIZE, error::Error, protocol::ProtocolVersion, reason::ReasonCode,
    topic::parse_shared_subscription, Result,
};
use mercurio_packets::ControlPacket;
use tracing::warn;

use crate::sys::{ClientCounters, Stats};

/// Initial capacity of the read buffer, which grows for larger packets.
/// Kept small, as most connections are idle or exchange small packets, and
/// every connection holds one.
const READ_BUFFER_CAPACITY: usize = 1024;

/// Initial capacity of the buffer packets are encoded into before writing.
const WRITE_BUFFER_CAPACITY: usize = 256;

/// A buffer grown past this for a large packet is released once empty,
/// rather than held for the rest of the connection.
const RELEASE_ABOVE: usize = 64 * 1024;

/// Largest packet accepted from a client unless configured otherwise, fixed
/// header included. 1 MiB, the same default as EMQX.
pub const DEFAULT_MAX_PACKET_SIZE: usize = 1024 * 1024;

/// Largest CONNECT accepted unless configured otherwise. It is read before
/// the client is authenticated, so it gets less room than other packets;
/// 64 KiB still fits the will, credentials and properties of ordinary
/// clients.
pub const DEFAULT_MAX_CONNECT_PACKET_SIZE: usize = 64 * 1024;

/// How long a closing connection waits for the client to close its side.
const LINGER: Duration = Duration::from_secs(1);

/// How long writing one packet may take unless configured otherwise.
pub(crate) const DEFAULT_WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long an accepted TCP connection may be silent before the kernel
/// probes whether the client is still there, which also keeps NAT and
/// firewall state alive for clients with a long MQTT keep-alive or none.
const KEEPALIVE_IDLE: Duration = Duration::from_secs(60);
#[cfg(any(target_os = "linux", target_os = "android"))]
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
#[cfg(any(target_os = "linux", target_os = "android"))]
const KEEPALIVE_PROBES: u32 = 5;

/// Sets up an accepted TCP socket: packets go out at once rather than
/// waiting for Nagle's algorithm to batch them, and a client that vanished
/// is noticed by the kernel within about two minutes, whether the
/// connection is idle (keepalive) or has data the client never acknowledges
/// (`TCP_USER_TIMEOUT`, on Linux, matching the keepalive probes' time).
/// A socket that refuses an option is served without it.
pub(crate) fn configure_tcp(socket: &TcpStream) {
    let socket = socket2::SockRef::from(socket);
    let _ = socket.set_tcp_nodelay(true);
    let keepalive = socket2::TcpKeepalive::new().with_time(KEEPALIVE_IDLE);
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let keepalive = keepalive
        .with_interval(KEEPALIVE_INTERVAL)
        .with_retries(KEEPALIVE_PROBES);
    let _ = socket.set_tcp_keepalive(&keepalive);
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let _ =
        socket.set_tcp_user_timeout(Some(KEEPALIVE_IDLE + KEEPALIVE_INTERVAL * KEEPALIVE_PROBES));
}

/// A connection that can be either plain TCP or TLS-encrypted.
pub struct Connection<S = TcpStream>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    stream: S,
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
    /// Where this client's traffic is counted, once it has a session
    counters: Option<Arc<ClientCounters>>,
    write_timeout: Duration,
    /// A write timed out partway through a packet, so nothing more can be
    /// framed on the stream
    stalled: bool,
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

fn stalled() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "the client stopped taking what is sent to it",
    )
}

impl<S> Connection<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    /// Speaks MQTT over `stream`: plain TCP, TLS or a WebSocket.
    pub fn new(stream: S) -> Connection<S> {
        Connection {
            stream,
            read_buffer: BytesMut::with_capacity(READ_BUFFER_CAPACITY),
            write_buffer: BytesMut::with_capacity(WRITE_BUFFER_CAPACITY),
            protocol_version: ProtocolVersion::V5,
            max_packet_size: DEFAULT_MAX_PACKET_SIZE,
            connect_version: None,
            mount_point: None,
            stats: None,
            counters: None,
            write_timeout: DEFAULT_WRITE_TIMEOUT,
            stalled: false,
        }
    }

    /// How long writing one packet may take before the connection is given
    /// up as stalled.
    pub fn set_write_timeout(&mut self, write_timeout: Duration) {
        self.write_timeout = write_timeout;
    }

    pub(crate) fn set_stats(&mut self, stats: Arc<Stats>) {
        self.stats = Some(stats);
    }

    pub(crate) fn set_counters(&mut self, counters: Arc<ClientCounters>) {
        self.counters = Some(counters);
    }

    pub fn set_mount_point(&mut self, mount_point: Option<String>) {
        self.mount_point = mount_point;
    }

    pub(crate) fn mount_point(&self) -> Option<&str> {
        self.mount_point.as_deref()
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
                if let (Some(counters), ControlPacket::Publish(_)) = (&self.counters, &packet) {
                    Stats::add(&counters.messages_in);
                }
                if let Some(mount_point) = &self.mount_point {
                    mount(&mut packet, mount_point);
                }
                if self.read_buffer.is_empty() && self.read_buffer.capacity() > RELEASE_ABOVE {
                    self.read_buffer = BytesMut::with_capacity(READ_BUFFER_CAPACITY);
                }
                return Ok(Some(packet));
            }

            let read = self.stream.read_buf(&mut self.read_buffer).await?;
            if let Some(stats) = &self.stats {
                Stats::add_n(&stats.bytes_received, read as u64);
            }
            if let Some(counters) = &self.counters {
                Stats::add_n(&counters.bytes_in, read as u64);
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

    /// Sends `packet`. It is encoded before the returned future is first
    /// polled, so the future holds only the bytes to write, not the packet:
    /// awaited in a connection's loop, a large future would take room in
    /// every connection's state.
    pub fn write_packet(
        &mut self,
        mut packet: ControlPacket,
    ) -> impl std::future::Future<Output = Result<()>> + '_ {
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
        // Only a forwarded PUBLISH can grow this large, by the properties
        // added to a message that already filled a whole packet
        if self.write_buffer.len() > MAX_PACKET_SIZE {
            warn!("Dropping a packet too large for MQTT to frame");
            self.write_buffer.clear();
        }
        if let (Some(counters), ControlPacket::Publish(_)) = (&self.counters, &packet) {
            if !self.write_buffer.is_empty() {
                Stats::add(&counters.messages_out);
            }
        }
        self.send_encoded()
    }

    async fn send_encoded(&mut self) -> Result<()> {
        if self.stalled {
            return Err(stalled().into());
        }
        // Each packet is encoded whole first, so it goes out in one write
        let (stream, buffer) = (&mut self.stream, &self.write_buffer);
        let write = async {
            stream.write_all(buffer).await?;
            stream.flush().await
        };
        match time::timeout(self.write_timeout, write).await {
            Ok(written) => written?,
            Err(_) => {
                self.stalled = true;
                return Err(stalled().into());
            }
        }
        if let Some(stats) = &self.stats {
            Stats::add_n(&stats.bytes_sent, self.write_buffer.len() as u64);
        }
        if let Some(counters) = &self.counters {
            Stats::add_n(&counters.bytes_out, self.write_buffer.len() as u64);
        }
        if self.write_buffer.capacity() > RELEASE_ABOVE {
            self.write_buffer = BytesMut::with_capacity(WRITE_BUFFER_CAPACITY);
        }

        Ok(())
    }

    /// Closes the connection without losing what was just sent. Closing a
    /// socket while the client is still sending makes the kernel answer with
    /// a reset, and a reset can make the client drop a DISCONNECT or CONNACK
    /// it hasn't read yet. So writing stops first, then whatever the client
    /// still sends is discarded until it closes too, for at most a second.
    pub async fn close(&mut self) {
        // Shutting TLS or WebSocket down writes to the client, which a
        // stalled one wouldn't take
        if !self.stalled {
            let _ = time::timeout(self.write_timeout, self.stream.shutdown()).await;
        }
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

        match ControlPacket::check(&self.read_buffer) {
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use socket2::SockRef;
    use tokio::net::{TcpListener, TcpStream};

    #[tokio::test]
    async fn test_accepted_sockets_send_at_once_and_notice_dead_peers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let _client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (socket, _) = listener.accept().await.unwrap();
        super::configure_tcp(&socket);

        let socket = SockRef::from(&socket);
        assert!(socket.tcp_nodelay().unwrap());
        assert!(socket.keepalive().unwrap());
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            assert_eq!(socket.tcp_keepalive_time().unwrap(), super::KEEPALIVE_IDLE);
            assert_eq!(
                socket.tcp_keepalive_interval().unwrap(),
                super::KEEPALIVE_INTERVAL
            );
            assert_eq!(
                socket.tcp_keepalive_retries().unwrap(),
                super::KEEPALIVE_PROBES
            );
            assert_eq!(
                socket.tcp_user_timeout().unwrap(),
                Some(Duration::from_secs(110))
            );
        }
    }
}
