mod common;

use std::{net::SocketAddr, time::Duration};

use mercurio_server::server::{Limits, ServerConfig};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::{sleep, timeout},
};

async fn start_broker(limits: Limits) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = ServerConfig {
        limits,
        ..Default::default()
    };
    tokio::spawn(mercurio_server::server::run_with_config(
        listener,
        config,
        std::future::pending::<()>(),
    ));
    addr
}

async fn connect(addr: SocketAddr, client_id: u8) -> (TcpStream, Vec<u8>) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(&[
            0x10, 0x0e, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x05, 0x02, 0x00, 0x3c, 0x00, 0x00,
            0x01, client_id,
        ])
        .await
        .unwrap();
    let connack = common::read_connack(&mut stream).await;
    (stream, connack)
}

async fn read_until_closed(stream: &mut TcpStream) -> Vec<u8> {
    let mut received = Vec::new();
    timeout(Duration::from_secs(2), stream.read_to_end(&mut received))
        .await
        .expect("connection still open")
        .unwrap();
    received
}

#[tokio::test]
async fn configured_packet_size_is_advertised_and_enforced() {
    let addr = start_broker(Limits {
        max_packet_size: 64,
        ..Default::default()
    })
    .await;

    let (mut stream, connack) = connect(addr, b'c').await;
    // Maximum Packet Size property (0x27) carrying 64
    assert!(connack
        .windows(5)
        .any(|w| w == [0x27, 0x00, 0x00, 0x00, 0x40]));

    // A 100 byte PUBLISH to "t"
    let mut publish = vec![0x30, 98, 0x00, 0x01, b't', 0x00];
    publish.resize(100, b'x');
    stream.write_all(&publish).await.unwrap();

    // DISCONNECT, Packet Too Large
    assert_eq!(
        common::disconnect_reason(&read_until_closed(&mut stream).await),
        0x95
    );
}

#[tokio::test]
async fn connections_beyond_the_limit_are_refused() {
    let addr = start_broker(Limits {
        max_connections: Some(1),
        ..Default::default()
    })
    .await;

    let (first, _) = connect(addr, b'a').await;

    let mut refused = TcpStream::connect(addr).await.unwrap();
    assert!(read_until_closed(&mut refused).await.is_empty());

    // Closing the first connection frees its slot
    drop(first);
    sleep(Duration::from_millis(100)).await;
    connect(addr, b'b').await;
}

#[tokio::test]
async fn keep_alive_is_capped_for_mqtt5_clients() {
    let addr = start_broker(Limits {
        max_keepalive: Some(1),
        ..Default::default()
    })
    .await;

    // MQTT 5 client asking for a one minute keep-alive
    let (mut stream, connack) = connect(addr, b'c').await;
    // Server Keep Alive property (0x13) of 1 second
    assert!(connack.windows(3).any(|w| w == [0x13, 0x00, 0x01]));
    let disconnect = read_until_closed(&mut stream).await;
    assert_eq!(common::disconnect_reason(&disconnect), 0x8d);

    // MQTT 3.1.1 can't be told, so its keep-alive is left alone
    let mut v3 = TcpStream::connect(addr).await.unwrap();
    v3.write_all(&[
        0x10, 0x0d, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x04, 0x02, 0x00, 0x00, 0x00, 0x01, b'v',
    ])
    .await
    .unwrap();
    common::read_connack(&mut v3).await;
    assert!(timeout(Duration::from_secs(2), v3.read(&mut [0u8; 1]))
        .await
        .is_err());
}
