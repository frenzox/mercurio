mod common;

use std::time::Duration;

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

const PINGREQ: [u8; 2] = [0xc0, 0x00];
const PINGRESP: [u8; 2] = [0xd0, 0x00];

#[tokio::test]
async fn keepalive_zero_keeps_connection_alive() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(mercurio_server::server::run(
        listener,
        std::future::pending::<()>(),
    ));

    let mut stream = TcpStream::connect(addr).await.unwrap();

    // MQTT 5 CONNECT: clean start, keepalive 0, no properties, client id "ka0"
    let connect = [
        0x10, 0x10, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x05, 0x02, 0x00, 0x00, 0x00, 0x00, 0x03,
        b'k', b'a', b'0',
    ];
    stream.write_all(&connect).await.unwrap();

    common::read_connack(&mut stream).await;

    for _ in 0..2 {
        stream.write_all(&PINGREQ).await.unwrap();
        let mut resp = [0u8; 2];
        timeout(Duration::from_secs(2), stream.read_exact(&mut resp))
            .await
            .expect("no PINGRESP")
            .unwrap();
        assert_eq!(resp, PINGRESP);
    }
}
