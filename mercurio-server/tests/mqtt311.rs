use std::time::Duration;

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

async fn connect_v311(addr: std::net::SocketAddr, client_id: u8) -> TcpStream {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    // CONNECT: "MQTT" level 4, clean session, keepalive 60, one-byte client id
    let connect = [
        0x10, 0x0d, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x04, 0x02, 0x00, 0x3c, 0x00, 0x01,
        client_id,
    ];
    stream.write_all(&connect).await.unwrap();
    assert_eq!(read(&mut stream, 4).await, [0x20, 0x02, 0x00, 0x00]);
    stream
}

#[tokio::test]
async fn mqtt311_clients_exchange_messages() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(mercurio_server::server::run(
        listener,
        std::future::pending::<()>(),
    ));

    let mut subscriber = connect_v311(addr, b's').await;
    // SUBSCRIBE packet id 1, "t" at QoS 1
    subscriber
        .write_all(&[0x82, 0x06, 0x00, 0x01, 0x00, 0x01, b't', 0x01])
        .await
        .unwrap();
    assert_eq!(
        read(&mut subscriber, 5).await,
        [0x90, 0x03, 0x00, 0x01, 0x01]
    );

    let mut publisher = connect_v311(addr, b'p').await;
    // PUBLISH QoS 0 to "t" with payload "hi"
    publisher
        .write_all(&[0x30, 0x05, 0x00, 0x01, b't', b'h', b'i'])
        .await
        .unwrap();

    assert_eq!(
        read(&mut subscriber, 7).await,
        [0x30, 0x05, 0x00, 0x01, b't', b'h', b'i']
    );

    // UNSUBSCRIBE packet id 2, "t"
    subscriber
        .write_all(&[0xa2, 0x05, 0x00, 0x02, 0x00, 0x01, b't'])
        .await
        .unwrap();
    assert_eq!(read(&mut subscriber, 4).await, [0xb0, 0x02, 0x00, 0x02]);
}
