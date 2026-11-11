//! A scripted MQTT 5 broker that decodes what the client sends, for tests
//! that need to see packets the real broker would hide.
#![allow(dead_code)]

use std::time::Duration;

use bytes::BytesMut;
use mercurio_core::error::Error;
use mercurio_core::protocol::ProtocolVersion;
use mercurio_core::reason::ReasonCode;
use mercurio_packets::connack::{ConnAckFlags, ConnAckPacket, ConnAckProperties};
use mercurio_packets::connect::ConnectPacket;
use mercurio_packets::suback::{SubAckPacket, SubAckPayload};
use mercurio_packets::ControlPacket;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

/// One connection of the client, seen from the broker.
pub struct Peer {
    stream: TcpStream,
    buffer: BytesMut,
}

impl Peer {
    /// Accepts the next connection and returns it with its CONNECT.
    pub async fn accept(listener: &TcpListener) -> (Peer, ConnectPacket) {
        let (stream, _) = timeout(Duration::from_secs(10), listener.accept())
            .await
            .expect("no connection within 10 s")
            .unwrap();
        let mut peer = Peer {
            stream,
            buffer: BytesMut::new(),
        };
        let ControlPacket::Connect(connect) = peer.read().await else {
            panic!("expected CONNECT");
        };
        (peer, connect)
    }

    /// Accepts the next connection with a successful CONNACK carrying
    /// `properties`.
    pub async fn accept_with(
        listener: &TcpListener,
        session_present: bool,
        properties: ConnAckProperties,
    ) -> (Peer, ConnectPacket) {
        let (mut peer, connect) = Self::accept(listener).await;
        peer.connack(session_present, ReasonCode::Success, properties)
            .await;
        (peer, connect)
    }

    pub async fn connack(
        &mut self,
        session_present: bool,
        reason_code: ReasonCode,
        properties: ConnAckProperties,
    ) {
        self.write(ControlPacket::ConnAck(ConnAckPacket {
            flags: ConnAckFlags { session_present },
            reason_code,
            properties: Some(properties),
        }))
        .await;
    }

    /// The next packet, within 10 s.
    pub async fn read(&mut self) -> ControlPacket {
        self.next(Duration::from_secs(10))
            .await
            .expect("no packet within 10 s")
    }

    /// The next packet other than PINGREQ, within 10 s.
    pub async fn read_skipping_pings(&mut self) -> ControlPacket {
        loop {
            match self.read().await {
                ControlPacket::PingReq(_) => continue,
                packet => return packet,
            }
        }
    }

    /// The next packet, unless none comes within `wait` or the connection
    /// closes.
    pub async fn next(&mut self, wait: Duration) -> Option<ControlPacket> {
        timeout(wait, async {
            loop {
                match ControlPacket::parse(&mut self.buffer, ProtocolVersion::V5) {
                    Ok(packet) => return Some(packet),
                    Err(Error::PacketIncomplete) => {}
                    Err(e) => panic!("undecodable packet: {e:?}"),
                }
                let mut chunk = [0u8; 4096];
                match self.stream.read(&mut chunk).await {
                    Ok(0) | Err(_) => return None,
                    Ok(n) => self.buffer.extend_from_slice(&chunk[..n]),
                }
            }
        })
        .await
        .ok()
        .flatten()
    }

    pub async fn write(&mut self, packet: ControlPacket) {
        let mut buffer = BytesMut::new();
        packet.encode(&mut buffer, ProtocolVersion::V5);
        self.stream.write_all(&buffer).await.unwrap();
    }

    /// Answers a SUBSCRIBE with these reason codes.
    pub async fn suback(&mut self, packet_id: u16, reasons: &[ReasonCode]) {
        self.write(ControlPacket::SubAck(SubAckPacket {
            packet_id,
            properties: None,
            payload: reasons
                .iter()
                .map(|reason| SubAckPayload {
                    reason_code: *reason,
                })
                .collect(),
        }))
        .await;
    }
}
