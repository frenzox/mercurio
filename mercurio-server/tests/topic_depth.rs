mod common;

use std::{net::SocketAddr, time::Duration};

use mercurio_server::server::ServerConfig;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

async fn connect(addr: SocketAddr, client_id: u8) -> TcpStream {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(&[
            0x10, 0x0e, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x05, 0x02, 0x00, 0x3c, 0x00, 0x00,
            0x01, client_id,
        ])
        .await
        .unwrap();
    common::read_connack(&mut stream).await;
    stream
}

fn variable_byte_integer(mut value: usize) -> Vec<u8> {
    let mut encoded = Vec::new();
    loop {
        let byte = (value % 128) as u8;
        value /= 128;
        encoded.push(if value > 0 { byte | 0x80 } else { byte });
        if value == 0 {
            return encoded;
        }
    }
}

/// A filter thousands of levels deep was accepted, and removing it recursed
/// once per level until the stack overflowed and the broker aborted.
#[tokio::test]
async fn filters_too_deep_are_refused_and_the_broker_survives() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(mercurio_server::server::run_with_config(
        listener,
        ServerConfig::default(),
        std::future::pending::<()>(),
    ));

    let mut stream = connect(addr, b'a').await;
    let filter = "/".repeat(8000);
    // Packet id 1, no properties, the filter, QoS 0
    let mut body = vec![0x00, 0x01, 0x00];
    body.extend((filter.len() as u16).to_be_bytes());
    body.extend(filter.as_bytes());
    body.push(0x00);
    let mut subscribe = vec![0x82];
    subscribe.extend(variable_byte_integer(body.len()));
    subscribe.extend(body);
    stream.write_all(&subscribe).await.unwrap();

    let mut suback = [0u8; 6];
    timeout(Duration::from_secs(2), stream.read_exact(&mut suback))
        .await
        .unwrap()
        .unwrap();
    // Topic Filter invalid
    assert_eq!(suback, [0x90, 0x04, 0x00, 0x01, 0x00, 0x8f]);
    drop(stream);

    // Still serving once the connection, and its session, are gone
    tokio::time::sleep(Duration::from_millis(100)).await;
    connect(addr, b'b').await;
}
