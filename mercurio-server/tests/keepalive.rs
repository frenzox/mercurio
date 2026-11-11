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

/// A connection task that runs late, here as the runtime thread it shares
/// with its clients is blocked, can find its keep-alive deadline passed
/// and a packet waiting. The packet came in time, so the client is served,
/// not disconnected. Each client is a separate draw of the order the
/// connection loop looks at what is ready.
#[tokio::test]
async fn packets_waiting_when_a_late_task_runs_count_before_the_deadline() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(mercurio_server::server::run(
        listener,
        std::future::pending::<()>(),
    ));

    let mut clients = Vec::new();
    for i in 0..16 {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        // MQTT 5 CONNECT: clean start, keep-alive 1 second, no properties
        let mut connect = [
            0x10, 0x10, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x05, 0x02, 0x00, 0x01, 0x00, 0x00,
            0x03, b'k', b'a', b'a',
        ];
        connect[17] += i;
        stream.write_all(&connect).await.unwrap();
        common::read_connack(&mut stream).await;
        clients.push(stream);
    }
    for stream in &mut clients {
        stream.write_all(&PINGREQ).await.unwrap();
    }
    // Past every deadline, 1.5 times the keep-alive, with the PINGREQs
    // waiting unread
    std::thread::sleep(Duration::from_millis(1700));

    for (i, stream) in clients.iter_mut().enumerate() {
        let mut resp = [0u8; 2];
        timeout(Duration::from_secs(2), stream.read_exact(&mut resp))
            .await
            .expect("no PINGRESP")
            .unwrap();
        assert_eq!(resp, PINGRESP, "client {i} was disconnected");
    }
}
