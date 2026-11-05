mod common;

use std::{collections::HashMap, net::SocketAddr, sync::Arc};

use mercurio_server::{
    auth::SimpleCredentialValidator,
    server::{AuthConfig, ServerConfig},
};
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
};

async fn start_broker(allow_anonymous: bool) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let credentials = HashMap::from([("user".to_string(), "secret".to_string())]);
    let config = ServerConfig {
        auth: AuthConfig {
            require_auth: true,
            allow_anonymous,
            credential_validator: Some(Arc::new(SimpleCredentialValidator::new(credentials))),
            auth_manager: None,
        },
        ..Default::default()
    };
    tokio::spawn(mercurio_server::server::run_with_config(
        listener,
        config,
        std::future::pending::<()>(),
    ));
    addr
}

/// MQTT 3.1.1 CONNECT; returns the CONNACK return code.
async fn connect(addr: SocketAddr, credentials: Option<(&str, &str)>) -> u8 {
    let mut flags = 0x02;
    let mut payload = vec![0x00, 0x01, b'c'];
    if let Some((user, password)) = credentials {
        flags |= 0xc0;
        for field in [user, password] {
            payload.extend_from_slice(&(field.len() as u16).to_be_bytes());
            payload.extend_from_slice(field.as_bytes());
        }
    }
    let mut body = vec![0x00, 0x04, b'M', b'Q', b'T', b'T', 0x04, flags, 0x00, 0x3c];
    body.extend_from_slice(&payload);
    let mut packet = vec![0x10, body.len() as u8];
    packet.extend_from_slice(&body);

    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(&packet).await.unwrap();
    common::read_connack(&mut stream).await[3]
}

#[tokio::test]
async fn anonymous_clients_follow_allow_anonymous() {
    let strict = start_broker(false).await;
    // Bad user name or password
    assert_eq!(connect(strict, None).await, 0x04);
    assert_eq!(connect(strict, Some(("user", "secret"))).await, 0x00);

    let lenient = start_broker(true).await;
    assert_eq!(connect(lenient, None).await, 0x00);
    assert_eq!(connect(lenient, Some(("user", "secret"))).await, 0x00);
    // Credentials that are sent are still checked
    assert_eq!(connect(lenient, Some(("user", "wrong"))).await, 0x04);
}
