mod common;

use std::{net::SocketAddr, time::Duration};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

async fn read(stream: &mut TcpStream, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    timeout(Duration::from_secs(2), stream.read_exact(&mut buf))
        .await
        .expect("timed out waiting for broker")
        .unwrap();
    buf
}

/// MQTT 5 CONNECT with client id "dup"; returns the CONNACK flags byte.
async fn connect(addr: SocketAddr, clean_start: bool) -> (TcpStream, u8) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let flags = if clean_start { 0x02 } else { 0x00 };
    let connect = [
        0x10, 0x10, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x05, flags, 0x00, 0x3c, 0x00, 0x00, 0x03,
        b'd', b'u', b'p',
    ];
    stream.write_all(&connect).await.unwrap();
    let connack = common::read_connack(&mut stream).await;
    assert_eq!(connack[3], 0x00, "connection refused");
    (stream, connack[2])
}

#[tokio::test]
async fn new_connection_takes_over_client_id() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(mercurio_server::server::run(
        listener,
        std::future::pending::<()>(),
    ));

    let (mut old, _) = connect(addr, true).await;
    // SUBSCRIBE packet id 1, "t" at QoS 0, then wait for SUBACK
    old.write_all(&[0x82, 0x07, 0x00, 0x01, 0x00, 0x00, 0x01, b't', 0x00])
        .await
        .unwrap();
    read(&mut old, 6).await;

    let (mut new, session_present) = connect(addr, false).await;
    assert_eq!(session_present, 0x01);

    // The old connection is told why, then closed
    assert_eq!(read(&mut old, 3).await, [0xe0, 0x01, 0x8e]);
    assert_eq!(old.read(&mut [0u8; 1]).await.unwrap(), 0);

    // The resumed subscription now delivers to the new connection
    let mut publisher = TcpStream::connect(addr).await.unwrap();
    publisher
        .write_all(&[
            0x10, 0x0e, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x05, 0x02, 0x00, 0x3c, 0x00, 0x00,
            0x01, b'p',
        ])
        .await
        .unwrap();
    common::read_connack(&mut publisher).await;
    publisher
        .write_all(&[0x30, 0x06, 0x00, 0x01, b't', 0x00, b'h', b'i'])
        .await
        .unwrap();
    assert_eq!(
        read(&mut new, 8).await,
        [0x30, 0x06, 0x00, 0x01, b't', 0x00, b'h', b'i']
    );
}
