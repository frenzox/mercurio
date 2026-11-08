mod common;

use std::time::Duration;

use mercurio_server::server::ServerConfig;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::{sleep, timeout},
};

/// QoS 2 PUBLISH on "t" with the given packet id.
fn qos2_publish(id: u8) -> [u8; 9] {
    [0x34, 0x07, 0x00, 0x01, b't', 0x00, id, 0x00, b'x']
}

#[tokio::test]
async fn disconnect_survives_a_client_still_sending() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut config = ServerConfig::default();
    config.limits.receive_maximum = 1;
    tokio::spawn(mercurio_server::server::run_with_config(
        listener,
        config,
        std::future::pending::<()>(),
    ));

    let mut client = TcpStream::connect(addr).await.unwrap();
    client
        .write_all(&[
            0x10, 0x0e, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x05, 0x02, 0x00, 0x3c, 0x00, 0x00,
            0x01, b'c',
        ])
        .await
        .unwrap();
    common::read_connack(&mut client).await;

    // The second message exceeds Receive Maximum, so the broker disconnects
    client.write_all(&qos2_publish(1)).await.unwrap();
    client.write_all(&qos2_publish(2)).await.unwrap();

    // Like a client busy with the PUBREC, which answers it before reading
    // further, after the broker has already closed its side
    for _ in 0..2 {
        sleep(Duration::from_millis(100)).await;
        client
            .write_all(&[0x62, 0x02, 0x00, 0x01])
            .await
            .expect("broker reset the connection");
    }

    let mut received = Vec::new();
    timeout(Duration::from_secs(3), client.read_to_end(&mut received))
        .await
        .unwrap()
        .expect("DISCONNECT lost to a reset");
    // PUBREC for the first message, then the DISCONNECT
    assert_eq!(received[0], 0x50, "{received:02x?}");
    let disconnect = &received[2 + received[1] as usize..];
    assert_eq!(
        common::disconnect_reason(disconnect),
        0x93,
        "{received:02x?}"
    );
}
