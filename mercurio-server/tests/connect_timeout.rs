use std::time::Duration;

use mercurio_server::server::{Limits, ServerConfig};
use tokio::{
    io::AsyncReadExt,
    net::{TcpListener, TcpStream},
    time::timeout,
};

#[tokio::test]
async fn connection_without_connect_is_closed() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = ServerConfig {
        limits: Limits {
            connect_timeout: Duration::from_millis(200),
            ..Default::default()
        },
        ..Default::default()
    };
    tokio::spawn(mercurio_server::server::run_with_config(
        listener,
        config,
        std::future::pending::<()>(),
    ));

    // Connect and never send CONNECT
    let mut stream = TcpStream::connect(addr).await.unwrap();

    let read = timeout(Duration::from_secs(2), stream.read(&mut [0u8; 1]))
        .await
        .expect("connection still open");
    assert!(matches!(read, Ok(0) | Err(_)));
}
