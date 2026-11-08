mod common;

use mercurio_server::server::ServerConfig;
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
};

/// MQTT 5 CONNECT with a will on `topic`; returns the CONNACK reason code.
async fn connect_with_will(addr: std::net::SocketAddr, topic: &str) -> u8 {
    let mut body = vec![
        0x00, 0x04, b'M', b'Q', b'T', b'T', 0x05, 0x06, 0x00, 0x3c, 0x00,
    ];
    // Client id "w", no will properties, the will topic, payload "x"
    body.extend([0x00, 0x01, b'w', 0x00]);
    body.extend((topic.len() as u16).to_be_bytes());
    body.extend(topic.as_bytes());
    body.extend([0x00, 0x01, b'x']);
    let mut connect = vec![0x10, body.len() as u8];
    connect.extend(body);

    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(&connect).await.unwrap();
    common::read_connack(&mut stream).await[3]
}

/// A will is published like a PUBLISH, so its topic must be valid for one,
/// and not one of the broker's own `$` topics.
#[tokio::test]
async fn wills_need_a_publishable_topic() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(mercurio_server::server::run_with_config(
        listener,
        ServerConfig::default(),
        std::future::pending::<()>(),
    ));

    // Topic Name invalid
    assert_eq!(connect_with_will(addr, "$SYS/broker/uptime").await, 0x90);
    assert_eq!(connect_with_will(addr, "a/#").await, 0x90);
    assert_eq!(connect_with_will(addr, "status/w").await, 0x00);
}
