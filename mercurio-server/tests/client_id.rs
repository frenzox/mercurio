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

/// Sends a CONNECT with an empty client id and no clean start, returning the
/// CONNACK variable header.
async fn connect_without_id(addr: SocketAddr, level: u8) -> Vec<u8> {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let mut connect = vec![0x10, 0x00, 0x00, 0x04, b'M', b'Q', b'T', b'T', level, 0x00];
    connect.extend_from_slice(&[0x00, 0x3c]);
    if level == 5 {
        connect.push(0x00);
    }
    connect.extend_from_slice(&[0x00, 0x00]);
    connect[1] = (connect.len() - 2) as u8;
    stream.write_all(&connect).await.unwrap();

    let mut header = [0u8; 2];
    let mut body = Vec::new();
    timeout(Duration::from_secs(2), async {
        stream.read_exact(&mut header).await.unwrap();
        body.resize(header[1] as usize, 0);
        stream.read_exact(&mut body).await.unwrap();
    })
    .await
    .expect("no CONNACK");
    assert_eq!(header[0], 0x20);
    body
}

#[tokio::test]
async fn clients_without_id_get_separate_sessions() {
    let addr = start_broker().await;

    let first = connect_without_id(addr, 5).await;
    let second = connect_without_id(addr, 5).await;

    // Accepted, without resuming another client's session
    assert_eq!(&first[..2], &[0x00, 0x00]);
    assert_eq!(&second[..2], &[0x00, 0x00]);
    // Each one is told a different assigned client identifier
    assert_ne!(first, second);
}

#[tokio::test]
async fn mqtt311_rejects_empty_id_without_clean_session() {
    let addr = start_broker().await;

    // Return code 0x02: identifier rejected
    assert_eq!(connect_without_id(addr, 4).await, [0x00, 0x02]);
}
