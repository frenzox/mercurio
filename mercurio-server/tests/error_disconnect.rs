mod common;

use std::{net::SocketAddr, time::Duration};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

async fn start_broker() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(mercurio_server::server::run(
        listener,
        std::future::pending::<()>(),
    ));
    addr
}

async fn connect_v5(addr: SocketAddr) -> TcpStream {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(&[
            0x10, 0x0e, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x05, 0x02, 0x00, 0x3c, 0x00, 0x00,
            0x01, b'c',
        ])
        .await
        .unwrap();
    common::read_connack(&mut stream).await;
    stream
}

/// Reads everything until the broker closes the connection.
async fn read_until_closed(stream: &mut TcpStream) -> Vec<u8> {
    let mut received = Vec::new();
    timeout(Duration::from_secs(2), stream.read_to_end(&mut received))
        .await
        .expect("connection still open")
        .unwrap();
    received
}

#[tokio::test]
async fn protocol_violation_is_explained_before_closing() {
    let addr = start_broker().await;
    let mut stream = connect_v5(addr).await;

    // PUBLISH to "a/+": wildcards are not allowed in topic names
    stream
        .write_all(&[0x30, 0x06, 0x00, 0x03, b'a', b'/', b'+', 0x00])
        .await
        .unwrap();

    // DISCONNECT, Topic Name Invalid, explained in the reason string
    let disconnect = read_until_closed(&mut stream).await;
    assert_eq!(common::disconnect_reason(&disconnect), 0x90);
    let mut buffer = bytes::BytesMut::from(&disconnect[..]);
    let Ok(mercurio_packets::ControlPacket::Disconnect(packet)) =
        mercurio_packets::ControlPacket::parse_with_version(
            &mut buffer,
            mercurio_core::protocol::ProtocolVersion::V5,
        )
    else {
        panic!("Expected DISCONNECT");
    };
    let reason_string = packet.properties.unwrap().reason_string.unwrap();
    assert_eq!(reason_string.value, "Topic name invalid");
}

#[tokio::test]
async fn oversized_packet_is_explained_before_closing() {
    let addr = start_broker().await;
    let mut stream = connect_v5(addr).await;

    stream
        .write_all(&[0x30, 0x80, 0x80, 0x80, 0x01])
        .await
        .unwrap();

    // DISCONNECT, Packet Too Large
    assert_eq!(
        common::disconnect_reason(&read_until_closed(&mut stream).await),
        0x95
    );
}

#[tokio::test]
async fn unsupported_protocol_version_is_refused() {
    let addr = start_broker().await;
    let mut stream = TcpStream::connect(addr).await.unwrap();

    // CONNECT with protocol level 6
    stream
        .write_all(&[
            0x10, 0x0d, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x06, 0x02, 0x00, 0x3c, 0x00, 0x01,
            b'c',
        ])
        .await
        .unwrap();

    // CONNACK, unacceptable protocol version
    assert_eq!(
        read_until_closed(&mut stream).await,
        [0x20, 0x02, 0x00, 0x01]
    );
}

#[tokio::test]
async fn keep_alive_timeout_is_explained_before_closing() {
    let addr = start_broker().await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    // CONNECT with a keepalive of 1 second, then silence
    stream
        .write_all(&[
            0x10, 0x0e, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x05, 0x02, 0x00, 0x01, 0x00, 0x00,
            0x01, b'k',
        ])
        .await
        .unwrap();
    common::read_connack(&mut stream).await;

    // DISCONNECT, Keep Alive timeout
    assert_eq!(
        common::disconnect_reason(&read_until_closed(&mut stream).await),
        0x8d
    );
}

#[tokio::test]
async fn shutdown_is_announced_to_connected_clients() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(mercurio_server::server::run(listener, async {
        let _ = stopped.await;
    }));

    let mut stream = connect_v5(addr).await;
    stop.send(()).unwrap();

    // DISCONNECT, Server shutting down
    assert_eq!(
        common::disconnect_reason(&read_until_closed(&mut stream).await),
        0x8b
    );
    timeout(Duration::from_secs(2), server)
        .await
        .expect("server did not finish shutting down")
        .unwrap()
        .unwrap();
}
